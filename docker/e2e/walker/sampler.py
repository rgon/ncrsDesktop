#!/usr/bin/env python3
"""Process sampler + pass/fail evaluator for the ncrs walker harness.

  sampler.py sample <pid> <results_dir> <done_file>
      Samples the daemon every SAMPLE_SECS until <done_file> exists, then keeps
      sampling for IDLE_SECS more (threads draining, idle CPU). Writes
      timeseries.csv, samples.jsonl (per-sample histograms) and sampler.json.

  sampler.py summarize <results_dir> <facts.json>
      Merges sampler.json with end-of-run facts (mount responsive, daemon alive,
      log grep counts, final proxy stats), evaluates the thresholds, writes
      summary.json, prints a human summary and exits 0 on pass / 1 on fail.

Thresholds (env): MAX_THREADS (200), IDLE_MAX_THREADS (60), IDLE_MAX_CPU (5.0,
percent of one core averaged over the last 30s of the idle phase).
"""
import csv
import json
import os
import sys
import time
import urllib.request
from collections import Counter

CLK_TCK = os.sysconf("SC_CLK_TCK")
SYSCALLS = {  # x86_64
    "0": "read", "1": "write", "3": "close", "7": "poll", "16": "ioctl",
    "17": "pread64", "18": "pwrite64", "23": "select", "35": "nanosleep",
    "42": "connect", "44": "sendto", "45": "recvfrom", "46": "sendmsg",
    "47": "recvmsg", "61": "wait4", "202": "futex", "230": "clock_nanosleep",
    "232": "epoll_wait", "257": "openat", "270": "pselect6", "271": "ppoll",
    "281": "epoll_pwait", "441": "epoll_pwait2"
}


def rd(path):
    try:
        with open(path, "rb") as f:
            return f.read().decode(errors="replace")
    except OSError:
        return None


def proc_status(pid):
    s = rd(f"/proc/{pid}/status")
    if s is None:
        return None
    out = {}
    for line in s.splitlines():
        k, _, v = line.partition(":")
        if k in ("Threads", "VmRSS", "VmHWM"):
            out[k] = int(v.split()[0])
    return out


def proc_cpu_ticks(pid):
    s = rd(f"/proc/{pid}/stat")
    if s is None:
        return None
    fields = s[s.rindex(")") + 2:].split()
    return int(fields[11]) + int(fields[12])  # utime + stime


def task_histograms(pid):
    comm, wchan, state, sysc, combo = Counter(), Counter(), Counter(), Counter(), Counter()
    wchan_ok = False
    try:
        tids = os.listdir(f"/proc/{pid}/task")
    except OSError:
        return None
    for tid in tids:
        base = f"/proc/{pid}/task/{tid}"
        c = rd(base + "/comm")
        if c is None:  # task exited between listdir and read
            continue
        c = c.strip()
        comm[c] += 1
        w = rd(base + "/wchan")
        w = (w or "").strip()
        if w and w != "0":
            wchan_ok = True
        wchan[w or "?"] += 1
        st = rd(base + "/stat")
        s = st[st.rindex(")") + 2] if st else "?"
        state[s] += 1
        sc = rd(base + "/syscall")
        if sc is None:
            name = "?"
        else:
            nr = sc.split()[0]
            name = "running" if nr == "running" else SYSCALLS.get(nr, "sys" + nr)
        sysc[name] += 1
        combo[f"{c} | {name}"] += 1
    return {
        "comm": dict(comm.most_common(15)),
        "wchan": dict(wchan.most_common(15)) if wchan_ok else None,
        "state": dict(state),
        "syscall": dict(sysc.most_common(15)),
        "comm_syscall": dict(combo.most_common(15)),
    }


def proxy_stats(proxy):
    if not proxy:
        return None
    try:
        with urllib.request.urlopen(proxy.rstrip("/") + "/__stats", timeout=3) as r:
            return json.load(r)
    except Exception:
        return None


