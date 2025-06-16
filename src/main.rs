use clap::Parser;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry,
    Request,
};
use libc::ENOENT;
use log::info;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use chrono::{DateTime, Utc};

/// Command line arguments
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// WebDAV server URL
    #[clap(short, long)]
    url: String,

    /// WebDAV username (optional)
    #[clap(short, long)]
    username: Option<String>,

    /// WebDAV password (optional)
    #[clap(short, long)]
    password: Option<String>,

    /// Local mount point
    #[clap(short, long)]
    mount_point: PathBuf,

    /// Username for logging
    #[clap(short = 'U', long, default_value = "rgon")]
    log_user: String,
}

// File attributes 
const TTL: Duration = Duration::from_secs(1); // 1 second
// const DIRECTORY_TTL: Duration = Duration::from_secs(60); // 1 minute

struct WebdavFs {
    webdav_client: Arc<Mutex<WebdavClient>>,
    inodes: Arc<Mutex<HashMap<u64, PathBuf>>>,
    paths: Arc<Mutex<HashMap<PathBuf, u64>>>,
    next_inode: Arc<Mutex<u64>>,
    log_user: String,
}

struct WebdavClient {
    url: String,
    username: Option<String>,
    password: Option<String>,
    client: reqwest::Client,
}

fn get_formatted_time() -> String {
    let now = SystemTime::now();
    let datetime: DateTime<Utc> = now.into();
    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
}

impl WebdavClient {
    fn new(url: String, username: Option<String>, password: Option<String>) -> Self {
        WebdavClient {
            url,
            username,
            password,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
        }
    }

    async fn list_directory(&self, path: &Path) -> Result<Vec<DavEntry>, Box<dyn std::error::Error>> {
        let full_url = format!("{}{}", self.url, path.display());
        
        // Create a custom PROPFIND request
        let mut req = self.client
            .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &full_url);
        
        // Add authentication if provided
        if let (Some(username), Some(password)) = (&self.username, &self.password) {
            req = req.basic_auth(username, Some(password));
        }
        
        // WebDAV PROPFIND request
        req = req.header("Depth", "1");
        req = req.header("Content-Type", "application/xml");
        req = req.body(
            r#"<?xml version="1.0" encoding="utf-8"?>
               <propfind xmlns="DAV:">
                 <prop>
                   <resourcetype/>
                   <getcontentlength/>
                   <getlastmodified/>
                   <creationdate/>
                   <displayname/>
                 </prop>
               </propfind>"#
            .to_string(),
        );

        let response = req.send().await?;
        
        if !response.status().is_success() {
            return Err(format!("Failed to list directory: {}", response.status()).into());
        }
        
        let _xml = response.text().await?;
        
        // Parse XML response to extract directory entries
        // This is simplified for the example
        // In a real implementation, you would properly parse the XML
        let entries = parse_webdav_response(path)?;
        
        Ok(entries)
    }

    async fn get_file(&self, path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        let full_url = format!("{}{}", self.url, path.display());
        let mut req = self.client.get(&full_url);
        
        // Add authentication if provided
        if let (Some(username), Some(password)) = (&self.username, &self.password) {
            req = req.basic_auth(username, Some(password));
        }
        
        let response = req.send().await?;
        
        if !response.status().is_success() {
            return Err(format!("Failed to get file: {}", response.status()).into());
        }
        
        let bytes = response.bytes().await?;
        Ok(bytes.to_vec())
    }
}

struct DavEntry {
    name: String,
    is_dir: bool,
    size: u64,
    modified: SystemTime,
}

