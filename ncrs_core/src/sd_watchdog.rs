//! systemd's service watchdog (`WatchdogSec=` in packaging/ncrs.service).
//!
//! systemd sets `WATCHDOG_USEC` and kills, then restarts, a service that stops
//! sending `WATCHDOG=1` for that long. Restarting keeps the mount (the FUSE
//! connection waits in the fd store, see `handoff`), so the watchdog turns a
//! daemon that hangs — a deadlocked lock, a FUSE pool that stopped taking work —
//! into a short pause for the programs using the mount, instead of a mount that
//! hangs until someone kills it by hand.
//!
//! The `sd-watchdog` thread pings every WATCHDOG_USEC/3, but only while the
//! daemon looks healthy:
//! - each registered global lock (`cache`, `open_files`, the journal) can be
//!   taken within [`LOCK_WAIT`] — polled with try_lock, never blocking;
//! - the FUSE pool is not stuck: runnable requests waiting for a worker on
//!   every sample for more than WATCHDOG_USEC/2 while no request completed.
//!   One slow upload or download holds one worker and leaves the rest serving,
//!   so it never stops the pings.
//!
//! Before the mount is up nothing is registered and the thread pings
//! unconditionally, so a slow start (keyring, network) is not killed.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::MutexExt;

/// How long a lock may stay unavailable before the daemon counts as hung.
const LOCK_WAIT: Duration = Duration::from_secs(5);

type LockProbe = Box<dyn Fn(Duration) -> bool + Send>;

