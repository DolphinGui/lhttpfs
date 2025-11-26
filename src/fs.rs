use std::{
    collections::HashMap,
    ffi::OsString,
    fmt::Debug,
    time::{Duration, UNIX_EPOCH},
};

use curl::easy::Easy;
use fuser::{FileAttr, FileType, Filesystem};
use libc::ENOENT;
use log::{error, trace};
use serde::Deserialize;

pub struct LazyHTTPFS {
    nodes: Vec<Node>,
    // fuse3 can be multithreaded, which would make cache kinda annoying
    // fortunately fuser can't actually do multithreaded, which makes this simple for now
    cache: HashMap<String, Vec<u8>>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum InputFile {
    URLFile(URLFile),
    Directory(Directory),
}

impl InputFile {
    fn name(&self) -> &str {
        match self {
            InputFile::URLFile(urlfile) => &urlfile.name,
            InputFile::Directory(directory) => &directory.name,
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct URLFile {
    name: String,
    url: String,
    size: usize,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Directory {
    name: String,
    contents: Vec<InputFile>,
}

impl LazyHTTPFS {
    pub fn new(files: Vec<InputFile>) -> LazyHTTPFS {
        let mut inode = 1;
        let root = InputFile::Directory(Directory {
            name: "/".into(),
            contents: files,
        });
        let (mut r, _) = add_inodes(&[root], &mut inode);
        r.sort_unstable_by_key(|f| f.get_attr().ino);
        LazyHTTPFS {
            nodes: r,
            cache: HashMap::new(),
        }
    }
}

fn add_inodes(files: &[InputFile], inode: &mut u64) -> (Vec<Node>, Vec<usize>) {
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
    let mut result = Vec::new();
    let mut toplev = Vec::new();
    for file in files {
        match file {
            InputFile::URLFile(urlfile) => {
                result.push(Node::FileNode(FileNode {
                    attr: FileAttr {
                        ino: *inode,
                        size: urlfile.size as u64,
                        blocks: urlfile.size as u64 / 512,
                        ..attr
                    },
                    url: urlfile.url.clone(),
                }));
                toplev.push(*inode as usize);
                *inode += 1;
            }
            InputFile::Directory(dir) => {
                result.push(Node::DirNode(DirNode {
                    attr: FileAttr {
                        ino: *inode,
                        kind: FileType::Directory,
                        ..attr
                    },
                    contents: HashMap::new(),
                }));
                let dir_index = result.len() - 1;
                toplev.push(*inode as usize);
                *inode += 1;
                let (results, toplev) = add_inodes(&dir.contents, inode);
                let inodes = toplev
                    .iter()
                    .zip(&dir.contents)
                    .map(|(inode, file)| (OsString::from(file.name()), *inode as u64));
                result.extend(results.into_iter());
                if let Some(Node::DirNode(n)) = result.get_mut(dir_index) {
                    n.contents = inodes.collect();
                } else {
                    panic!("Directory indexing failed!");
                }
            }
        }
    }
    (result, toplev)
}

#[derive(Debug, PartialEq, Eq)]
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

#[derive(PartialEq, Eq)]
struct DirNode {
    attr: FileAttr,
    contents: HashMap<OsString, u64>,
}

#[derive(PartialEq, Eq)]
struct FileNode {
    attr: FileAttr,
    url: String,
}

impl Debug for FileNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ino {}, url: {}, size: {}",
            self.attr.ino, self.url, self.attr.size
        )
    }
}

impl Debug for DirNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ino {}, contents {:?}", self.attr.ino, self.contents)
    }
}

const TTL: Duration = Duration::from_secs(1000000);

impl LazyHTTPFS {
    fn get_inode(&self, i: u64) -> Option<&Node> {
        if i == 0 {
            None
        } else {
            self.nodes.get(i as usize - 1)
        }
    }
}

