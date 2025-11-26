use std::{error::Error, fs::File};

use clap::{Arg, ArgAction, Command};
use fs::LazyHTTPFS;
use fuser::MountOption;

mod fs;

type Result<T> = core::result::Result<T, Box<dyn Error>>;

fn main() {
    let matches = Command::new("hello")
        .version("0.0.1")
        .author("Christopher Berner")
        .arg(
            Arg::new("MOUNT_POINT")
                .required(true)
                .index(1)
                .help("Act as a client, and mount FUSE at given path"),
        )
        .arg(
            Arg::new("auto_unmount")
                .long("auto_unmount")
                .action(ArgAction::SetTrue)
                .help("Automatically unmount on process exit"),
        )
        .arg(
            Arg::new("allow-root")
                .long("allow-root")
                .action(ArgAction::SetTrue)
                .help("Allow root user to access filesystem"),
        )
        .arg(
            Arg::new("LAYOUT")
                .required(true)
                .index(2)
                .help("JSON file that contains the layout of the filesystem"),
        )
        .get_matches();
    env_logger::init();
    let mountpoint = matches.get_one::<String>("MOUNT_POINT").unwrap();
    let mut options = vec![MountOption::RO, MountOption::FSName("lhttp".to_string())];
    if matches.get_flag("auto_unmount") {
        options.push(MountOption::AutoUnmount);
    }
    if matches.get_flag("allow-root") {
        options.push(MountOption::AllowRoot);
    }

    let a: Result<_> = File::open(matches.get_one::<String>("LAYOUT").unwrap())
        .map_err(From::from)
        .and_then(|f| serde_json::from_reader(f).map_err(From::from))
        .and_then(LazyHTTPFS::new);

    match a {
        Ok(data) => {
            fuser::mount2(data, mountpoint, &options).unwrap();
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}
