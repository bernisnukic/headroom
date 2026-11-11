//! macOS collector. System memory comes from host_statistics64 and sysctl,
//! per-app RAM from proc_pid_rusage (phys_footprint, the figure Activity
//! Monitor shows as "Memory"). Without root only the user's own processes can
//! be read; everything else is counted as hidden and skipped.
//!
//! The snapshot types mirror collect.rs so the shared UI renders either.

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::cpu::{CpuCollector, ProcTime};
use crate::gpu::GpuCollector;

pub const KIB: u64 = 1;
pub const MIB: u64 = 1024 * KIB;
pub const GIB: u64 = 1024 * MIB;

/// All sizes in KiB. The fields keep their Linux names so the shared UI can
/// draw them; on macOS they hold:
/// - `anon`: app memory (anonymous pages that are not purgeable)
/// - `shmem`: memory held by the compressor
/// - `kernel`: wired memory plus whatever the page counts don't cover
/// - `file_cache`: cached files and purgeable memory, given back on demand
/// - `available`: `file_cache` + `free`
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

/// Linux pressure stall figures. macOS has no equivalent, so these stay at
/// zero there; `Pressure` carries the macOS reading instead.
#[derive(Clone, Copy, Default, Debug)]
pub struct Psi {
    pub some10: f32,
    pub full10: f32,
}

/// The kernel's own memory pressure verdict.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Pressure {
    /// kern.memorystatus_vm_pressure_level: 1 normal, 2 warning, 4 critical.
    pub level: u8,
    /// kern.memorystatus_level: percent of memory the kernel counts as free.
    pub free_pct: u8,
}

impl Pressure {
    pub fn label(&self) -> &'static str {
        match self.level {
            0 | 1 => "Normal",
            2 => "Elevated",
            _ => "Critical",
        }
    }

    pub fn level(&self) -> Level {
        match self.level {
            0 | 1 => Level::Info,
            2 => Level::Warn,
            _ => Level::Bad,
        }
    }
}

/// systemd-oomd's limit on Linux. Nothing kills apps on a pressure threshold
/// on macOS; the limit is set out of reach so the shared Linux warning path
/// never fires.
#[derive(Clone, Copy, Debug)]
pub struct OomdPolicy {
    pub limit_pct: f32,
}

pub fn oomd_policy() -> OomdPolicy {
    OomdPolicy { limit_pct: 100.0 }
}

#[derive(Clone, Default, Debug)]
pub struct App {
    pub name: String,
    pub pids: Vec<i32>,
    /// Physical footprint, KiB. Includes the app's compressed and swapped pages.
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

/// Parts of the session that come back on their own or log you out when
/// stopped, so "restart it" is not useful advice.
pub const NO_RESTART: &[&str] = &["WindowServer", "loginwindow", "Dock", "Finder", "SystemUIServer"];

/// An OOM kill. macOS keeps no such log for normal users, so none are made.
#[allow(dead_code)]
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
    pub user_psi: Psi,
    pub tmpfs: u64,
    pub pressure: Pressure,
    pub apps: Vec<App>,
    pub hidden_procs: usize,
    pub took: Duration,
    pub cpu: crate::cpu::CpuSnap,
    pub gpu: crate::gpu::GpuSnap,
    pub board: crate::sensors::Board,
}

// libc marks its Mach bindings deprecated in favour of another crate; these
// three calls are all that is needed, so they are declared here.
#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_host_self() -> libc::mach_port_t;
    fn mach_timebase_info(info: *mut Timebase) -> libc::kern_return_t;
    static mach_task_self_: libc::mach_port_t;
}

/// The host port. mach_host_self() adds a send right on every call, so ask once.
pub fn host() -> libc::mach_port_t {
    static HOST: OnceLock<libc::mach_port_t> = OnceLock::new();
    *HOST.get_or_init(|| unsafe { mach_host_self() })
}

pub fn task_self() -> libc::mach_port_t {
    unsafe { mach_task_self_ }
}

fn sysctl<T: Copy>(name: &str) -> Option<T> {
    let c = CString::new(name).ok()?;
    let mut v: T = unsafe { std::mem::zeroed() };
    let mut len = size_of::<T>();
    let r = unsafe { libc::sysctlbyname(c.as_ptr(), (&raw mut v).cast(), &mut len, std::ptr::null_mut(), 0) };
    (r == 0 && len == size_of::<T>()).then_some(v)
}

fn total_kib() -> u64 {
    static TOTAL: OnceLock<u64> = OnceLock::new();
    *TOTAL.get_or_init(|| sysctl::<u64>("hw.memsize").unwrap_or(0) / 1024)
}