impl Filesystem for LazyHTTPFS {
    fn lookup(
        &mut self,
        _req: &fuser::Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        reply: fuser::ReplyEntry,
    ) {
        trace!("Searching for {:?} with parent {}", name, parent);
        let Some(parent_dir) = self.get_inode(parent) else {
            reply.error(ENOENT);
            return;
        };
        match parent_dir {
            Node::DirNode(dir_node) => {
                let f = dir_node.contents.get(name);
                if let Some(file) = f.and_then(|i| self.get_inode(*i)) {
                    trace!("Reply with {:?}", file);
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
        match self.get_inode(ino) {
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
        let parent_dir = &self.get_inode(ino);
        match parent_dir {
            Some(Node::DirNode(dir)) => {
                trace!("reading directory {} at offset {}", ino, offset);
                let dots = [
                    (ino, &OsString::from("."), FileType::Directory),
                    (ino, &OsString::from(".."), FileType::Directory),
                ];
                dots.into_iter()
                    .chain(dir.contents.iter().filter_map(|(filename, inode)| {
                        self.get_inode(*inode)
                            .map(|file| (*inode, filename, file.filetype()))
                    }))
                    .enumerate()
                    .skip(offset as usize)
                    .try_for_each(|(i, (inode, name, file))| -> Option<()> {
                        if reply.add(inode, (i + 1) as i64, file, name) {
                            trace!("READDIR listing file {}: {:?}", inode, name);
                            None
                        } else {
                            Some(())
                        }
                    });
                reply.ok();
            }
            Some(Node::FileNode(file_node)) => {
                error!(
                    "Inode {}, url {} was erroneously used in readdir() as a parent directory",
                    ino, file_node.url
                );
                reply.error(ENOENT);
            }
            None => {
                reply.error(ENOENT);
            }
        };
    }

    fn read(
        &mut self,
        _req: &fuser::Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: fuser::ReplyData,
    ) {
        if let Some(Node::FileNode(file)) = self.get_inode(ino) {
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

#[cfg(test)]
mod test {

    use super::{Directory, InputFile, LazyHTTPFS, Node, URLFile};

    const JSON: &str = r#"
[
  {
    "name": "helloworld.txt",
    "size": 25,
    "url": "https://ping.archlinux.org/nm-check.txt"
  },{
    "name": "outer.dir",
  "contents": [
    {
      "name": "inner.txt",
      "size": 25,
      "url": "https://ping.archlinux.org/nm-check.txt"
    }
  ]}
]"#;

    #[test]
    fn deserialize() {
        let result: Vec<InputFile> = serde_json::from_str(JSON).unwrap();
        let expected: Vec<InputFile> = vec![
            InputFile::URLFile(URLFile {
                name: "helloworld.txt".into(),
                url: "https://ping.archlinux.org/nm-check.txt".into(),
                size: 25,
            }),
            InputFile::Directory(Directory {
                name: "outer.dir".into(),
                contents: vec![InputFile::URLFile(URLFile {
                    name: "inner.txt".into(),
                    url: "https://ping.archlinux.org/nm-check.txt".into(),
                    size: 25,
                })],
            }),
        ];
        assert_eq!(result, expected);
    }

    #[test]
    fn parsing() {
        let result: Vec<InputFile> = serde_json::from_str(JSON).unwrap();
        let fs = LazyHTTPFS::new(result);
        for (inode, node) in fs.nodes.iter().enumerate() {
            println!("{}: {:?}\n", inode + 1, node);
            assert_eq!(inode as u64 + 1, node.get_attr().ino);
        }
    }

    const JSON2: &str = include_str!("models.json");

    #[test]
    fn parsing2() {
        let result: Vec<InputFile> = serde_json::from_str(JSON2).unwrap();
        let fs = LazyHTTPFS::new(result);
        let Node::DirNode(ref root) = fs.nodes[0] else {
            panic!("Root needs to be a directory");
        };
        for (inode, node) in fs.nodes.iter().enumerate() {
            println!("{}: {:?}", inode, node);
            assert_eq!(inode as u64 + 1, node.get_attr().ino);
        }

        for (name, inode) in root.contents.iter() {
            match fs.get_inode(*inode).unwrap() {
                Node::DirNode(_) => {}
                Node::FileNode(ref f) => {
                    panic!(
                        "Expected directory for {:?} inode {}, got {:?}",
                        name, *inode, f
                    )
                }
            };
        }
    }
}
