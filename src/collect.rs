//! Reads everything from /proc, /sys and the journal. No root needed for the
//! user's own processes; other users' processes are skipped silently.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::process::Command;
use std::time::{Duration, Instant};

pub const KIB: u64 = 1;
pub const MIB: u64 = 1024 * KIB;
pub const GIB: u64 = 1024 * MIB;

/// All sizes in KiB.
#[derive(Clone, Default, Debug)]
pub struct MemInfo {
    pub total: u64,
    pub free: u64,
    pub available: u64,
    pub anon: u64,
    pub shmem: u64,
    pub file_cache: u64,
    pub kernel: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

impl MemInfo {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
}

pub fn meminfo() -> MemInfo {
    let s = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut m: HashMap<&str, u64> = HashMap::new();
    for l in s.lines() {
        let mut it = l.split_whitespace();
        if let (Some(k), Some(v)) = (it.next(), it.next()) {
            m.insert(k.trim_end_matches(':'), v.parse().unwrap_or(0));
        }
    }
    let g = |k: &str| *m.get(k).unwrap_or(&0);
    let total = g("MemTotal");
    let free = g("MemFree");
    let anon = g("AnonPages");
    let shmem = g("Shmem");
    let file_cache = (g("Cached") + g("Buffers") + g("SwapCached")).saturating_sub(shmem);
    // Everything else is kernel: slab, stacks, page tables, drivers.
    let kernel = total.saturating_sub(free + anon + shmem + file_cache);
    MemInfo {
        total,
        free,
        available: g("MemAvailable"),
        anon,
        shmem,
        file_cache,
        kernel,
        swap_total: g("SwapTotal"),
        swap_used: g("SwapTotal").saturating_sub(g("SwapFree")),
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Psi {
    pub some10: f32,
    pub full10: f32,
    pub full60: f32,
}

fn psi(path: &str) -> Psi {
    let s = fs::read_to_string(path).unwrap_or_default();
    let mut p = Psi::default();
    for l in s.lines() {
        let full = l.starts_with("full");
        for kv in l.split_whitespace() {
            let Some((k, v)) = kv.split_once('=') else { continue };
            let v: f32 = v.parse().unwrap_or(0.0);
            match (full, k) {
                (false, "avg10") => p.some10 = v,
                (true, "avg10") => p.full10 = v,
                (true, "avg60") => p.full60 = v,
                _ => {}
            }
        }
    }
    p
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

/// systemd-oomd watches "full" pressure on user@UID.service and kills the
/// biggest app scope once avg10 stays above the limit for the duration.
#[derive(Clone, Copy, Debug)]
pub struct OomdPolicy {
    pub limit_pct: f32,
    pub duration_s: u32,
}

pub fn oomd_policy() -> OomdPolicy {
    let out = Command::new("systemctl")
        .args(["show", &format!("user@{}.service", uid()), "-p", "ManagedOOMMemoryPressureLimit"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let raw: f64 = out.trim().split('=').nth(1).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let limit_pct = if raw > 0.0 { (raw / 4294967296.0 * 100.0) as f32 } else { 50.0 };
    OomdPolicy { limit_pct, duration_s: 20 }
}

#[derive(Clone, Default, Debug)]
pub struct App {
    pub name: String,
    pub pids: Vec<i32>,
    pub pss: u64,
    pub swap: u64,
    pub gpu: u64,
    pub age_s: u64,
    /// Least-squares slope over the history window, MB per hour.
    pub growth_mb_h: Option<f32>,
    /// How well a straight line fits the history (0..1).
    pub growth_r2: f32,
    /// Minutes of history behind the slope, and MB gained over it.
    pub span_min: f32,
    pub gained_mb: f32,
    pub spark: Vec<u64>,
}

impl App {
    pub fn total(&self) -> u64 {
        self.pss + self.swap + self.gpu
    }

    pub fn steady_growth(&self) -> bool {
        self.span_min >= 45.0 && self.gained_mb >= 500.0 && self.growth_r2 >= 0.7
    }
}

/// Processes that are the desktop session itself: can't be restarted on
/// their own under Wayland.
pub const NO_RESTART: &[&str] = &["gnome-shell", "Xwayland", "gnome-session-binary", "systemd"];

#[derive(Clone, Debug)]
pub struct Kill {
    pub when: String,
    pub sort: String,
    pub name: String,
    pub how: &'static str,
    pub detail: String,
}

#[derive(Clone, Default, Debug)]
pub struct Snapshot {
    pub mem: MemInfo,
    pub psi: Psi,
    pub user_psi: Psi,
    pub tmpfs: u64,
    pub gpu_total: u64,
    pub apps: Vec<App>,
    pub hidden_procs: usize,
    pub took: Duration,
    pub cpu: crate::cpu::CpuSnap,
    pub gpu: crate::gpu::GpuSnap,
    /// Board temperatures and fans, refreshed every 10 s while the window is open.
    pub board: crate::sensors::Board,
}

struct Proc {
    pid: i32,
    name: String,
    pss: u64,
    swap: u64,
    age_s: u64,
}

fn clk_tck() -> u64 {
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 { t as u64 } else { 100 }
}

fn app_name(pid: i32) -> String {
    let exe = fs::read_link(format!("/proc/{pid}/exe"))
        .map(|p| p.to_string_lossy().trim_end_matches(" (deleted)").to_string())
        .unwrap_or_default();
    let base = exe.rsplit('/').next().unwrap_or("").to_string();
    if base.is_empty() || base.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        return comm.trim().to_string();
    }
    match base.as_str() {
        "glocom-next" => "gloCOM Next".into(),
        "gloCOM" => "gloCOM".into(),
        "chrome" => "Chrome".into(),
        _ => base,
    }
}

/// What doesn't change for a running process: resolved app name and start
/// time. Keyed by pid; `comm` guards against pid reuse.
struct Known {
    comm: String,
    name: String,
    start_ticks: u64,
    seen: bool,
}

fn start_ticks(pid: i32) -> u64 {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok())
        .unwrap_or(0)
}

/// One pass over /proc. Per process this reads only /proc/PID/status
/// (O(1) kernel counters); name and start time come from `known` after the
/// first sighting. smaps_rollup is avoided: it walks page tables, which is
/// slow for Electron apps with 1.5 TB of reserved address space.
fn procs(known: &mut HashMap<i32, Known>) -> (Vec<Proc>, usize) {
    use std::io::Read;
    let uptime: f64 = fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0);
    let tck = clk_tck() as f64;
    let mut out = Vec::with_capacity(known.len());
    let mut hidden = 0;
    let mut buf = String::with_capacity(2048);
    let mut path = String::with_capacity(32);
    for k in known.values_mut() {
        k.seen = false;
    }
    let Ok(dir) = fs::read_dir("/proc") else { return (out, 0) };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
        path.clear();
        use std::fmt::Write as _;
        let _ = write!(path, "/proc/{pid}/status");
        buf.clear();
        if fs::File::open(&path).and_then(|mut f| f.read_to_string(&mut buf)).is_err() {
            hidden += 1;
            continue;
        }
        let (mut comm, mut anon, mut swap) = ("", 0u64, 0u64);
        for l in buf.lines() {
            if let Some(v) = l.strip_prefix("Name:") {
                comm = v.trim();
            } else if let Some(v) = l.strip_prefix("RssAnon:") {
                anon = v.trim().trim_end_matches(" kB").parse().unwrap_or(0);
            } else if let Some(v) = l.strip_prefix("VmSwap:") {
                swap = v.trim().trim_end_matches(" kB").parse().unwrap_or(0);
                break; // last field we need
            }
        }
        if anon == 0 && swap == 0 {
            continue; // kernel thread
        }
        let k = match known.get_mut(&pid) {
            Some(k) if k.comm == comm => k,
            _ => {
                known.insert(
                    pid,
                    Known { comm: comm.to_string(), name: app_name(pid), start_ticks: start_ticks(pid), seen: false },
                );
                known.get_mut(&pid).unwrap()
            }
        };
        k.seen = true;
        let age_s = (uptime - k.start_ticks as f64 / tck).max(0.0) as u64;
        out.push(Proc { pid, name: k.name.clone(), pss: anon, swap, age_s });
    }
    known.retain(|_, k| k.seen);
    (out, hidden)
}

fn parse_kib(v: &str, unit: Option<&str>) -> u64 {
    let n: f64 = v.parse().unwrap_or(0.0);
    (match unit {
        Some("KiB") => n,
        Some("MiB") => n * 1024.0,
        Some("GiB") => n * 1048576.0,
        _ => n / 1024.0,
    }) as u64
}

/// Every open DRM file descriptor on the system, as (pid, fd number).
/// Expensive (readlink on every fd of every process), so it runs once a
/// minute; in between only these fds' fdinfo is re-read.
fn drm_fds() -> Vec<(i32, String)> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir("/proc") else { return out };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for fd in fds.flatten() {
            if fs::read_link(fd.path()).is_ok_and(|l| l.starts_with("/dev/dri/")) {
                out.push((pid, fd.file_name().to_string_lossy().into_owned()));
            }
        }
    }
    out
}