pub fn meminfo() -> MemInfo {
    let total = total_kib();
    let page = unsafe { libc::vm_page_size } as u64 / 1024;
    let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let kr = unsafe { libc::host_statistics64(host(), libc::HOST_VM_INFO64, (&raw mut vm).cast(), &mut count) };
    let mut m = MemInfo { total, ..Default::default() };
    if kr == libc::KERN_SUCCESS {
        let purgeable = vm.purgeable_count as u64;
        m.anon = (vm.internal_page_count as u64).saturating_sub(purgeable) * page;
        m.file_cache = (vm.external_page_count as u64 + purgeable) * page;
        m.shmem = vm.compressor_page_count as u64 * page;
        // free_count includes speculative pages, which are also file-backed
        m.free = (vm.free_count as u64).saturating_sub(vm.speculative_count as u64) * page;
        m.kernel = total.saturating_sub(m.anon + m.file_cache + m.shmem + m.free);
        m.available = m.file_cache + m.free;
    }
    if let Some(sw) = sysctl::<libc::xsw_usage>("vm.swapusage") {
        m.swap_total = sw.xsu_total / 1024;
        m.swap_used = sw.xsu_used / 1024;
    }
    m
}

pub fn pressure() -> Pressure {
    Pressure {
        level: sysctl::<i32>("kern.memorystatus_vm_pressure_level").unwrap_or(1).clamp(0, 255) as u8,
        free_pct: sysctl::<i32>("kern.memorystatus_level").unwrap_or(0).clamp(0, 100) as u8,
    }
}

/// Converts mach absolute time units to nanoseconds. On Apple Silicon one
/// unit is 125/3 ns; on Intel it is 1 ns.
pub fn abs_to_ns(t: u64) -> u64 {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    let (n, d) = *TB.get_or_init(|| {
        let mut tb = Timebase { numer: 1, denom: 1 };
        unsafe { mach_timebase_info(&mut tb) };
        (tb.numer.max(1) as u64, tb.denom.max(1) as u64)
    });
    (t as u128 * n as u128 / d as u128) as u64
}

const PROC_UID_ONLY: u32 = 4;

/// The user's pids, and how many processes exist in all.
fn pids(buf: &mut Vec<i32>) -> usize {
    let all = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) }.max(0) as usize;
    let uid = unsafe { libc::getuid() };
    let need = unsafe { libc::proc_listpids(PROC_UID_ONLY, uid, std::ptr::null_mut(), 0) }.max(0) as usize;
    buf.clear();
    // room for processes started between the two calls
    buf.resize(need / size_of::<i32>() + 32, 0);
    let got = unsafe {
        libc::proc_listpids(PROC_UID_ONLY, uid, buf.as_mut_ptr().cast(), (buf.len() * size_of::<i32>()) as i32)
    }
    .max(0) as usize;
    buf.truncate(got / size_of::<i32>());
    buf.retain(|p| *p > 0);
    all.max(buf.len())
}

fn rusage(pid: i32) -> Option<libc::rusage_info_v4> {
    let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, (&raw mut ri).cast()) };
    (r == 0).then_some(ri)
}

/// CPU time (user + system) of one process, in ns.
pub fn cpu_ns(pid: i32) -> Option<u64> {
    rusage(pid).map(|ri| abs_to_ns(ri.ri_user_time + ri.ri_system_time))
}

/// Groups helper processes under their app: the outermost `.app` bundle in
/// the executable's path, so "Google Chrome Helper (Renderer)" deep inside
/// Google Chrome.app counts as "Google Chrome". Anything outside a bundle
/// goes by its executable name.
fn app_name(path: &str) -> String {
    if let Some(i) = path.find(".app/") {
        let head = &path[..i];
        return head.rsplit('/').next().unwrap_or(head).to_string();
    }
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn exe_name(pid: i32) -> String {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n > 0 {
        return app_name(&String::from_utf8_lossy(&buf[..n as usize]));
    }
    let mut name = [0u8; 64];
    let n = unsafe { libc::proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) };
    String::from_utf8_lossy(&name[..n.max(0) as usize]).into_owned()
}

/// Wall-clock start time, seconds since the epoch.
fn start_secs(pid: i32) -> u64 {
    let mut bi: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut bi).cast(), size) };
    if n == size { bi.pbi_start_tvsec } else { 0 }
}

/// What doesn't change for a running process. `start_abs` (mach time at
/// start) guards against pid reuse.
struct Known {
    start_abs: u64,
    name: String,
    start_s: u64,
    seen: bool,
}

struct Proc {
    pid: i32,
    name: String,
    footprint: u64,
    age_s: u64,
    cpu_ns: u64,
}

