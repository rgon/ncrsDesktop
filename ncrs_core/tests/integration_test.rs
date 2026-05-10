/// Integration tests against a real WebDAV server.
///
/// Set `WEBDAV_TEST_URL` (e.g. `http://localhost:8888`) to run these tests.
/// The docker-compose in `docker/docker-compose.yml` provides a suitable server:
///
///   docker compose -f docker/docker-compose.yml up -d
///   WEBDAV_TEST_URL=http://localhost:8888 cargo test --test integration_test
use remotefs::RemoteFs;
use remotefs_webdav::WebDAVFs;
use std::path::Path;

const TEST_USER: &str = "testuser";
const TEST_PASS: &str = "testpass";

fn test_url() -> Option<String> {
    std::env::var("WEBDAV_TEST_URL").ok()
}

fn connect() -> Option<WebDAVFs> {
    let url = test_url()?;
    let mut fs = WebDAVFs::new(TEST_USER, TEST_PASS, &url);
    fs.connect().expect("connect failed");
    Some(fs)
}

#[test]
fn test_connect_and_list_root() {
    let mut fs = match connect() {
        Some(f) => f,
        None => return,
    };

    let files = fs.list_dir(Path::new("/")).expect("list_dir / failed");
    // Root listing succeeds (may be empty on a fresh container).
    drop(files);
    fs.disconnect().ok();
}

#[test]
fn test_create_read_delete_file() {
    let mut fs = match connect() {
        Some(f) => f,
        None => return,
    };

    let path = Path::new("/ncrs_integration_test.txt");
    let content = b"hello from ncrs integration test\n";

    // Upload (remotefs-webdav uses create_file, not streaming create)
    {
        use remotefs::fs::Metadata;
        let meta = Metadata::default().size(content.len() as u64);
        fs.create_file(path, &meta, Box::new(content.as_ref()))
            .expect("create_file failed");
    }

    // Verify it appears in the directory listing.
    let files = fs.list_dir(Path::new("/")).expect("list_dir failed");
    let found = files.iter().any(|f| {
        f.path
            .file_name()
            .and_then(|n| n.to_str())
            == Some("ncrs_integration_test.txt")
    });
    assert!(found, "uploaded file not found in listing");

    // Read back via open_file (remotefs-webdav does not support streaming open())
    {
        let tmp = std::env::temp_dir().join("ncrs_test_read.bin");
        let f = std::fs::File::create(&tmp).expect("create tmp");
        fs.open_file(path, Box::new(f)).expect("open_file failed");
        let got = std::fs::read(&tmp).expect("read tmp");
        std::fs::remove_file(&tmp).ok();
        assert_eq!(got, content);
    }

    // Stat
    {
        let stat = fs.stat(path).expect("stat failed");
        assert_eq!(stat.metadata.size, content.len() as u64);
    }

    // Remove
    fs.remove_file(path).expect("remove_file failed");

    // Confirm gone
    let files_after = fs.list_dir(Path::new("/")).expect("list_dir after delete");
    let still_present = files_after.iter().any(|f| {
        f.path.file_name().and_then(|n| n.to_str())
            == Some("ncrs_integration_test.txt")
    });
    assert!(!still_present, "file still present after deletion");

    fs.disconnect().ok();
}

#[test]
fn test_create_and_list_directory() {
    let mut fs = match connect() {
        Some(f) => f,
        None => return,
    };

    let dir_path = Path::new("/ncrs_test_dir");

    fs.create_dir(dir_path, remotefs::fs::UnixPex::from(0o755))
        .expect("create_dir failed");

    let files = fs.list_dir(Path::new("/")).expect("list_dir failed");
    let found = files.iter().any(|f| {
        f.path.file_name().and_then(|n| n.to_str()) == Some("ncrs_test_dir")
    });
    assert!(found, "created directory not in listing");

    fs.remove_dir(dir_path).expect("remove_dir failed");

    fs.disconnect().ok();
}