/// GPU buffer objects per pid, from DRM fdinfo (i915, xe, amdgpu, nvidia-open).
/// Each DRM client is counted once, for its first holder that isn't systemd.
/// Drops fds that were closed since the last full scan.
fn gpu_by_pid(fds: &mut Vec<(i32, String)>) -> HashMap<i32, u64> {
    let mut clients: HashMap<String, (i32, bool, u64)> = HashMap::new();
    let mut systemd: HashMap<i32, bool> = HashMap::new();
    fds.retain(|(pid, fd)| {
        let Ok(info) = fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")) else { return false };
        let (mut pdev, mut id, mut total) = ("", "", 0u64);
        for l in info.lines() {
            let mut it = l.split_whitespace();
            let Some(k) = it.next() else { continue };
            if k == "drm-pdev:" {
                pdev = it.next().unwrap_or("");
            } else if k == "drm-client-id:" {
                id = it.next().unwrap_or("");
            } else if k.starts_with("drm-total-") || k.starts_with("drm-memory-") {
                if let Some(v) = it.next() {
                    total += parse_kib(v, it.next());
                }
            }
        }
        if id.is_empty() {
            return true; // KMS-only fd, keep watching it
        }
        let sd = *systemd.entry(*pid).or_insert_with(|| {
            fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default().trim() == "systemd"
        });
        match clients.get_mut(&format!("{pdev}/{id}")) {
            Some(c) if c.1 && !sd => *c = (*pid, sd, total),
            Some(_) => {}
            None => {
                clients.insert(format!("{pdev}/{id}"), (*pid, sd, total));
            }
        }
        true
    });
    let mut by_pid = HashMap::new();
    for (_, (pid, _, t)) in clients {
        *by_pid.entry(pid).or_insert(0) += t;
    }
    by_pid
}

