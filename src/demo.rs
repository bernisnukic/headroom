//! `--demo`: a made-up machine for screenshots and recordings. Nothing here
//! reads the system. Every value is a function of a fixed seed and of how many
//! 2 s ticks have passed since launch, so two runs show the same numbers at the
//! same moment. The clock starts 90 minutes in, so trends and history charts
//! are already full.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::collect::{App, GIB, Kill, MIB, MemInfo, OomdPolicy, Snapshot, fmt_age, fmt_kib};
use crate::cpu::{CpuApp, CpuSnap};
use crate::gpu::{GpuApp, GpuDevice, GpuEngine, GpuSnap};

pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::args().any(|a| a == "--demo"))
}

const SEED: u64 = 0x4865_6164_726f_6f6d;
const TICK: f64 = 2.0;
/// Seconds of made-up history before the first tick.
const HEAD_START: f64 = 90.0 * 60.0;
const CORES: usize = 12;
const TOTAL: u64 = 32 * GIB;

/// xorshift64*, enough to vary phases and amplitudes.
struct Rng(u64);

impl Rng {
    fn f(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// One simulated app. Sizes in MB, CPU in % of one core.
struct Sim {
    name: &'static str,
    procs: usize,
    ram: f64,
    wobble: f64,
    leak_per_h: f64,
    swap: f64,
    gpu_mem: f64,
    cpu: f64,
    cpu_wobble: f64,
    threads: u32,
    /// Busy % on each GPU (Linux: integrated, discrete; macOS: the one GPU).
    gpu_busy: [f64; 2],
    age_s: f64,
    phase: f64,
    period: f64,
}

#[allow(clippy::too_many_arguments)]
fn sim(name: &'static str, procs: usize, ram: f64, leak_per_h: f64, swap: f64, gpu_mem: f64, cpu: f64, threads: u32, gpu_busy: [f64; 2], age_h: f64) -> Sim {
    Sim {
        name,
        procs,
        ram,
        wobble: ram * 0.03,
        leak_per_h,
        swap,
        gpu_mem,
        cpu,
        // busy apps hold steady, idle ones flicker
        cpu_wobble: if cpu > 50.0 { cpu * 0.12 } else { cpu * 0.5 },
        threads,
        gpu_busy,
        age_s: age_h * 3600.0,
        phase: 0.0,
        period: 600.0,
    }
}

fn cast() -> Vec<Sim> {
    let mut v = vec![
        sim("Firefox", 14, 3300.0, 0.0, 120.0, 450.0, 9.0, 380, [7.0, 0.0], 6.5),
        sim("Code", 9, 1900.0, 0.0, 40.0, 180.0, 4.0, 210, [1.0, 0.0], 5.2),
        // the slow climber: steady growth that the trend column flags
        sim("Slack", 6, 1150.0, 640.0, 0.0, 120.0, 2.5, 96, [1.0, 0.0], 7.8),
        sim("Factorio", 1, 2600.0, 0.0, 0.0, 1800.0, 118.0, 34, [0.0, 58.0], 1.6),
        sim("Spotify", 5, 540.0, 0.0, 60.0, 90.0, 2.0, 74, [0.5, 0.0], 7.9),
        sim("Terminal", 2, 190.0, 0.0, 0.0, 30.0, 0.8, 12, [0.3, 0.0], 8.1),
    ];
    #[cfg(not(target_os = "macos"))]
    v.extend([
        sim("gnome-shell", 1, 820.0, 0.0, 30.0, 940.0, 3.5, 28, [4.0, 0.0], 8.2),
        sim("Xwayland", 1, 140.0, 0.0, 0.0, 60.0, 0.4, 9, [0.5, 0.0], 8.2),
        sim("tracker-miner-fs-3", 1, 170.0, 0.0, 20.0, 0.0, 0.6, 7, [0.0, 0.0], 8.2),
        sim("evolution-data-server", 4, 120.0, 0.0, 35.0, 0.0, 0.2, 22, [0.0, 0.0], 8.2),
        sim("pipewire", 3, 48.0, 0.0, 0.0, 0.0, 0.9, 11, [0.0, 0.0], 8.2),
    ]);
    #[cfg(target_os = "macos")]
    v.extend([
        sim("WindowServer", 1, 690.0, 0.0, 0.0, 0.0, 6.0, 24, [0.0, 0.0], 8.2),
        sim("mds_stores", 2, 230.0, 0.0, 0.0, 0.0, 0.7, 9, [0.0, 0.0], 8.2),
        sim("Spotlight", 1, 150.0, 0.0, 0.0, 0.0, 0.3, 11, [0.0, 0.0], 8.2),
        sim("Finder", 1, 130.0, 0.0, 0.0, 0.0, 0.2, 8, [0.0, 0.0], 8.2),
        sim("coreaudiod", 1, 45.0, 0.0, 0.0, 0.0, 0.9, 13, [0.0, 0.0], 8.2),
    ]);
    let mut rng = Rng(SEED);
    for s in &mut v {
        s.phase = rng.f() * std::f64::consts::TAU;
        s.period = 400.0 + rng.f() * 1400.0;
        if s.leak_per_h > 0.0 {
            // a climber that wobbled would not read as steady
            s.wobble = 15.0;
        }
    }
    v
}

fn wave(t: f64, period: f64, phase: f64) -> f64 {
    (t / period * std::f64::consts::TAU + phase).sin()
}

impl Sim {
    /// Footprint at virtual time `t`, MB.
    fn ram_at(&self, t: f64) -> f64 {
        let leak = self.leak_per_h * (t - HEAD_START) / 3600.0;
        self.ram + leak + self.wobble * wave(t, self.period, self.phase)
    }

    fn cpu_at(&self, t: f64) -> f64 {
        let w = 0.6 * wave(t, self.period / 7.0, self.phase) + 0.4 * wave(t, 23.0, self.phase * 3.0);
        (self.cpu + self.cpu_wobble * w).max(0.0)
    }

    fn gpu_at(&self, t: f64, dev: usize) -> f64 {
        let b = self.gpu_busy[dev];
        (b + b * 0.25 * wave(t, 37.0, self.phase + dev as f64)).clamp(0.0, 100.0)
    }

    fn pids(&self, idx: usize) -> Vec<i32> {
        (0..self.procs).map(|j| 41_000 + idx as i32 * 211 + j as i32 * 7).collect()
    }
}

/// MB to KiB, the unit snapshots use.
fn mb(v: f64) -> u64 {
    (v.max(0.0) * MIB as f64) as u64
}

/// Least-squares slope (MB/h) and r² over (hours, MB) points.
fn fit(pts: &[(f64, f64)]) -> (f64, f64) {
    let n = pts.len() as f64;
    let mx = pts.iter().map(|p| p.0).sum::<f64>() / n;
    let my = pts.iter().map(|p| p.1).sum::<f64>() / n;
    let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
    let syy: f64 = pts.iter().map(|p| (p.1 - my).powi(2)).sum();
    if sxx == 0.0 {
        return (0.0, 0.0);
    }
    (sxy / sxx, if syy > 0.0 { sxy * sxy / (sxx * syy) } else { 0.0 })
}

pub struct Demo {
    start: Instant,
    cast: Vec<Sim>,
}

pub fn policy() -> OomdPolicy {
    #[cfg(not(target_os = "macos"))]
    return OomdPolicy { limit_pct: 50.0, duration_s: 20 };
    #[cfg(target_os = "macos")]
    return OomdPolicy { limit_pct: 100.0 };
}

/// Two sample entries for the Linux "Killed for memory" list.
pub fn kills() -> Vec<Kill> {
    vec![
        Kill {
            when: "9 Nov 16:42".into(),
            sort: "2026-11-09T16:42:07+01:00".into(),
            name: "Factorio".into(),
            how: "oomd",
            detail: "pressure 71%, limit 50%".into(),
        },
        Kill {
            when: "6 Nov 11:05".into(),
            sort: "2026-11-06T11:05:31+01:00".into(),
            name: "node".into(),
            how: "kernel",
            detail: format!("out of memory, was using {}", fmt_kib(6 * GIB + 300 * MIB)),
        },
    ]
}

impl Demo {
    pub fn new() -> Self {
        Self { start: Instant::now(), cast: cast() }
    }

    fn ticks(&self) -> u64 {
        (self.start.elapsed().as_secs_f64() / TICK) as u64
    }

    pub fn snapshot(&self) -> Snapshot {
        self.at(self.ticks())
    }

    /// The machine after `tick` refreshes.
    pub fn at(&self, tick: u64) -> Snapshot {
        let t = HEAD_START + tick as f64 * TICK;
        let ndev = if cfg!(target_os = "macos") { 1 } else { 2 };

        // --- per app memory, with an hour of 30 s history behind it ---
        let mut apps: Vec<App> = self
            .cast
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let pts: Vec<(f64, f64)> = (0..120).map(|k| {
                    let at = t - (119 - k) as f64 * 30.0;
                    ((at - (t - 3570.0)) / 3600.0, s.ram_at(at) + s.swap + s.gpu_mem)
                }).collect();
                let (slope, r2) = fit(&pts);
                let extra = if cfg!(target_os = "macos") { 0.0 } else { s.swap + s.gpu_mem };
                let spark = (0..crate::collect::SPARK_POINTS)
                    .map(|k| mb(s.ram_at(t - (crate::collect::SPARK_POINTS - 1 - k) as f64 * 30.0) + extra))
                    .collect();
                let mut a = App {
                    name: s.name.into(),
                    pids: s.pids(i),
                    pss: mb(s.ram_at(t)),
                    age_s: (s.age_s + t - HEAD_START) as u64,
                    growth_mb_h: Some(slope as f32),
                    growth_r2: r2 as f32,
                    span_min: 59.5,
                    gained_mb: (slope * 59.5 / 60.0) as f32,
                    spark,
                    ..Default::default()
                };
                if !cfg!(target_os = "macos") {
                    a.swap = mb(s.swap);
                    a.gpu = mb(s.gpu_mem);
                }
                a
            })
            .collect();
        apps.sort_by(|a, b| b.total().cmp(&a.total()));
        let app_kib: u64 = apps.iter().map(|a| a.pss).sum();

        // --- system memory, KiB ---
        let drift = |p: f64, amp: f64| (amp * wave(t, p, 1.3)) as i64;
        let anon = app_kib + GIB;
        let cache = (8 * GIB + GIB / 2).saturating_add_signed(drift(900.0, 300.0 * MIB as f64));
        let mut mem = MemInfo { total: TOTAL, anon, file_cache: cache, ..Default::default() };
        if cfg!(target_os = "macos") {
            mem.shmem = (4 * GIB + 300 * MIB).saturating_add_signed(drift(700.0, 150.0 * MIB as f64));
            mem.kernel = 3 * GIB + 400 * MIB;
            mem.swap_total = 4 * GIB;
            mem.swap_used = 2 * GIB + 900 * MIB;
        } else {
            mem.shmem = GIB + 700 * MIB;
            mem.kernel = GIB + 500 * MIB;
            mem.swap_total = 8 * GIB;
            mem.swap_used = (2 * GIB + 300 * MIB).saturating_add_signed(drift(1300.0, 80.0 * MIB as f64));
        }
        mem.free = TOTAL.saturating_sub(mem.anon + mem.shmem + mem.kernel + mem.file_cache);
        mem.available = if cfg!(target_os = "macos") {
            mem.file_cache + mem.free
        } else {
            mem.free + mem.file_cache * 85 / 100
        };

        // --- CPU ---
        let core_at = |c: usize, at: f64| -> f64 {
            // the game keeps two cores warm; the rest idle along
            let base = match c {
                0 => 72.0,
                3 => 64.0,
                _ => 9.0 + c as f64 * 1.5,
            };
            (base + base * 0.35 * wave(at, 17.0 + c as f64 * 3.0, c as f64)).clamp(0.0, 100.0)
        };
        let total_at = |at: f64| (0..CORES).map(|c| core_at(c, at)).sum::<f64>() / CORES as f64;
        let history: Vec<f32> =
            (0..crate::cpu::HISTORY_POINTS).map(|k| total_at(t - (crate::cpu::HISTORY_POINTS - 1 - k) as f64 * TICK) as f32).collect();
        let mut cpu_apps: Vec<CpuApp> = self
            .cast
            .iter()
            .enumerate()
            .map(|(i, s)| CpuApp {
                name: s.name.into(),
                pids: s.pids(i),
                pct: s.cpu_at(t) as f32,
                spark: (0..60).map(|k| s.cpu_at(t - (59 - k) as f64 * TICK) as f32).collect(),
                cpu_secs: (s.cpu / 100.0 * (s.age_s + t - HEAD_START)) as u64,
                threads: s.threads,
                age_s: (s.age_s + t - HEAD_START) as u64,
            })
            .collect();
        cpu_apps.sort_by(|a, b| b.pct.total_cmp(&a.pct).then(b.cpu_secs.cmp(&a.cpu_secs)));
        let mut cpu = CpuSnap {
            total: total_at(t) as f32,
            cores: (0..CORES).map(|c| core_at(c, t) as f32).collect(),
            load: [
                (3.1 + 0.4 * wave(t, 300.0, 0.2)) as f32,
                (2.8 + 0.2 * wave(t, 900.0, 0.5)) as f32,
                (2.6 + 0.1 * wave(t, 2700.0, 0.9)) as f32,
            ],
            history,
            pushes: tick + 1,
            apps: cpu_apps,
            ..Default::default()
        };
        if !cfg!(target_os = "macos") {
            cpu.psi_some10 = (4.0 + 2.0 * wave(t, 60.0, 0.4)) as f32;
            cpu.freq_mhz = (3100.0 + 400.0 * wave(t, 29.0, 0.1)) as u32;
            cpu.max_mhz = 4800;
            cpu.temp_c = Some((66.0 + 4.0 * wave(t, 90.0, 0.7)) as f32);
        }

        // --- GPU ---
        // macOS has one GPU doing the work of both Linux ones
        let busy_on = |s: &Sim, at: f64, d: usize| if ndev == 1 { (s.gpu_at(at, 0) + s.gpu_at(at, 1)) * 0.6 } else { s.gpu_at(at, d) };
        let dev_at = |d: usize, at: f64| -> f64 { (self.cast.iter().map(|s| busy_on(s, at, d)).sum::<f64>() + 1.5).min(100.0) };
        let mut devices: Vec<GpuDevice> = (0..ndev)
            .map(|d| GpuDevice {
                pdev: format!("demo{d}"),
                busy: dev_at(d, t) as f32,
                history: (0..crate::gpu::HISTORY_POINTS)
                    .map(|k| dev_at(d, t - (crate::gpu::HISTORY_POINTS - 1 - k) as f64 * TICK) as f32)
                    .collect(),
                pushes: tick + 1,
                ..Default::default()
            })
            .collect();
        let eng = |name: &str, busy: f64| GpuEngine { name: name.into(), busy: busy.clamp(0.0, 100.0) as f32 };
        if cfg!(target_os = "macos") {
            let d = &mut devices[0];
            d.name = "Apple GPU (10-core)".into();
            d.engines = vec![eng("Renderer", d.busy as f64 * 0.97), eng("Tiler", d.busy as f64 * 0.88)];
            d.mem_used = Some(2 * GIB + 200 * MIB);
        } else {
            let d = &mut devices[0];
            d.name = "Intel GPU".into();
            d.engines = vec![eng("Render", d.busy as f64), eng("Video", 3.0), eng("Copy", 1.0)];
            d.freq_mhz = Some((850.0 + 200.0 * wave(t, 41.0, 0.3)) as u32);
            d.max_mhz = Some(1450);
            let d = &mut devices[1];
            d.name = "NVIDIA GeForce RTX 4060".into();
            d.engines = vec![eng("Graphics and compute", d.busy as f64), eng("Memory controller", d.busy as f64 * 0.6)];
            d.mem_used = Some(3 * GIB + 100 * MIB);
            d.mem_total = Some(8 * GIB);
            d.temp_c = Some((71.0 + 3.0 * wave(t, 120.0, 0.2)) as f32);
            d.power_w = Some((64.0 + 9.0 * wave(t, 33.0, 0.8)) as f32);
            d.freq_mhz = Some(2100);
            d.max_mhz = Some(2460);
        }
        // per-app GPU use is a Linux view; macOS has no public source for it
        let mut gpu_apps: Vec<GpuApp> = Vec::new();
        if !cfg!(target_os = "macos") {
            gpu_apps = self
                .cast
                .iter()
                .enumerate()
                .filter(|(_, s)| s.gpu_mem > 0.0 || s.gpu_busy.iter().any(|b| *b > 0.0))
                .map(|(i, s)| {
                    let busy: Vec<f32> = (0..ndev).map(|d| s.gpu_at(t, d) as f32).collect();
                    let max_busy = busy.iter().copied().fold(0.0, f32::max);
                    GpuApp {
                        name: s.name.into(),
                        pids: s.pids(i),
                        busy,
                        max_busy,
                        mem: mb(s.gpu_mem),
                        spark: (0..60)
                            .map(|k| (0..ndev).map(|d| s.gpu_at(t - (59 - k) as f64 * TICK, d)).fold(0.0, f64::max) as f32)
                            .collect(),
                        age_s: (s.age_s + t - HEAD_START) as u64,
                    }
                })
                .collect();
            gpu_apps.sort_by(|a, b| b.max_busy.total_cmp(&a.max_busy).then(b.mem.cmp(&a.mem)));
        }
        let gpu = GpuSnap { devices, apps: gpu_apps, measuring: false };

        let mut s = Snapshot { mem, apps, took: Duration::from_millis(3), cpu, gpu, ..Default::default() };
        #[cfg(not(target_os = "macos"))]
        {
            use crate::collect::Psi;
            // amber: above 40% of oomd's limit, below where it would warn
            let p = (27.0 + 1.5 * wave(t, 150.0, 0.6)) as f32;
            s.user_psi = Psi { some10: p + 9.0, full10: p, full60: p - 2.0 };
            s.psi = Psi { some10: p + 4.0, full10: p - 5.0, full60: p - 6.0 };
            s.tmpfs = 380 * MIB;
            s.gpu_total = s.apps.iter().map(|a| a.gpu).sum();
            s.board = crate::sensors::Board {
                ram_area_c: Some(46.0),
                gpu_area_c: Some(58.0),
                cpu_fan_rpm: Some(3240),
                gpu_fan_rpm: Some(3910),
            };
        }
        #[cfg(target_os = "macos")]
        {
            s.pressure = crate::collect::Pressure { level: 2, free_pct: 38 };
        }
        s
    }
}

