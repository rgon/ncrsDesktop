// // UNUSED
// // -------------------------------------------------------------------------------
// use clap::Parser;
// use fuser::{
//     FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
//     Request,
// };
// use libc::ENOENT;
// use log::info;
// use std::collections::HashMap;
// use std::ffi::OsStr;
// use std::path::{Path, PathBuf};
// use std::sync::{Arc, Mutex};
// use std::time::{Duration, SystemTime, UNIX_EPOCH};
// use chrono::{DateTime, Utc};

// /// Command line arguments
// #[derive(Parser, Debug)]
// #[clap(author, version, about, long_about = None)]
// struct Args {
//     /// WebDAV server URL
//     #[clap(short, long)]
//     url: String,

//     /// WebDAV username (optional)
//     #[clap(short, long)]
//     username: Option<String>,

//     /// WebDAV password (optional)
//     #[clap(short, long)]
//     password: Option<String>,

//     /// Local mount point
//     #[clap(short, long)]
//     mount_point: PathBuf,

//     /// Username for logging
//     #[clap(short = 'U', long, default_value = "rgon")]
//     log_user: String,
// }

// fn main() -> Result<(), Box<dyn std::error::Error>> {
//     env_logger::init();
//     let args = Args::parse();

//     let filesystem = WebdavFs::new(args.url, args.username, args.password, args.log_user);
    
//     let options = vec![
//         MountOption::RO,
//         MountOption::FSName("webdav-fs".to_string()),
//         MountOption::AutoUnmount,
//         MountOption::AllowOther,
//     ];
    
//     println!("[{}] User: {} | WebDAV FUSE mount starting at {}", 
//         get_formatted_time(), 
//         args.log_user, 
//         args.mount_point.display()
//     );
    
//     fuser::mount2(filesystem, &args.mount_point, &options)?;
    
//     Ok(())
// }

fn main() -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}