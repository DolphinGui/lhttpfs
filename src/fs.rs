use std::{
    collections::HashMap,
    ffi::OsString,
    time::{Duration, UNIX_EPOCH},
};

use curl::easy::Easy;
use fuser::{FileAttr, FileType, Filesystem};
use libc::ENOENT;
use log::error;

pub struct LazyHTTPFS {
    nodes: Vec<Node>,
    // fuse3 can be multithreaded, which would make cache kinda annoying
    // fortunately fuser can't actually do multithreaded, which makes this simple for now
    cache: HashMap<String, Vec<u8>>,
}

pub enum InputFile {
    URLFile(URLFile),
    Directory(Directory),
}

pub struct URLFile {
    name: String,
    url: String,
    size: usize,
}

pub struct Directory {
    name: String,
    contents: Vec<InputFile>,
}

impl LazyHTTPFS {
    fn new(files: Vec<InputFile>, capacity: usize) -> LazyHTTPFS {
        let mut nodes = Vec::with_capacity(capacity + 1);

        let mut inode = 0;
        add_inodes(files, &mut nodes, &mut inode);
        LazyHTTPFS {
            nodes,
            cache: HashMap::new(),
        }
    }
}

fn add_inodes(files: &[InputFile], nodes: &mut Vec<Node>, inode: &mut u64) {
    let attr = FileAttr {
        ino: 0,
        size: 0,
        blocks: 0,
        atime: UNIX_EPOCH,
        mtime: UNIX_EPOCH,
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
        kind: FileType::RegularFile,
        perm: 0o444,
        nlink: 1,
        uid: 1000,
        gid: 1000,
        rdev: 0,
        blksize: 512,
        flags: 0,
    };

    for file in files {
        match file {
            InputFile::URLFile(urlfile) => nodes.push(Node::FileNode(FileNode {
                attr: FileAttr {
                    ino: *inode,
                    size: urlfile.size as u64,
                    blocks: urlfile.size as u64 / 512,
                    ..attr
                },
                url: urlfile.url.clone(),
            })),
            InputFile::Directory(dir) => {
                let ino = *inode;
                add_inodes(&dir.contents, nodes, inode);
                nodes.push(Node::DirNode(DirNode {
                    attr: FileAttr {
                        ino: *inode,
                        ..attr
                    },
                    contents: todo!(),
                }));
            }
        }
        *inode += 1;
    }
}

enum Node {
    DirNode(DirNode),
    FileNode(FileNode),
}

impl Node {
    fn get_attr(&self) -> FileAttr {
        match self {
            Node::DirNode(dir_node) => dir_node.attr,
            Node::FileNode(file_node) => file_node.attr,
        }
    }

    fn filetype(&self) -> FileType {
        match self {
            Node::DirNode(_) => FileType::Directory,
            Node::FileNode(_) => FileType::RegularFile,
        }
    }
}

struct DirNode {
    attr: FileAttr,
    contents: HashMap<OsString, u64>,
}

struct FileNode {
    attr: FileAttr,
    url: String,
}

const TTL: Duration = Duration::from_secs(1000000);

impl Filesystem for LazyHTTPFS {
    fn lookup(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        reply: fuser::ReplyEntry,
    ) {
        let parent_dir = &self.nodes[parent as usize];
        match parent_dir {
            Node::DirNode(dir_node) => {
                let f = dir_node.contents.get(name);
                if let Some(inode) = f {
                    let file = &self.nodes[*inode as usize];
                    reply.entry(&TTL, &file.get_attr(), 0)
                } else {
                    reply.error(ENOENT)
                }
            }
            Node::FileNode(file_node) => {
                error!(
                    "Inode {}, url {} was erroneously used in lookup() as a parent directory",
                    parent, file_node.url
                );
                reply.error(ENOENT);
            }
        };
    }

    fn getattr(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: Option<u64>,
        reply: fuser::ReplyAttr,
    ) {
        match self.nodes.get(ino as usize) {
            Some(file) => reply.attr(&TTL, &file.get_attr()),
            None => reply.error(ENOENT),
        }
    }

    fn readdir(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: fuser::ReplyDirectory,
    ) {
        let parent_dir = &self.nodes[ino as usize];
        match parent_dir {
            Node::DirNode(dir) => {
                dir.contents
                    .iter()
                    .filter_map(|(filename, inode)| {
                        self.nodes
                            .get(*inode as usize)
                            .map(|file| (*inode, filename, file))
                    })
                    .enumerate()
                    .skip(offset as usize)
                    .try_for_each(|(i, (inode, name, file))| -> Option<()> {
                        if reply.add(inode, (i + 1) as i64, file.filetype(), name) {
                            Some(())
                        } else {
                            None
                        }
                    });
                reply.ok();
            }
            Node::FileNode(file_node) => {
                error!(
                    "Inode {}, url {} was erroneously used in readdir() as a parent directory",
                    ino, file_node.url
                );
                reply.error(ENOENT);
            }
        };
    }

    fn read(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        flags: i32,
        lock_owner: Option<u64>,
        reply: fuser::ReplyData,
    ) {
        if let Some(file) = self.nodes.get(ino as usize).and_then(|f| {
            if let Node::FileNode(f) = f {
                Some(f)
            } else {
                None
            }
        }) {
            if let Some(data) = self.cache.get(&file.url) {
                reply.data(&data[offset as usize..]);
                return;
            }
            let mut vec = Vec::with_capacity(size as usize);
            {
                let mut curl = Easy::new();
                curl.url(&file.url).unwrap();
                let mut transaction = curl.transfer();
                transaction
                    .write_function(|data| {
                        vec.extend(data);
                        Ok(data.len())
                    })
                    .unwrap();
                transaction.perform().unwrap();
            }
            reply.data(&vec[offset as usize..]);
            self.cache.insert(file.url.clone(), vec);
        } else {
            reply.error(ENOENT);
        }
    }
}