fn statvfs_used(path: &str) -> u64 {
    let c = std::ffi::CString::new(path).unwrap();
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return 0;
    }
    (s.f_blocks - s.f_bfree) as u64 * s.f_frsize as u64 / 1024
}

/// Keeps the slow-changing bits between refreshes.
pub struct Collector {
    history: HashMap<String, VecDeque<(Instant, u64)>>,
    known: HashMap<i32, Known>,
    gpu: HashMap<i32, u64>,
    gpu_at: Option<Instant>,
    drm: Vec<(i32, String)>,
    drm_at: Option<Instant>,
    last: Option<Snapshot>,
    cpu: crate::cpu::CpuCollector,
    gpu_c: crate::gpu::GpuCollector,
}

const HISTORY: Duration = Duration::from_secs(60 * 60);
const SAMPLE_EVERY: Duration = Duration::from_secs(30);
pub const SPARK_POINTS: usize = 40;

impl Collector {
    pub fn new() -> Self {
        Self {
            history: HashMap::new(),
            known: HashMap::new(),
            gpu: HashMap::new(),
            gpu_at: None,
            drm: Vec::new(),
            drm_at: None,
            last: None,
            cpu: crate::cpu::CpuCollector::new(),
            gpu_c: crate::gpu::GpuCollector::new(),
        }
    }

