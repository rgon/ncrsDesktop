//! HTTP/3 transport probe: exercises the exact client construction the daemon
//! uses and reports how QUIC behaves across connection reuse, idle timeouts and
//! (when run under `iptables`) a silently dying network path.
//!
//!   cargo run --example h3probe -- <url> [sleep_secs ...]
//!
//! Performs one request, then one more after each listed sleep, timing each and
//! printing the full error chain on failure. Compare an `--h2` run to tell a
//! QUIC problem from a server one.

use std::time::{Duration, Instant};

fn full_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s.push_str(&format!("\n    caused by: {}", c));
        cur = c.source();
    }
    s
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let h2 = args.iter().any(|a| a == "--h2");
    let v3 = args.iter().any(|a| a == "--v3");
    args.retain(|a| a != "--h2" && a != "--v3");
    let url = args.first().cloned().unwrap_or_else(|| {
        eprintln!("usage: h3probe [--h2] <url> [sleep_secs ...]");
        std::process::exit(2);
    });
    let sleeps: Vec<u64> = args[1..].iter().filter_map(|s| s.parse().ok()).collect();

    // Mirror the daemon's metadata client (lib.rs build_pair).
    let mut b = reqwest::blocking::Client::builder()
        .pool_max_idle_per_host(16)
        .connect_timeout(Duration::from_secs(5));
    if !h2 {
        b = b.http3_prior_knowledge();
    }
    let client = b.build().expect("client");

    let shoot = |label: &str| {
        let t0 = Instant::now();
        let mut rb = client.get(&url).timeout(Duration::from_secs(10));
        if v3 {
            // In reqwest 0.13 the client-level http3_prior_knowledge() only
            // builds the QUIC connector; requests reach it solely when the
            // REQUEST version is HTTP_3.
            rb = rb.version(reqwest::Version::HTTP_3);
        }
        match rb.send() {
            Ok(r) => println!(
                "[{label}] {:?} {} in {:?}",
                r.version(),
                r.status(),
                t0.elapsed()
            ),
            Err(e) => println!("[{label}] FAILED in {:?}: {}", t0.elapsed(), full_chain(&e)),
        }
    };

    shoot("fresh");
    shoot("reuse-immediate");
    for (i, secs) in sleeps.iter().enumerate() {
        println!("  … sleeping {secs}s");
        std::thread::sleep(Duration::from_secs(*secs));
        shoot(&format!("after-sleep-{}", i + 1));
    }
}
