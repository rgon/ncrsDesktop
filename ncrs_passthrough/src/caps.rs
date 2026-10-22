//! Just enough of capget(2)/capset(2) for the helper and the daemon, without
//! linking libcap.

use std::io;

pub const CAP_SYS_ADMIN: u32 = 21;

const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct Header {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Data {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// The calling thread's (effective, permitted, inheritable) sets as 64-bit masks.
fn get() -> io::Result<(u64, u64, u64)> {
    let mut h = Header { version: LINUX_CAPABILITY_VERSION_3, pid: 0 };
    let mut d = [Data::default(); 2];
    // SAFETY: capget writes two Data structs for a version-3 header.
    if unsafe { libc::syscall(libc::SYS_capget, &mut h as *mut Header, d.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let join = |lo: u32, hi: u32| (lo as u64) | ((hi as u64) << 32);
    Ok((
        join(d[0].effective, d[1].effective),
        join(d[0].permitted, d[1].permitted),
        join(d[0].inheritable, d[1].inheritable),
    ))
}

/// Whether `cap` is in this thread's effective set.
pub fn effective_has(cap: u32) -> bool {
    get().is_ok_and(|(e, _, _)| e & (1u64 << cap) != 0)
}

/// Shrinks effective and permitted to `keep` (where currently permitted) and
/// clears the inheritable set. Only ever drops capabilities, which capset
/// always allows.
pub fn restrict_to(keep: &[u32]) -> io::Result<()> {
    let (_, permitted, _) = get()?;
    let want = keep.iter().fold(0u64, |m, c| m | (1u64 << c)) & permitted;
    let mut h = Header { version: LINUX_CAPABILITY_VERSION_3, pid: 0 };
    let lo = want as u32;
    let hi = (want >> 32) as u32;
    let d = [
        Data { effective: lo, permitted: lo, inheritable: 0 },
        Data { effective: hi, permitted: hi, inheritable: 0 },
    ];
    // SAFETY: capset reads two Data structs for a version-3 header.
    if unsafe { libc::syscall(libc::SYS_capset, &mut h as *mut Header, d.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricting_to_nothing_leaves_no_capability() {
        // Runs in a child so the test process keeps whatever it had.
        // SAFETY: the child only makes raw syscalls and _exits.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            let ok = restrict_to(&[]).is_ok() && get().is_ok_and(|(e, p, i)| e == 0 && p == 0 && i == 0);
            unsafe { libc::_exit(if ok { 0 } else { 1 }) };
        }
        let mut status = 0;
        // SAFETY: waits for the child forked above.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    }
}