    /// Full scan. `want_cpu` adds per-app CPU (one more /proc/PID/stat read
    /// per process); only needed while the CPU tab is visible.
    pub fn collect(&mut self, want_cpu: bool, want_gpu: bool) -> Snapshot {
        let t0 = Instant::now();
        let mem = meminfo();
        let (procs, hidden) = procs(&mut self.known);
        let mut cpu = self.cpu.sample_system(true);
        let list: Vec<(i32, &str, u64)> = procs.iter().map(|p| (p.pid, p.name.as_str(), p.age_s)).collect();
        if want_cpu {
            self.cpu.sample_apps(&mut cpu, &list);
        } else {
            self.cpu.reset_apps();
        }
        if self.drm_at.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
            self.drm = drm_fds();
            self.drm_at = Some(Instant::now());
            self.gpu_at = None;
        }
        if self.gpu_at.is_none_or(|t| t.elapsed() > Duration::from_secs(6)) {
            self.gpu = gpu_by_pid(&mut self.drm);
            self.gpu_at = Some(Instant::now());
        }
        let gpu = if want_gpu {
            let list: Vec<(i32, &str, u64)> = procs.iter().map(|p| (p.pid, p.name.as_str(), p.age_s)).collect();
            self.gpu_c.sample(&self.drm, &list, &self.gpu)
        } else {
            self.gpu_c.reset();
            crate::gpu::GpuSnap::default()
        };

        let mut apps: HashMap<String, App> = HashMap::new();
        for p in &procs {
            let a = apps.entry(p.name.clone()).or_insert_with(|| App { name: p.name.clone(), ..Default::default() });
            a.pids.push(p.pid);
            a.pss += p.pss;
            a.swap += p.swap;
            a.gpu += self.gpu.get(&p.pid).copied().unwrap_or(0);
            a.age_s = a.age_s.max(p.age_s);
        }

        let now = Instant::now();
        let mut apps: Vec<App> = apps.into_values().collect();
        for a in &mut apps {
            let h = self.history.entry(a.name.clone()).or_default();
            if h.back().is_none_or(|(t, _)| now.duration_since(*t) >= SAMPLE_EVERY) {
                h.push_back((now, a.total()));
            } else if let Some(last) = h.back_mut() {
                // keep the newest value in the open bucket
                last.1 = a.total();
            }
            while h.front().is_some_and(|(t, _)| now.duration_since(*t) > HISTORY) {
                h.pop_front();
            }
            if let (Some((t0, _)), Some((t1, _))) = (h.front(), h.back()) {
                let span = t1.duration_since(*t0).as_secs_f32();
                // a rate needs 15 min of samples; less is just warm-up noise
                if span >= 900.0 && h.len() >= 10 {
                    let pts: Vec<(f32, f32)> = h
                        .iter()
                        .map(|(t, v)| (t.duration_since(*t0).as_secs_f32() / 3600.0, *v as f32 / 1024.0))
                        .collect();
                    let n = pts.len() as f32;
                    let mx = pts.iter().map(|p| p.0).sum::<f32>() / n;
                    let my = pts.iter().map(|p| p.1).sum::<f32>() / n;
                    let sxy: f32 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
                    let sxx: f32 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
                    let syy: f32 = pts.iter().map(|p| (p.1 - my).powi(2)).sum();
                    if sxx > 0.0 {
                        let slope = sxy / sxx;
                        a.growth_mb_h = Some(slope);
                        a.growth_r2 = if syy > 0.0 { (sxy * sxy) / (sxx * syy) } else { 0.0 };
                        a.span_min = span / 60.0;
                        a.gained_mb = slope * span / 3600.0;
                    }
                }
            }
            a.spark = h.iter().rev().take(SPARK_POINTS).map(|(_, v)| *v).collect::<Vec<_>>();
            a.spark.reverse();
        }
        apps.sort_by(|a, b| b.total().cmp(&a.total()));
        self.history.retain(|k, _| apps.iter().any(|a| &a.name == k));