def sample(pid, results, done_file):
    interval = float(os.environ.get("SAMPLE_SECS", "2"))
    idle_secs = float(os.environ.get("IDLE_SECS", "60"))
    proxy = os.environ.get("PROXY", "")
    os.makedirs(results, exist_ok=True)
    csv_f = open(os.path.join(results, "timeseries.csv"), "w", newline="")
    w = csv.writer(csv_f)
    w.writerow(["t_s", "phase", "threads", "rss_kb", "cpu_pct", "state_S", "state_D", "state_R",
                "top_syscall", "proxy_total", "proxy_propfind", "proxy_injected_500",
                "proxy_inflight", "proxy_max_inflight", "proxy_unique_dirs"])
    jl = open(os.path.join(results, "samples.jsonl"), "w")
    t0 = time.time()
    last_ticks, last_t = proc_cpu_ticks(pid), time.time()
    done_at = None
    peak = {"threads": 0}
    rows = []
    alive = True
    while True:
        time.sleep(interval)
        now = time.time()
        if done_at is None and os.path.exists(done_file):
            done_at = now
        phase = "walk" if done_at is None else "idle"
        st = proc_status(pid)
        if st is None:
            alive = False
            print("[sampler] daemon pid %d gone" % pid, flush=True)
            break
        ticks = proc_cpu_ticks(pid)
        cpu = 0.0
        if ticks is not None and last_ticks is not None and now > last_t:
            cpu = (ticks - last_ticks) / CLK_TCK / (now - last_t) * 100.0
        last_ticks, last_t = ticks, now
        hist = task_histograms(pid) or {}
        ps = proxy_stats(proxy) or {}
        t = round(now - t0, 1)
        state = hist.get("state", {})
        top_sys = next(iter(hist.get("syscall", {}) or {"?": 0}))
        row = {"t_s": t, "phase": phase, "threads": st.get("Threads", 0),
               "rss_kb": st.get("VmRSS", 0), "cpu_pct": round(cpu, 1)}
        rows.append(row)
        w.writerow([t, phase, row["threads"], row["rss_kb"], row["cpu_pct"],
                    state.get("S", 0), state.get("D", 0), state.get("R", 0), top_sys,
                    ps.get("total", ""), ps.get("propfind", ""), ps.get("injected_500", ""),
                    ps.get("inflight", ""), ps.get("max_inflight", ""),
                    ps.get("unique_propfind_paths", "")])
        csv_f.flush()
        rec = dict(row, hist=hist, proxy={k: ps.get(k) for k in
                   ("total", "propfind", "injected_500", "inflight", "max_inflight",
                    "unique_propfind_paths")})
        jl.write(json.dumps(rec) + "\n")
        jl.flush()
        if row["threads"] > peak["threads"]:
            peak = dict(rec)
        print("[sampler] t=%6.1fs %-4s threads=%5d rss=%7.1fMB cpu=%6.1f%% top=%s propfind=%s 500s=%s inflight=%s"
              % (t, phase, row["threads"], row["rss_kb"] / 1024, cpu, top_sys,
                 ps.get("propfind", "?"), ps.get("injected_500", "?"), ps.get("inflight", "?")),
              flush=True)
        if done_at is not None and now - done_at >= idle_secs:
            break
    csv_f.close()
    jl.close()

    walk = [r for r in rows if r["phase"] == "walk"]
    idle = [r for r in rows if r["phase"] == "idle"]
    tail_n = max(1, int(30 / interval))
    idle_tail = idle[-tail_n:] if idle else []
    out = {
        "samples": len(rows),
        "walk_samples": len(walk),
        "idle_samples": len(idle),
        "daemon_alive_after_sampling": alive,
        "peak_threads": peak.get("threads", 0),
        "peak_at_s": peak.get("t_s"),
        "peak_phase": peak.get("phase"),
        "peak_hist": peak.get("hist"),
        "peak_rss_mb": round(max((r["rss_kb"] for r in rows), default=0) / 1024, 1),
        "max_cpu_pct": max((r["cpu_pct"] for r in rows), default=0),
        "walk_mean_cpu_pct": round(sum(r["cpu_pct"] for r in walk) / len(walk), 1) if walk else None,
        "threads_at_walk_end": walk[-1]["threads"] if walk else None,
        "threads_after_idle": rows[-1]["threads"] if rows else None,
        "idle_cpu_pct_last30s": round(sum(r["cpu_pct"] for r in idle_tail) / len(idle_tail), 2) if idle_tail else None,
        "final_hist": task_histograms(pid) if alive else None,
    }
    with open(os.path.join(results, "sampler.json"), "w") as f:
        json.dump(out, f, indent=1)