fn parse_webdav_response(base_path: &Path) -> Result<Vec<DavEntry>, Box<dyn std::error::Error>> {
    // In a real implementation, you would use an XML parser here
    // This is just a placeholder for the example
    let mut entries = Vec::new();
    
    // Add a dummy entry for testing
    if base_path == Path::new("/") {
        entries.push(DavEntry {
            name: "example_dir".to_string(),
            is_dir: true,
            size: 0,
            modified: SystemTime::now(),
        });
        entries.push(DavEntry {
            name: "example_file.txt".to_string(),
            is_dir: false,
            size: 1024,
            modified: SystemTime::now(),
        });
    } else if base_path == Path::new("/example_dir") {
        entries.push(DavEntry {
            name: "nested_file.txt".to_string(),
            is_dir: false,
            size: 2048,
            modified: SystemTime::now(),
        });
    }
    
    Ok(entries)
}

impl WebdavFs {
    fn new(url: String, username: Option<String>, password: Option<String>, log_user: String) -> Self {
        let mut inodes = HashMap::new();
        let mut paths = HashMap::new();
        
        // Initialize root directory
        inodes.insert(1, PathBuf::from("/"));
        paths.insert(PathBuf::from("/"), 1);
        
        WebdavFs {
            webdav_client: Arc::new(Mutex::new(WebdavClient::new(url, username, password))),
            inodes: Arc::new(Mutex::new(inodes)),
            paths: Arc::new(Mutex::new(paths)),
            next_inode: Arc::new(Mutex::new(2)),
            log_user,
        }
    }
    
    fn get_inode(&self, path: &Path) -> Option<u64> {
        self.paths.lock().unwrap().get(path).copied()
    }
    
    fn get_path(&self, inode: u64) -> Option<PathBuf> {
        self.inodes.lock().unwrap().get(&inode).cloned()
    }
    
    fn allocate_inode(&self, path: PathBuf) -> u64 {
        let mut next_inode = self.next_inode.lock().unwrap();
        let mut paths = self.paths.lock().unwrap();
        let mut inodes = self.inodes.lock().unwrap();
        
        if let Some(inode) = paths.get(&path) {
            return *inode;
        }
        
        let inode = *next_inode;
        *next_inode += 1;
        
        paths.insert(path.clone(), inode);
        inodes.insert(inode, path);
        
        inode
    }
    
    fn create_file_attr(&self, inode: u64, size: u64, is_dir: bool, modified: SystemTime) -> FileAttr {
        FileAttr {
            ino: inode,
            size,
            blocks: (size + 511) / 512,
            atime: modified,
            mtime: modified,
            ctime: modified,
            crtime: modified,
            kind: if is_dir { FileType::Directory } else { FileType::RegularFile },
            perm: if is_dir { 0o755 } else { 0o644 },
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            flags: 0,
            blksize: 512,
        }
    }

    fn log_operation(&self, operation: &str, path: &Path) {
        println!("[{}] User: {} | Operation: {} | Path: {}", 
            get_formatted_time(), 
            self.log_user, 
            operation, 
            path.display()
        );
    }
}