        let user_cg = format!(
            "/sys/fs/cgroup/user.slice/user-{0}.slice/user@{0}.service/memory.pressure",
            uid()
        );
        let snap = Snapshot {
            psi: psi("/proc/pressure/memory"),
            user_psi: psi(&user_cg),
            tmpfs: statvfs_used("/tmp") + statvfs_used("/dev/shm"),
            gpu_total: self.gpu.values().sum(),
            mem,
            apps,
            hidden_procs: hidden,
            took: t0.elapsed(),
            cpu,
            gpu,
            board: Default::default(),
        };
        self.last = Some(snap.clone());
        snap
    }

    /// Cheap refresh (~100 µs): memory, pressure and tmpfs only; app list
    /// comes from the last full scan.
    /// `open`: the window is showing, so read clock/temperature too and keep
    /// the per-app CPU list from the last full scan.
    pub fn collect_light(&mut self, open: bool) -> Snapshot {
        let Some(mut s) = self.last.clone() else { return self.collect(false, false) };
        s.mem = meminfo();
        let apps = std::mem::take(&mut s.cpu.apps);
        s.cpu = self.cpu.sample_system(open);
        if open {
            s.cpu.apps = apps;
        } else {
            self.cpu.reset_apps();
        }
        s.psi = psi("/proc/pressure/memory");
        s.user_psi = psi(&format!(
            "/sys/fs/cgroup/user.slice/user-{0}.slice/user@{0}.service/memory.pressure",
            uid()
        ));
        s.tmpfs = statvfs_used("/tmp") + statvfs_used("/dev/shm");
        s
    }
}

fn pretty_unit(unit: &str) -> String {
    let mut u = unit.rsplit('/').next().unwrap_or(unit).to_string();
    u = u.replace("\\x2d", "-");
    for suf in [".scope", ".service"] {
        if let Some(s) = u.strip_suffix(suf) {
            u = s.to_string();
        }
    }
    if let Some(s) = u.strip_prefix("snap.") {
        // snap.thunderbird.thunderbird-<uuid>
        return s.split('.').next().unwrap_or(s).to_string();
    }
    u = u.strip_prefix("app-").unwrap_or(&u).to_string();
    u = u.strip_prefix("gnome-").unwrap_or(&u).to_string();
    // drop trailing -<id>
    if let Some((head, tail)) = u.rsplit_once('-') {
        if tail.chars().all(|c| c.is_ascii_hexdigit()) && !head.is_empty() {
            u = head.to_string();
        }
    }
    u.rsplit('.').next().unwrap_or(&u).to_string()
}

fn short_time(iso: &str) -> String {
    // 2026-11-10T13:11:08+01:00 -> 10 Nov 13:11
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let m: usize = iso.get(5..7).and_then(|s| s.parse().ok()).unwrap_or(1);
    let d = iso.get(8..10).unwrap_or("").trim_start_matches('0');
    format!("{d} {} {}", months[m.clamp(1, 12) - 1], iso.get(11..16).unwrap_or(""))
}