def fmt_hist(h):
    if not h:
        return "    (unavailable)"
    return "\n".join("    %6d  %s" % (v, k) for k, v in sorted(h.items(), key=lambda kv: -kv[1]))


def summarize(results, facts_path):
    with open(os.path.join(results, "sampler.json")) as f:
        s = json.load(f)
    with open(facts_path) as f:
        facts = json.load(f)
    max_threads = int(os.environ.get("MAX_THREADS", "200"))
    idle_max = int(os.environ.get("IDLE_MAX_THREADS", "60"))
    idle_cpu_max = float(os.environ.get("IDLE_MAX_CPU", "5"))
    ps = facts.get("proxy_final") or {}
    uniq = ps.get("unique_propfind_paths") or 0
    d1 = (ps.get("propfind_by_depth") or {}).get("1", 0)
    seeded = int(os.environ.get("SEEDED_DIRS", "0") or 0)

    crit = [
        ("peak threads <= MAX_THREADS (%d)" % max_threads, s["peak_threads"] <= max_threads,
         s["peak_threads"]),
        ("threads after %ss idle <= IDLE_MAX_THREADS (%d)" % (os.environ.get("IDLE_SECS", "60"), idle_max),
         s["threads_after_idle"] is not None and s["threads_after_idle"] <= idle_max,
         s["threads_after_idle"]),
        ("idle CPU%% over last 30s < %.1f%%" % idle_cpu_max,
         s["idle_cpu_pct_last30s"] is not None and s["idle_cpu_pct_last30s"] < idle_cpu_max,
         s["idle_cpu_pct_last30s"]),
        ("mount responsive at end (timeout 10 ls)", bool(facts.get("mount_responsive")),
         facts.get("mount_responsive_detail")),
        ("daemon alive at end", bool(facts.get("daemon_alive")), facts.get("daemon_alive")),
    ]
    nofreeze = None
    nf_path = os.path.join(results, "nofreeze.json")
    if os.path.exists(nf_path):
        with open(nf_path) as f:
            nofreeze = json.load(f)
        crit += [("no-freeze: " + n, ok, v) for n, ok, v in nofreeze["criteria"]]
    elif os.environ.get("SCENARIO") == "nofreeze":
        crit.append(("no-freeze: nofreeze.json written", False, None))
    uploadfreeze = None
    uf_path = os.path.join(results, "uploadfreeze.json")
    if os.path.exists(uf_path):
        with open(uf_path) as f:
            uploadfreeze = json.load(f)
        crit += [("upload-freeze: " + n, ok, v) for n, ok, v in uploadfreeze["criteria"]]
    elif os.environ.get("SCENARIO") == "uploadfreeze":
        crit.append(("upload-freeze: uploadfreeze.json written", False, None))
    passed = all(ok for _, ok, _ in crit)
    walk = facts.get("walk") or {}
    elapsed = walk.get("elapsed_s") or 0
    err = {"ENOENT": 0, "EAGAIN": 0, "EIO": 0, "other": 0}
    for k, v in walk.items():
        if isinstance(v, str):
            for part in v.split():
                name, _, val = part.partition("=")
                if name in err:
                    err[name] += int(val)
    summary = {
        "label": os.environ.get("RUN_LABEL", ""),
        "pass": passed,
        "peak_threads": s["peak_threads"],
        "threads_after_idle": s["threads_after_idle"],
        "walk_errors_by_errno": err,
        "unique_dirs_per_min": round(uniq / (elapsed / 60.0), 1) if elapsed else None,
        "criteria": [{"name": n, "pass": ok, "value": v} for n, ok, v in crit],
        "sampler": s,
        "proxy_final": ps,
        "propfind_depth1_per_unique_dir": round(d1 / uniq, 2) if uniq else None,
        "unique_dirs_listed": uniq,
        "seeded_dirs": seeded,
        "walk": facts.get("walk"),
        "log_counts": facts.get("log_counts"),
        "nofreeze": nofreeze,
        "uploadfreeze": uploadfreeze,
    }
    with open(os.path.join(results, "summary.json"), "w") as f:
        json.dump(summary, f, indent=1)

    print("\n================ ncrs walker summary %s ================" % summary["label"])
    print("  peak threads        : %s (t=%ss, %s phase)" % (s["peak_threads"], s["peak_at_s"], s["peak_phase"]))
    print("  threads @ walk end  : %s   after idle: %s" % (s["threads_at_walk_end"], s["threads_after_idle"]))
    print("  peak RSS            : %s MB   max CPU %s%%   walk mean CPU %s%%   idle CPU(30s) %s%%"
          % (s["peak_rss_mb"], s["max_cpu_pct"], s["walk_mean_cpu_pct"], s["idle_cpu_pct_last30s"]))
    print("  proxy               : total=%s propfind=%s (by depth %s) injected_500=%s max_inflight=%s upstream_err=%s"
          % (ps.get("total"), ps.get("propfind"), ps.get("propfind_by_depth"), ps.get("injected_500"),
             ps.get("max_inflight"), ps.get("upstream_errors")))
    print("  unique dirs listed  : %s of %s seeded;  Depth:1 PROPFIND per unique dir = %s"
          % (uniq, seeded, summary["propfind_depth1_per_unique_dir"]))
    print("  walk                : %s" % json.dumps(facts.get("walk")))
    print("  walk errors by errno: %s   unique dirs/min: %s" % (json.dumps(err), summary["unique_dirs_per_min"]))
    print("  log counts          : %s" % json.dumps(facts.get("log_counts")))
    ph = s.get("peak_hist") or {}
    print("  -- at peak: thread comm")
    print(fmt_hist(ph.get("comm")))
    print("  -- at peak: syscall (from /proc/<pid>/task/*/syscall)")
    print(fmt_hist(ph.get("syscall")))
    print("  -- at peak: comm | syscall")
    print(fmt_hist(ph.get("comm_syscall")))
    print("  -- at peak: wchan")
    print(fmt_hist(ph.get("wchan")))
    print("  -- at peak: task state")
    print(fmt_hist(ph.get("state")))
    print("  -- top PROPFIND paths")
    for e in (ps.get("top_propfind_paths") or [])[:10]:
        print("    %6d  %s" % (e["count"], e["path"]))
    if nofreeze:
        print("  no-freeze probes    : n=%s p50=%sms p99=%sms max=%sms errors=%s; slow stats n=%s max=%ss %s"
              % (nofreeze["probes"], nofreeze["probe_p50_ms"], nofreeze["probe_p99_ms"], nofreeze["probe_max_ms"],
                 nofreeze["probe_errors"], nofreeze["slow_stats"], nofreeze["slow_stat_max_s"],
                 json.dumps(nofreeze["slow_stat_outcomes"])))
    if uploadfreeze:
        print("  upload-freeze       : %s MB copied in %ss; probes n=%s p50=%sms p99=%sms max=%sms errors=%s; server hash %s"
              % (uploadfreeze["upload_mb"], uploadfreeze["copy_s"], uploadfreeze["probes"], uploadfreeze["probe_p50_ms"],
                 uploadfreeze["probe_p99_ms"], uploadfreeze["probe_max_ms"], uploadfreeze["probe_errors"],
                 "match" if uploadfreeze["server_sha256"] == uploadfreeze["want_sha256"] else "MISMATCH"))
    print("  -- criteria")
    for n, ok, v in crit:
        print("    [%s] %s  (value: %s)" % ("PASS" if ok else "FAIL", n, v))
    print("  RESULT: %s" % ("PASS" if passed else "FAIL"))
    return 0 if passed else 1


if __name__ == "__main__":
    if sys.argv[1] == "sample":
        sample(int(sys.argv[2]), sys.argv[3], sys.argv[4])
    elif sys.argv[1] == "summarize":
        sys.exit(summarize(sys.argv[2], sys.argv[3]))
    else:
        sys.exit("usage: sampler.py sample|summarize ...")
