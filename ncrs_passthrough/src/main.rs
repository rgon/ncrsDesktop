//! `ncrs-passthrough` — the only ncrs binary with a capability
//! (`cap_sys_admin=ep`, set by the package). See lib.rs for the design.
//!
//! Spawned by the `ncrs` daemon as `ncrs-passthrough <daemon-pid>` with its
//! end of a SOCK_SEQPACKET socketpair on fd 3. Before it reads a single
//! message it:
//!
//! 1. dies with its parent (PR_SET_PDEATHSIG, re-checked against the pid it
//!    was given: a secure exec clears an inherited death signal);
//! 2. refuses to run set-uid/set-gid (backing files are checked against the
//!    real uid, so the real and effective ids must agree);
//! 3. keeps only fd 3 (and stdio) open;
//! 4. drops every capability but CAP_SYS_ADMIN from the effective, permitted
//!    and inheritable sets, clears the ambient set, and drops what it can
//!    from the bounding set (that needs CAP_SETPCAP, which it is not given;
//!    NO_NEW_PRIVS and the seccomp filter below forbid exec anyway, so the
//!    bounding set never comes into play);
//! 5. becomes non-dumpable (no ptrace or core dump of a privileged process)
//!    and sets NO_NEW_PRIVS;
//! 6. installs a seccomp allowlist (anything else kills the process):
//!    recvmsg/sendto on its socket, the two backing ioctls, fstat/fstatfs/
//!    fcntl for the checks, close, writes to stderr only, non-executable
//!    memory management for the allocator, and exit. No mount, umount2,
//!    open, exec, socket or ptrace.
//!
//! Then it serves until the daemon closes its end.

use std::collections::BTreeMap;
use std::os::fd::{BorrowedFd, RawFd};

use ncrs_passthrough::caps::{self, CAP_SYS_ADMIN};
use ncrs_passthrough::client::HELPER_SOCK_FD;
use ncrs_passthrough::server::{self, DevFuse, Server, FUSE_DEV_IOC_BACKING_CLOSE, FUSE_DEV_IOC_BACKING_OPEN};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen as Len, SeccompCmpOp as Cmp, SeccompCondition as Cond, SeccompFilter,
    SeccompRule,
};

fn main() {
    let code = match run() {
        Ok(()) => 0,
        Err(msg) => {
            eprintln!("ncrs-passthrough: {}", msg);
            1
        }
    };
    // Straight to exit_group: no atexit handlers or runtime teardown, whose
    // syscalls the filter does not allow.
    // SAFETY: ends the process.
    unsafe { libc::_exit(code) }
}

fn last_err() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn run() -> Result<(), String> {
    let parent: libc::pid_t = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("usage: ncrs-passthrough <daemon-pid> (started by ncrs, with its socket on fd 3)")?;

    // SAFETY (all prctl/get*id calls below): plain syscalls on this process.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong, 0, 0, 0) } != 0 {
        return Err(format!("PR_SET_PDEATHSIG: {}", last_err()));
    }
    if unsafe { libc::getppid() } != parent {
        return Err("the daemon that started this helper is gone".into());
    }

    let uid = unsafe { libc::getuid() };
    if unsafe { libc::geteuid() } != uid || unsafe { libc::getegid() } != unsafe { libc::getgid() } {
        return Err("refusing to run set-uid or set-gid; install with a file capability instead".into());
    }

    check_socket(HELPER_SOCK_FD)?;
    close_other_fds();

    let capable = caps::effective_has(CAP_SYS_ADMIN);
    drop_privileges()?;
    install_seccomp()?;

    // SAFETY: fd 3 was checked to be our socket above and stays open until exit.
    let sock = unsafe { BorrowedFd::borrow_raw(HELPER_SOCK_FD) };
    let mut srv = Server::new(DevFuse, uid, capable);
    server::serve(sock, &mut srv).map_err(|e| format!("socket: {}", e))
}