impl Filesystem for WebdavFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let parent_path = match self.get_path(parent) {
            Some(path) => path,
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        
        let file_name = match name.to_str() {
            Some(name) => name,
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        
        let mut path = parent_path.clone();
        path.push(file_name);
        
        self.log_operation("LOOKUP", &path);
        
        // For simplicity in this example, we're using a blocking runtime
        // In a real implementation, you'd want to properly handle async
        let rt = tokio::runtime::Runtime::new().unwrap();
        
        rt.block_on(async {
            let client = self.webdav_client.lock().unwrap();
            match client.list_directory(&parent_path).await {
                Ok(entries) => {
                    for entry in entries {
                        if entry.name == file_name {
                            let inode = self.allocate_inode(path);
                            let attr = self.create_file_attr(inode, entry.size, entry.is_dir, entry.modified);
                            reply.entry(&TTL, &attr, 0);
                            return;
                        }
                    }
                    reply.error(ENOENT);
                },
                Err(_) => {
                    reply.error(ENOENT);
                }
            }
        });
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        if ino == 1 {
            // Root directory
            let attr = self.create_file_attr(1, 0, true, SystemTime::now());
            self.log_operation("GETATTR", Path::new("/"));
            reply.attr(&TTL, &attr);
            return;
        }
        
        let path = match self.get_path(ino) {
            Some(path) => path,
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        
        self.log_operation("GETATTR", &path);
        
        let rt = tokio::runtime::Runtime::new().unwrap();
        
        rt.block_on(async {
            let client = self.webdav_client.lock().unwrap();
            let parent_path = path.parent().unwrap_or(Path::new("/"));
            
            match client.list_directory(parent_path).await {
                Ok(entries) => {
                    if let Some(filename) = path.file_name() {
                        if let Some(filename_str) = filename.to_str() {
                            for entry in entries {
                                if entry.name == filename_str {
                                    let attr = self.create_file_attr(ino, entry.size, entry.is_dir, entry.modified);
                                    reply.attr(&TTL, &attr);
                                    return;
                                }
                            }
                        }
                    }
                    reply.error(ENOENT);
                },
                Err(_) => {
                    reply.error(ENOENT);
                }
            }
        });
    }

    fn read(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, size: u32, _flags: i32, _lock: Option<u64>, reply: ReplyData) {
        let path = match self.get_path(ino) {
            Some(path) => path,
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        
        self.log_operation("READ", &path);
        println!("[{}] User: {} | File Access: {} | Offset: {} | Size: {} bytes", 
            get_formatted_time(), 
            self.log_user, 
            path.display(),
            offset,
            size
        );
        
        let rt = tokio::runtime::Runtime::new().unwrap();
        
        rt.block_on(async {
            let client = self.webdav_client.lock().unwrap();
            
            match client.get_file(&path).await {
                Ok(data) => {
                    let offset = offset as usize;
                    let size = size as usize;
                    
                    if offset >= data.len() {
                        reply.data(&[]);
                    } else {
                        let end = std::cmp::min(offset + size, data.len());
                        reply.data(&data[offset..end]);
                    }
                },
                Err(_) => {
                    reply.error(ENOENT);
                }
            }
        });
    }

    fn readdir(&mut self, _req: &Request, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let path = match self.get_path(ino) {
            Some(path) => path,
            None => {
                reply.error(ENOENT);
                return;
            }
        };
        
        self.log_operation("LS", &path);
        
        // Add the standard entries
        if offset == 0 {
            let _ = reply.add(ino, 0, FileType::Directory, ".");
            
            if let Some(parent_ino) = if ino == 1 { Some(1) } else { self.get_inode(path.parent().unwrap_or(Path::new("/"))) } {
                let _ = reply.add(parent_ino, 1, FileType::Directory, "..");
            } else {
                let _ = reply.add(1, 1, FileType::Directory, "..");
            }
        }
        
        let rt = tokio::runtime::Runtime::new().unwrap();
        
        rt.block_on(async {
            let client = self.webdav_client.lock().unwrap();
            
            match client.list_directory(&path).await {
                Ok(entries) => {
                    for (i, entry) in entries.into_iter().enumerate().skip(offset as usize) {
                        let mut entry_path = path.clone();
                        entry_path.push(&entry.name);
                        
                        let entry_ino = self.allocate_inode(entry_path);
                        let file_type = if entry.is_dir { FileType::Directory } else { FileType::RegularFile };
                        
                        if reply.add(entry_ino, (i + 2 + offset as usize) as i64, file_type, entry.name) {
                            break;
                        }
                    }
                    reply.ok();
                },
                Err(_) => {
                    reply.error(ENOENT);
                }
            }
        });
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let args = Args::parse();

    let filesystem = WebdavFs::new(args.url, args.username, args.password, args.log_user.clone());
    
    let options = vec![
        MountOption::RO,
        MountOption::FSName("webdav-fs".to_string()),
        MountOption::AutoUnmount,
        MountOption::AllowOther,
    ];
    
    println!("[{}] User: {} | WebDAV FUSE mount starting at {}", 
        get_formatted_time(), 
        args.log_user.clone(), 
        args.mount_point.display()
    );
    
    fuser::mount2(filesystem, &args.mount_point, &options)?;
    
    Ok(())
}