fn journal(args: &[&str]) -> String {
    Command::new("journalctl")
        .args(["--since", "-7d", "-o", "short-iso", "--no-pager", "-q"])
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

/// Parses one `journalctl -o short-iso` line from systemd-oomd or the
/// kernel OOM killer.
pub fn parse_kill(l: &str) -> Option<Kill> {
    let ts = l.split_whitespace().next()?;
    if l.contains("systemd-oomd") {
        let rest = l.split_once("Killed ")?.1;
        let (unit, why) = rest.split_once(" due to ").unwrap_or((rest, ""));
        let detail = if let Some(p) = why.split(" being ").nth(1) {
            // "87.70% > 50.00% for > 20s ..." -> "pressure 88%, limit 50%"
            let nums: Vec<f32> = p
                .split('%')
                .take(2)
                .filter_map(|x| x.trim().trim_start_matches('>').trim().parse().ok())
                .collect();
            match nums.as_slice() {
                [v, lim] => format!("pressure {v:.0}%, limit {lim:.0}%"),
                _ => format!("pressure {}", p.split(" for").next().unwrap_or(p)),
            }
        } else {
            why.chars().take(60).collect()
        };
        return Some(Kill { when: short_time(ts), sort: ts.into(), name: pretty_unit(unit), how: "oomd", detail });
    }
    if l.contains("Out of memory: Killed process") {
        let name = l.split_once('(').and_then(|x| x.1.split_once(')')).map(|x| x.0).unwrap_or("?");
        let rss = l
            .split_once("anon-rss:")
            .and_then(|x| x.1.split_once("kB"))
            .and_then(|x| x.0.parse::<u64>().ok())
            .unwrap_or(0);
        return Some(Kill {
            when: short_time(ts),
            sort: ts.into(),
            name: name.into(),
            how: "kernel",
            detail: format!("out of memory, was using {}", fmt_kib(rss)),
        });
    }
    None
}

/// OOM kills from the last 7 days, newest first. Runs once at startup;
/// after that `watch_kills` streams new ones.
pub fn kills() -> Vec<Kill> {
    let mut out: Vec<Kill> = journal(&["-u", "systemd-oomd", "-g", "Killed "])
        .lines()
        .chain(journal(&["-k", "-g", "Out of memory: Killed process"]).lines())
        .filter_map(parse_kill)
        .collect();
    out.sort_by(|a, b| b.sort.cmp(&a.sort));
    out
}

/// Follows the journal for new kills with one long-lived `journalctl -f`.
/// It blocks in the kernel until oomd or the kernel log something, so it
/// costs nothing while idle and reports a kill the moment it happens.
pub fn watch_kills(on_kill: impl Fn(Kill) + Send + 'static) {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    std::thread::spawn(move || loop {
        let child = Command::new("journalctl")
            .args(["-f", "-n", "0", "-q", "-o", "short-iso", "_SYSTEMD_UNIT=systemd-oomd.service", "+", "_TRANSPORT=kernel"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            if let Some(out) = child.stdout.take() {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    if let Some(k) = parse_kill(&line) {
                        on_kill(k);
                    }
                }
            }
            let _ = child.wait();
        }
        std::thread::sleep(Duration::from_secs(30)); // journalctl died; retry
    });
}

pub fn fmt_kib(k: u64) -> String {
    if k >= GIB {
        format!("{:.1} GB", k as f64 / GIB as f64)
    } else if k >= MIB {
        format!("{:.0} MB", k as f64 / MIB as f64)
    } else {
        format!("{k} KB")
    }
}