/// One pass over the user's processes: a single proc_pid_rusage call each,
/// plus path and start time the first time a pid shows up.
fn procs(known: &mut HashMap<i32, Known>, buf: &mut Vec<i32>) -> (Vec<Proc>, usize) {
    let all = pids(buf);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    for k in known.values_mut() {
        k.seen = false;
    }
    let mut out = Vec::with_capacity(buf.len());
    for &pid in buf.iter() {
        let Some(ri) = rusage(pid) else { continue };
        if ri.ri_phys_footprint == 0 {
            continue; // exiting
        }
        let k = match known.get_mut(&pid) {
            Some(k) if k.start_abs == ri.ri_proc_start_abstime => k,
            _ => {
                let k = Known {
                    start_abs: ri.ri_proc_start_abstime,
                    name: exe_name(pid),
                    start_s: start_secs(pid),
                    seen: false,
                };
                known.insert(pid, k);
                known.get_mut(&pid).unwrap()
            }
        };
        k.seen = true;
        out.push(Proc {
            pid,
            name: k.name.clone(),
            footprint: ri.ri_phys_footprint / 1024,
            age_s: if k.start_s > 0 { now.saturating_sub(k.start_s) } else { 0 },
            cpu_ns: abs_to_ns(ri.ri_user_time + ri.ri_system_time),
        });
    }
    known.retain(|_, k| k.seen);
    let hidden = all.saturating_sub(out.len());
    (out, hidden)
}

/// Keeps the slow-changing bits between refreshes.
pub struct Collector {
    history: HashMap<String, VecDeque<(Instant, u64)>>,
    known: HashMap<i32, Known>,
    pids: Vec<i32>,
    last: Option<Snapshot>,
    cpu: CpuCollector,
    gpu_c: GpuCollector,
}

const HISTORY: Duration = Duration::from_secs(60 * 60);
const SAMPLE_EVERY: Duration = Duration::from_secs(30);
pub const SPARK_POINTS: usize = 40;

impl Collector {
    pub fn new() -> Self {
        Self {
            history: HashMap::new(),
            known: HashMap::new(),
            pids: Vec::new(),
            last: None,
            cpu: CpuCollector::new(),
            gpu_c: GpuCollector::new(),
        }
    }

