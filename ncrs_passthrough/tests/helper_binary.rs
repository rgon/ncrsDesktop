//! Runs the real `ncrs-passthrough` binary — with its capability drop,
//! NO_NEW_PRIVS and seccomp filter in force — and talks the protocol to it.
//! Needs no privileges: without CAP_SYS_ADMIN the helper still serves, it just
//! reports the missing capability and the kernel refuses the ioctl.

use std::fs::File;
use std::os::fd::AsFd;
use std::path::Path;

use ncrs_passthrough::client::{Helper, Unavailable};
use ncrs_passthrough::proto::Request;

fn exe() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_ncrs-passthrough"))
}

/// The helper exited by itself with status 0 — not killed by its seccomp
/// filter (SIGSYS) for a syscall the allowlist forgot.
fn assert_clean_exit(h: &Helper) {
    let st = h.shutdown().expect("helper did not exit when its socket closed");
    assert_eq!(st.code(), Some(0), "helper exit status {:?}", st);
}

/// One "Key:\tvalue" line of /proc/<pid>/status.
fn status_field(pid: u32, key: &str) -> String {
    let s = std::fs::read_to_string(format!("/proc/{}/status", pid)).unwrap();
    s.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(':')))
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

#[test]
fn protocol_errors_are_answered_under_the_seccomp_filter() {
    let h = Helper::start(exe()).unwrap();
    let dev_null = File::open("/dev/null").unwrap();
    // A first round trip guarantees the hardening has run.
    assert_eq!(h.call(Request::Close(5), None).unwrap(), Err(libc::ENOTCONN));
    assert_eq!(status_field(h.pid(), "Seccomp"), "2", "seccomp filter not installed");
    assert_eq!(status_field(h.pid(), "NoNewPrivs"), "1");
    assert_eq!(status_field(h.pid(), "CapInh"), "0000000000000000");
    assert_eq!(status_field(h.pid(), "CapAmb"), "0000000000000000");
    // Without the file capability (a test run) nothing at all is left.
    let eff = u64::from_str_radix(&status_field(h.pid(), "CapEff"), 16).unwrap();
    assert_eq!(eff & !(1u64 << 21), 0, "a capability other than CAP_SYS_ADMIN survived");
    // Before Setup nothing works.
    assert_eq!(h.call(Request::Close(5), None).unwrap(), Err(libc::ENOTCONN));
    assert_eq!(h.call(Request::Open, Some(dev_null.as_fd())).unwrap(), Err(libc::ENOTCONN));
    // Malformed messages are refused and the helper keeps serving.
    assert_eq!(h.call_raw(&[0u8; 3]).unwrap(), Err(libc::EINVAL));
    assert_eq!(h.call_raw(&[0u8; 16]).unwrap(), Err(libc::EPROTO));
    assert_eq!(h.call(Request::Open, None).unwrap(), Err(libc::EINVAL));
    // Only /dev/fuse is accepted as the connection.
    assert_eq!(h.call(Request::Setup, Some(dev_null.as_fd())).unwrap(), Err(libc::ENODEV));
    assert_clean_exit(&h);
}

#[test]
fn spawn_reports_a_non_fuse_descriptor() {
    let dev_null = File::open("/dev/null").unwrap();
    match Helper::spawn(exe(), dev_null.as_fd(), &[]) {
        Err(Unavailable::Refused(e)) => assert_eq!(e, libc::ENODEV),
        Err(other) => panic!("unexpected: {}", other),
        Ok(_) => panic!("a /dev/null connection was accepted"),
    }
}

#[test]
fn spawn_of_a_missing_helper_fails_cleanly() {
    let dev_null = File::open("/dev/null").unwrap();
    assert!(matches!(
        Helper::spawn(Path::new("/nonexistent/ncrs-passthrough"), dev_null.as_fd(), &[]),
        Err(Unavailable::Spawn(_))
    ));
}

/// With a real (unmounted) /dev/fuse descriptor the whole Open path runs:
/// validation (fstat, fcntl, fstatfs) and the ioctl, which the kernel refuses
/// on a connection that is not mounted. Skipped where /dev/fuse is absent.
#[test]
fn open_path_runs_under_the_filter_with_a_real_dev_fuse() {
    let Ok(fuse) = File::options().read(true).write(true).open("/dev/fuse") else {
        eprintln!("skipping: /dev/fuse not available");
        return;
    };
    let h = Helper::start(exe()).unwrap();
    let capable = h.call(Request::Setup, Some(fuse.as_fd())).unwrap().unwrap();
    assert!(capable <= 1);
    assert_eq!(h.call(Request::Setup, Some(fuse.as_fd())).unwrap(), Err(libc::EALREADY));
    assert_eq!(h.call(Request::Adopt(9), None).unwrap(), Ok(0));

    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("cached");
    std::fs::write(&p, b"cached bytes").unwrap();
    // A directory and a writable file are refused before any ioctl.
    assert_eq!(h.call(Request::Open, Some(File::open(dir.path()).unwrap().as_fd())).unwrap(), Err(libc::EINVAL));
    let rw = File::options().read(true).write(true).open(&p).unwrap();
    assert_eq!(h.call(Request::Open, Some(rw.as_fd())).unwrap(), Err(libc::EACCES));
    // A valid file reaches the ioctl; an unmounted connection refuses it
    // (EPERM: fuse_get_dev() finds no connection, or no CAP_SYS_ADMIN).
    let r = h.call(Request::Open, Some(File::open(&p).unwrap().as_fd())).unwrap();
    assert!(r.is_err(), "backing open on an unmounted /dev/fuse succeeded: {:?}", r);
    // Unknown ids are refused; the adopted one reaches the (refusing) kernel.
    assert_eq!(h.call(Request::Close(12345), None).unwrap(), Err(libc::ENOENT));
    assert!(h.call(Request::Close(9), None).unwrap().is_err());
    assert_eq!(h.call(Request::Close(9), None).unwrap(), Err(libc::ENOENT));
    assert_clean_exit(&h);
}