static PROBES: Mutex<Vec<(&'static str, LockProbe)>> = Mutex::new(Vec::new());

/// Adds a global lock to the health check. `try_for` returns whether the lock
/// could be taken within the given time.
pub(crate) fn watch_lock(name: &'static str, try_for: impl Fn(Duration) -> bool + Send + 'static) {
    let mut probes = PROBES.safe_lock();
    probes.retain(|(n, _)| *n != name);
    probes.push((name, Box::new(try_for)));
}

/// Polls `try_lock` until it succeeds or `within` passes. A poisoned lock
/// counts as available: `safe_lock` callers carry on through poisoning.
pub(crate) fn lock_available<T>(m: &Mutex<T>, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        match m.try_lock() {
            Ok(_) | Err(std::sync::TryLockError::Poisoned(_)) => return true,
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The ping interval systemd asked for, if it asked this process.
fn interval_from_env(usec: Option<&str>, pid: Option<&str>, me: u32) -> Option<Duration> {
    let usec: u64 = usec?.parse().ok().filter(|u| *u > 0)?;
    if let Some(p) = pid {
        if p.parse::<u32>().ok() != Some(me) {
            return None;
        }
    }
    Some(Duration::from_micros(usec))
}

/// Tracks whether the FUSE pool keeps making progress.
#[derive(Debug, Default)]
struct PoolProbe {
    last_completed: Option<u64>,
    /// When the pool was first seen with runnable work and no completion since.
    stuck_since: Option<Instant>,
}

impl PoolProbe {
    /// One sample; returns how long the pool has been stuck.
    fn observe(&mut self, queued: usize, completed: u64, now: Instant) -> Option<Duration> {
        let progressed = self.last_completed != Some(completed);
        self.last_completed = Some(completed);
        if queued == 0 || progressed {
            self.stuck_since = None;
            return None;
        }
        Some(now.duration_since(*self.stuck_since.get_or_insert(now)))
    }
}

/// The health decision: Err(why) withholds the ping.
fn verdict(unavailable_locks: &[&str], pool_stuck_for: Option<Duration>, stuck_limit: Duration) -> Result<(), String> {
    if !unavailable_locks.is_empty() {
        return Err(format!(
            "lock(s) {} not available for {}s",
            unavailable_locks.join(", "),
            LOCK_WAIT.as_secs()
        ));
    }
    match pool_stuck_for {
        Some(d) if d > stuck_limit => Err(format!(
            "FUSE pool has had requests waiting for a worker for {}s with none completing",
            d.as_secs()
        )),
        _ => Ok(()),
    }
}

/// How long a daemon that took over a live connection may take to serve it.
/// Generous: a slow but healthy start (a large persisted cache, a slow keyring) must not
/// cost everyone their open files; only a start that is plainly stuck gives the mount up.
const TAKEOVER_LIMIT: Duration = Duration::from_secs(180);

/// A daemon that took over a live connection keeps programs on the mount waiting until
/// it serves it, so starting up may not take forever: past `limit` the watchdog gives the
/// connection up itself (handoff::give_up_stalled_takeover). Withholding pings would not
/// do: systemd's kills would come WatchdogSec apart, too slow for StartLimitBurst to ever
/// end the mount. A fresh start has no such bound: nobody waits on it, and it may sit at
/// a locked keyring.
fn takeover_verdict(pending_for: Option<Duration>, limit: Duration) -> Result<(), String> {
    match pending_for {
        Some(d) if d > limit => Err(format!(
            "took over the mount {}s ago and still does not serve it",
            d.as_secs()
        )),
        _ => Ok(()),
    }
}

/// Starts the `sd-watchdog` thread when systemd enabled the watchdog for this process.
pub(crate) fn spawn() {
    let Some(every) = interval_from_env(
        std::env::var("WATCHDOG_USEC").ok().as_deref(),
        std::env::var("WATCHDOG_PID").ok().as_deref(),
        std::process::id(),
    ) else {
        return;
    };
    let r = crate::bg::spawn_service("sd-watchdog", move || {
        log::info!("watchdog: systemd expects a ping every {:?}", every);
        let mut pool = PoolProbe::default();
        loop {
            let s = crate::bg::FUSE.stats();
            let stuck = pool.observe(s.queued, s.completed, Instant::now());
            let unavailable: Vec<&str> = PROBES
                .safe_lock()
                .iter()
                .filter(|(_, ok)| !ok(LOCK_WAIT))
                .map(|(name, _)| *name)
                .collect();
            if let Err(why) = takeover_verdict(crate::handoff::takeover_pending_for(), TAKEOVER_LIMIT) {
                log::error!("watchdog: {} — giving the mount up", why);
                crate::handoff::give_up_stalled_takeover();
                continue;
            }
            match verdict(&unavailable, stuck, every / 2) {
                Ok(()) => {
                    let _ = crate::handoff::sd_notify("WATCHDOG=1", None);
                }
                Err(why) => log::warn!("watchdog: withholding the ping — {}", why),
            }
            std::thread::sleep(every / 3);
        }
    });
    if let Err(e) = r {
        log::warn!("watchdog: cannot start its thread ({}) — systemd will restart the daemon when the watchdog expires", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_comes_from_watchdog_usec_for_this_pid_only() {
        assert_eq!(interval_from_env(Some("6000000"), None, 7), Some(Duration::from_secs(6)));
        assert_eq!(interval_from_env(Some("6000000"), Some("7"), 7), Some(Duration::from_secs(6)));
        assert_eq!(interval_from_env(Some("6000000"), Some("8"), 7), None, "meant for another process");
        assert_eq!(interval_from_env(Some("0"), None, 7), None);
        assert_eq!(interval_from_env(None, None, 7), None);
        assert_eq!(interval_from_env(Some("x"), None, 7), None);
    }

    #[test]
    fn a_takeover_that_never_starts_serving_stops_the_pings() {
        let limit = Duration::from_secs(60);
        assert!(takeover_verdict(None, limit).is_ok(), "a fresh start is not bounded");
        assert!(takeover_verdict(Some(Duration::from_secs(30)), limit).is_ok());
        assert!(takeover_verdict(Some(Duration::from_secs(61)), limit).is_err());
    }

    #[test]
    fn a_pool_that_keeps_completing_is_never_stuck() {
        let mut p = PoolProbe::default();
        let t = Instant::now();
        for i in 0..10u64 {
            // Busy, with a backlog, but requests finish between samples.
            assert_eq!(p.observe(5, i, t + Duration::from_secs(i * 30)), None);
        }
    }

    #[test]
    fn one_slow_request_is_not_a_stuck_pool() {
        // A single long upload: nothing queued, nothing completing for minutes.
        let mut p = PoolProbe::default();
        let t = Instant::now();
        for i in 0..10u64 {
            assert_eq!(p.observe(0, 42, t + Duration::from_secs(i * 30)), None);
        }
        assert!(verdict(&[], None, Duration::from_secs(45)).is_ok());
    }

    #[test]
    fn a_backlog_with_no_completions_is_stuck_after_the_limit() {
        let mut p = PoolProbe::default();
        let t = Instant::now();
        let limit = Duration::from_secs(45);
        assert_eq!(p.observe(3, 10, t), None, "the first sample only learns the counter");
        let d1 = p.observe(3, 10, t + Duration::from_secs(30));
        assert_eq!(d1, Some(Duration::ZERO));
        assert!(verdict(&[], d1, limit).is_ok());
        let d2 = p.observe(3, 10, t + Duration::from_secs(60));
        assert!(verdict(&[], d2, limit).is_ok(), "30s stuck, under the limit");
        let d3 = p.observe(3, 10, t + Duration::from_secs(90));
        assert!(verdict(&[], d3, limit).is_err(), "60s stuck, past the limit");
        // One completion resets it.
        assert_eq!(p.observe(3, 11, t + Duration::from_secs(120)), None);
    }

    #[test]
    fn an_unavailable_lock_withholds_the_ping() {
        let e = verdict(&["cache"], None, Duration::from_secs(45)).unwrap_err();
        assert!(e.contains("cache"));
    }

    #[test]
    fn lock_available_polls_without_blocking() {
        let m = Mutex::new(0);
        assert!(lock_available(&m, Duration::from_millis(10)));
        let g = m.lock().unwrap();
        let t = Instant::now();
        assert!(!lock_available(&m, Duration::from_millis(50)));
        assert!(t.elapsed() < Duration::from_secs(2));
        drop(g);
    }
}