pub fn fmt_age(s: u64) -> String {
    if s >= 86400 {
        format!("{}d {}h", s / 86400, s % 86400 / 3600)
    } else if s >= 3600 {
        format!("{}h {}m", s / 3600, s % 3600 / 60)
    } else {
        format!("{}m", s / 60)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Level {
    Info,
    Warn,
    Bad,
}

#[derive(Clone, Debug)]
pub struct Insight {
    pub level: Level,
    pub text: String,
}

/// Plain-language reasons the machine is tight.
pub fn insights(s: &Snapshot, pol: OomdPolicy) -> Vec<Insight> {
    let mut v = Vec::new();
    let avail_pct = s.mem.available as f32 / s.mem.total.max(1) as f32 * 100.0;
    let p = s.user_psi.full10;
    if p >= pol.limit_pct * 0.5 {
        v.push(Insight {
            level: if p >= pol.limit_pct * 0.8 { Level::Bad } else { Level::Warn },
            text: format!(
                "Memory pressure is {p:.0}%. systemd-oomd kills your biggest app if it stays above {:.0}% for {} s.",
                pol.limit_pct, pol.duration_s
            ),
        });
    }
    if avail_pct < 10.0 {
        v.push(Insight {
            level: Level::Bad,
            text: format!("Only {} available and swap is {} full.", fmt_kib(s.mem.available), pct(s.mem.swap_used, s.mem.swap_total)),
        });
    } else if s.mem.swap_total > 0 && s.mem.swap_used * 10 > s.mem.swap_total * 8 {
        v.push(Insight { level: Level::Warn, text: format!("Swap is {} full.", pct(s.mem.swap_used, s.mem.swap_total)) });
    }
    for a in s.apps.iter().take(15) {
        if a.name == "gnome-shell" && a.gpu > 3 * GIB {
            v.push(Insight {
                level: Level::Bad,
                text: format!(
                    "gnome-shell holds {} of GPU buffers, far above the usual 1 GB or so. Only logging out and back in, or a reboot, gets that back.",
                    fmt_kib(a.gpu)
                ),
            });
        }
        // Only call out steady, sustained growth: 45+ min of history, a
        // straight-line fit, and 500+ MB gained. Session warm-up after login
        // and apps that grow with their workload don't qualify.
        if a.steady_growth() {
            let advice = if NO_RESTART.contains(&a.name.as_str()) {
                "It is part of your session; logging out and back in resets it."
            } else {
                "If it keeps climbing while idle, restarting it gets the memory back."
            };
            v.push(Insight {
                level: Level::Warn,
                text: format!(
                    "{} grew {:.0} MB over the last {:.0} min at a steady rate and is at {} now. {}",
                    a.name,
                    a.gained_mb,
                    a.span_min,
                    fmt_kib(a.total()),
                    advice
                ),
            });
        }
        if a.total() > 4 * GIB && !NO_RESTART.contains(&a.name.as_str()) {
            v.push(Insight {
                level: Level::Warn,
                text: format!("{} uses {} after running for {}. Restarting it frees most of that.", a.name, fmt_kib(a.total()), fmt_age(a.age_s)),
            });
        }
    }
    if s.tmpfs > 2 * GIB {
        v.push(Insight {
            level: Level::Warn,
            text: format!("/tmp and /dev/shm hold {}. Both live in RAM, so clear old files there.", fmt_kib(s.tmpfs)),
        });
    }
    if v.is_empty() {
        v.push(Insight { level: Level::Info, text: format!("Nothing needs attention. {} available and no memory pressure.", fmt_kib(s.mem.available)) });
    }
    v
}

pub fn pct(a: u64, b: u64) -> String {
    format!("{:.0}%", a as f64 / b.max(1) as f64 * 100.0)
}

/// `headroom --bench`: average cost of each part of a scan.
pub fn bench() {
    let n = 20;
    let time = |label: &str, f: &mut dyn FnMut()| {
        let t = Instant::now();
        for _ in 0..n {
            f();
        }
        println!("{label:<22} {:>8.2} ms", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
    };
    time("meminfo", &mut || {
        meminfo();
    });
    time("psi", &mut || {
        psi("/proc/pressure/memory");
    });
    let mut known = HashMap::new();
    procs(&mut known);
    time("procs (cached names)", &mut || {
        procs(&mut known);
    });
    time("drm fd discovery", &mut || {
        drm_fds();
    });
    let mut fds = drm_fds();
    time("gpu fdinfo (known)", &mut || {
        gpu_by_pid(&mut fds);
    });
    let mut c = Collector::new();
    time("full collect", &mut || {
        c.collect(false, false);
    });
    time("full collect + cpu apps", &mut || {
        c.collect(true, false);
    });
    c.collect(true, true);
    time("full collect + cpu + gpu", &mut || {
        c.collect(true, true);
    });
    time("light collect", &mut || {
        c.collect_light(true);
    });
    let t = Instant::now();
    kills();
    println!("{:<22} {:>8.2} ms", "kills (journalctl x2)", t.elapsed().as_secs_f64() * 1000.0);
    let nprocs = fs::read_dir("/proc").map(|d| d.count()).unwrap_or(0);
    println!("({nprocs} entries in /proc)");
}