    /// Full scan. `want_cpu` adds per-app CPU, `want_gpu` reads the GPU's
    /// statistics; each only while its tab is visible.
    pub fn collect(&mut self, want_cpu: bool, want_gpu: bool) -> Snapshot {
        let t0 = Instant::now();
        let mem = meminfo();
        let (procs, hidden) = procs(&mut self.known, &mut self.pids);
        let mut cpu = self.cpu.sample_system(true);
        if want_cpu {
            let list: Vec<ProcTime> =
                procs.iter().map(|p| ProcTime { pid: p.pid, name: &p.name, age_s: p.age_s, cpu_ns: p.cpu_ns }).collect();
            self.cpu.sample_apps(&mut cpu, &list);
        } else {
            self.cpu.reset_apps();
        }
        let gpu = if want_gpu {
            self.gpu_c.sample()
        } else {
            self.gpu_c.reset();
            crate::gpu::GpuSnap::default()
        };

        let mut apps: HashMap<String, App> = HashMap::new();
        for p in &procs {
            let a = apps.entry(p.name.clone()).or_insert_with(|| App { name: p.name.clone(), ..Default::default() });
            a.pids.push(p.pid);
            a.pss += p.footprint;
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

        let snap = Snapshot {
            mem,
            user_psi: Psi::default(),
            tmpfs: 0,
            pressure: pressure(),
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

    /// Cheap refresh: memory, pressure and system CPU only; the app list
    /// comes from the last full scan.
    pub fn collect_light(&mut self, open: bool) -> Snapshot {
        let Some(mut s) = self.last.clone() else { return self.collect(false, false) };
        s.mem = meminfo();
        s.pressure = pressure();
        let apps = std::mem::take(&mut s.cpu.apps);
        s.cpu = self.cpu.sample_system(open);
        if open {
            s.cpu.apps = apps;
        } else {
            self.cpu.reset_apps();
        }
        s
    }
}

/// No kill log on macOS.
pub fn kills() -> Vec<Kill> {
    Vec::new()
}

/// Nothing to follow on macOS.
pub fn watch_kills(_on_kill: impl Fn(Kill) + Send + 'static) {}

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

/// Plain-language reasons the machine is tight. macOS fills RAM on purpose
/// (cache, compression), so the kernel's pressure level leads, not "used".
pub fn insights(s: &Snapshot, _pol: OomdPolicy) -> Vec<Insight> {
    let mut v = Vec::new();
    match s.pressure.level {
        0 | 1 => {}
        2 => v.push(Insight {
            level: Level::Warn,
            text: format!(
                "Memory pressure is elevated. macOS is compressing memory ({}) and swapping to disk to keep up, so apps you switch back to may be slow.",
                fmt_kib(s.mem.shmem)
            ),
        }),
        _ => v.push(Insight {
            level: Level::Bad,
            text: "Memory pressure is critical. Quit something big from the list below before macOS asks you to.".into(),
        }),
    }
    if s.mem.swap_used > 4 * GIB && s.mem.swap_used * 2 > s.mem.total {
        v.push(Insight {
            level: Level::Warn,
            text: format!("{} is swapped out to disk. Apps that sat idle will take a moment to come back.", fmt_kib(s.mem.swap_used)),
        });
    }
    for a in s.apps.iter().take(15) {
        // Only call out steady, sustained growth: 45+ min of history, a
        // straight-line fit, and 500+ MB gained.
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
    if v.is_empty() {
        v.push(Insight {
            level: Level::Info,
            text: format!("Nothing needs attention. {} available and memory pressure is normal.", fmt_kib(s.mem.available)),
        });
    }
    v
}

/// Terminal report: `headroom --dump`
pub fn dump() {
    let mut c = Collector::new();
    // CPU figures are deltas, so give the first sample something to measure
    std::thread::sleep(Duration::from_millis(500));
    let s = c.collect(true, true);
    let t = Instant::now();
    let _ = c.collect(true, true);
    let full = t.elapsed();
    let t = Instant::now();
    let _ = meminfo();
    let light = t.elapsed();
    let m = &s.mem;
    println!(
        "RAM {} used of {} ({} available) · swap {} / {}",
        fmt_kib(m.used()),
        fmt_kib(m.total),
        fmt_kib(m.available),
        fmt_kib(m.swap_used),
        fmt_kib(m.swap_total)
    );
    println!(
        "apps {} · compressed {} · wired and system {} · cache {} · free {}",
        fmt_kib(m.anon),
        fmt_kib(m.shmem),
        fmt_kib(m.kernel),
        fmt_kib(m.file_cache),
        fmt_kib(m.free)
    );
    println!(
        "memory pressure: {} (level {}, {}% free by the kernel's count)",
        s.pressure.label(),
        s.pressure.level,
        s.pressure.free_pct
    );
    let cores: Vec<String> = s.cpu.cores.iter().map(|p| format!("{p:.0}")).collect();
    println!(
        "CPU {:.0}% across {} cores [{}] · load {:.2} {:.2} {:.2}",
        s.cpu.total,
        s.cpu.cores.len(),
        cores.join(" "),
        s.cpu.load[0],
        s.cpu.load[1],
        s.cpu.load[2]
    );
    for d in &s.gpu.devices {
        let eng: Vec<String> = d.engines.iter().map(|e| format!("{} {:.0}%", e.name, e.busy)).collect();
        println!(
            "GPU {}: {:.0}% busy ({}) · {} in use",
            d.name,
            d.busy,
            eng.join(", "),
            d.mem_used.map(fmt_kib).unwrap_or_else(|| "-".into())
        );
    }
    println!("\n{:>9} {:>8}  APP", "RAM", "UP");
    for a in s.apps.iter().take(15) {
        println!("{:>9} {:>8}  {} ({} procs)", fmt_kib(a.pss), fmt_age(a.age_s), a.name, a.pids.len());
    }
    println!("\n{:>7} {:>9} {:>7}  APP", "CPU", "CPU TIME", "THREADS");
    for a in s.cpu.apps.iter().take(8) {
        println!("{:>6.1}% {:>8}s {:>7}  {}", a.pct, a.cpu_secs, a.threads, a.name);
    }
    println!("\nWhy:");
    for i in insights(&s, oomd_policy()) {
        println!("  [{:?}] {}", i.level, i.text);
    }
    println!(
        "\n{} of your processes read, {} owned by other users skipped",
        s.apps.iter().map(|a| a.pids.len()).sum::<usize>(),
        s.hidden_procs
    );
    println!("full scan {} ms · meminfo only {} µs", full.as_millis(), light.as_micros());
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
    time("pressure", &mut || {
        pressure();
    });
    let mut known = HashMap::new();
    let mut buf = Vec::new();
    procs(&mut known, &mut buf);
    time("procs (cached names)", &mut || {
        procs(&mut known, &mut buf);
    });
    time("procs (cold)", &mut || {
        procs(&mut HashMap::new(), &mut buf);
    });
    let mut g = GpuCollector::new();
    g.sample();
    time("gpu statistics", &mut || {
        g.sample();
    });
    let mut c = Collector::new();
    time("full collect", &mut || {
        c.collect(false, false);
    });
    c.collect(true, false);
    time("full collect + cpu apps", &mut || {
        c.collect(true, false);
    });
    time("full collect + cpu + gpu", &mut || {
        c.collect(true, true);
    });
    time("light collect", &mut || {
        c.collect_light(true);
    });
    let mut b = Vec::new();
    println!("({} processes, {} of them yours)", pids(&mut b), b.len());
}