/// `headroom --demo --dump`: the simulated machine at its first tick.
pub fn dump() {
    let s = Demo::new().at(0);
    let m = &s.mem;
    println!(
        "RAM {} used of {} ({} available) · swap {} / {}",
        fmt_kib(m.used()),
        fmt_kib(m.total),
        fmt_kib(m.available),
        fmt_kib(m.swap_used),
        fmt_kib(m.swap_total)
    );
    if cfg!(target_os = "macos") {
        println!(
            "apps {} · compressed {} · wired and system {} · cache {} · free {}",
            fmt_kib(m.anon),
            fmt_kib(m.shmem),
            fmt_kib(m.kernel),
            fmt_kib(m.file_cache),
            fmt_kib(m.free)
        );
    } else {
        println!(
            "apps {} · GPU&shared {} · /tmp {} · kernel {} · cache {} · free {}",
            fmt_kib(m.anon),
            fmt_kib(m.shmem.saturating_sub(s.tmpfs)),
            fmt_kib(s.tmpfs.min(m.shmem)),
            fmt_kib(m.kernel),
            fmt_kib(m.file_cache),
            fmt_kib(m.free)
        );
    }
    #[cfg(not(target_os = "macos"))]
    println!(
        "pressure: system full10 {:.1}% · your session full10 {:.1}% (oomd limit {:.0}%)",
        s.psi.full10,
        s.user_psi.full10,
        policy().limit_pct
    );
    #[cfg(target_os = "macos")]
    println!("memory pressure: {} (level {})", s.pressure.label(), s.pressure.level);
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
        println!("GPU {}: {:.0}% busy · {} in use", d.name, d.busy, d.mem_used.map(fmt_kib).unwrap_or_else(|| "-".into()));
    }
    if cfg!(target_os = "macos") {
        println!("\n{:>9} {:>8} {:>8}  APP", "RAM", "MB/h", "UP");
    } else {
        println!("\n{:>9} {:>9} {:>9} {:>8} {:>8}  APP", "RAM", "SWAP", "GPU", "MB/h", "UP");
    }
    for a in s.apps.iter().take(15) {
        let growth = a.growth_mb_h.map(|g| format!("{g:+.0}")).unwrap_or_default();
        if cfg!(target_os = "macos") {
            println!("{:>9} {:>8} {:>8}  {} ({} procs)", fmt_kib(a.pss), growth, fmt_age(a.age_s), a.name, a.pids.len());
            continue;
        }
        println!(
            "{:>9} {:>9} {:>9} {:>8} {:>8}  {} ({} procs)",
            fmt_kib(a.pss),
            fmt_kib(a.swap),
            fmt_kib(a.gpu),
            growth,
            fmt_age(a.age_s),
            a.name,
            a.pids.len()
        );
    }
    println!("\n{:>7} {:>9} {:>7}  APP", "CPU", "CPU TIME", "THREADS");
    for a in s.cpu.apps.iter().take(8) {
        println!("{:>6.1}% {:>8}s {:>7}  {}", a.pct, a.cpu_secs, a.threads, a.name);
    }
    println!("\nWhy:");
    for i in crate::collect::insights(&s, policy()) {
        println!("  [{:?}] {}", i.level, i.text);
    }
    if !cfg!(target_os = "macos") {
        println!("\nKilled for memory (7 days):");
        for k in kills() {
            println!("  {:<14} {:<7} {:<16} {}", k.when, k.how, k.name, k.detail);
        }
    }
    println!("\n(simulated machine; nothing was read from this computer)");
}