fn check_socket(fd: RawFd) -> Result<(), String> {
    let opt = |name: libc::c_int| -> Option<libc::c_int> {
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: getsockopt writes one int.
        let rc = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, name, &mut v as *mut libc::c_int as *mut libc::c_void, &mut len) };
        (rc == 0).then_some(v)
    };
    if opt(libc::SO_DOMAIN) != Some(libc::AF_UNIX) || opt(libc::SO_TYPE) != Some(libc::SOCK_SEQPACKET) {
        return Err("fd 3 is not the AF_UNIX SOCK_SEQPACKET socket ncrs passes".into());
    }
    // SAFETY: F_SETFD on our own descriptor.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    Ok(())
}

/// Everything above fd 3 is something the daemon (or its parent) leaked.
fn close_other_fds() {
    // SAFETY: close_range only closes descriptors.
    let rc = unsafe { libc::syscall(libc::SYS_close_range, (HELPER_SOCK_FD + 1) as libc::c_uint, libc::c_uint::MAX, 0 as libc::c_uint) };
    if rc != 0 {
        // Before Linux 5.9: the usual small range is enough.
        for fd in (HELPER_SOCK_FD + 1)..1024 {
            // SAFETY: closing a descriptor we do not use.
            unsafe { libc::close(fd) };
        }
    }
}

fn drop_privileges() -> Result<(), String> {
    // SAFETY (prctl calls): they only change this process's credentials.
    unsafe {
        libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong, 0, 0, 0);
        for cap in 0..64u32 {
            if cap != CAP_SYS_ADMIN {
                // EPERM without CAP_SETPCAP, EINVAL past the kernel's last
                // capability: both expected, see the module comment.
                libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0);
            }
        }
    }
    caps::restrict_to(&[CAP_SYS_ADMIN]).map_err(|e| format!("capset: {}", e))?;
    unsafe {
        if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
            return Err(format!("PR_SET_DUMPABLE: {}", last_err()));
        }
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(format!("PR_SET_NO_NEW_PRIVS: {}", last_err()));
        }
    }
    Ok(())
}

fn rule(conds: Vec<Cond>) -> Result<SeccompRule, String> {
    SeccompRule::new(conds).map_err(|e| format!("seccomp rule: {:?}", e))
}

fn cond(arg: u8, op: Cmp, value: u64) -> Result<Cond, String> {
    Cond::new(arg, Len::Dword, op, value).map_err(|e| format!("seccomp condition: {:?}", e))
}

fn install_seccomp() -> Result<(), String> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for sc in [
        libc::SYS_recvmsg,
        libc::SYS_sendto,
        libc::SYS_fstat,
        libc::SYS_newfstatat,
        libc::SYS_statx,
        libc::SYS_fstatfs,
        libc::SYS_fcntl,
        libc::SYS_close,
        libc::SYS_brk,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_sigaltstack,
        libc::SYS_rt_sigreturn,
        libc::SYS_exit,
        libc::SYS_exit_group,
    ] {
        rules.insert(sc, vec![]);
    }
    // The two backing ioctls and nothing else (the kernel uses the low 32
    // bits of the command, which is what a Dword comparison checks).
    rules.insert(
        libc::SYS_ioctl,
        vec![
            rule(vec![cond(1, Cmp::Eq, FUSE_DEV_IOC_BACKING_OPEN)?])?,
            rule(vec![cond(1, Cmp::Eq, FUSE_DEV_IOC_BACKING_CLOSE)?])?,
        ],
    );
    // Diagnostics go to stderr; nothing may be written to /dev/fuse.
    rules.insert(libc::SYS_write, vec![rule(vec![cond(0, Cmp::Eq, 2)?])?]);
    // Allocator memory, never executable.
    rules.insert(
        libc::SYS_mmap,
        vec![rule(vec![cond(2, Cmp::MaskedEq(libc::PROT_EXEC as u64), 0)?])?],
    );

    let arch = std::env::consts::ARCH.try_into().map_err(|_| format!("seccomp: unsupported architecture {}", std::env::consts::ARCH))?;
    let filter = SeccompFilter::new(rules, SeccompAction::KillProcess, SeccompAction::Allow, arch)
        .map_err(|e| format!("seccomp filter: {:?}", e))?;
    let prog: BpfProgram = filter.try_into().map_err(|e| format!("seccomp compile: {:?}", e))?;
    seccompiler::apply_filter(&prog).map_err(|e| format!("seccomp install: {:?}", e))
}
