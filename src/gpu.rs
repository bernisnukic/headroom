//! GPU: per-device and per-app busy time.
//!
//! - Intel (i915/xe) and other DRM drivers: `drm-engine-<name>: N ns` in each
//!   client's /proc/PID/fdinfo. Busy % = engine time delta / wall time /
//!   engine capacity. Clock from gt_act_freq_mhz (gt_cur_freq_mhz while idle).
//! - NVIDIA (proprietary driver, no engine counters in fdinfo): NVML. Only
//!   queried while the card's runtime PM status is "active": talking to a
//!   suspended dGPU wakes it and costs battery.
//!
//! Measured only while the CPU or GPU tab is visible.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::time::Instant;

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::enums::device::UsedGpuMemory;

use crate::collect::{GIB, Insight, Level, fmt_kib};

pub const HISTORY_POINTS: usize = 90; // 3 min at 2 s
pub const APP_POINTS: usize = 60; // 2 min at 2 s

#[derive(Clone, Default, Debug)]
pub struct GpuEngine {
    pub name: String,
    pub busy: f32,
}

#[derive(Clone, Default, Debug)]
pub struct GpuDevice {
    pub name: String,
    /// PCI address, used as a stable key.
    pub pdev: String,
    /// Busiest engine, percent.
    pub busy: f32,
    pub engines: Vec<GpuEngine>,
    pub freq_mhz: Option<u32>,
    pub max_mhz: Option<u32>,
    /// KiB; dedicated VRAM (NVIDIA) or None for shared-memory iGPUs.
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    pub temp_c: Option<f32>,
    pub power_w: Option<f32>,
    /// Runtime-suspended: not queried, drawing (almost) no power.
    pub asleep: bool,
    pub history: Vec<f32>,
    pub pushes: u64,
}

#[derive(Clone, Default, Debug)]
pub struct GpuApp {
    pub name: String,
    pub pids: Vec<i32>,
    /// Busy % per device, same order as `GpuSnap::devices`.
    pub busy: Vec<f32>,
    /// The highest of `busy`.
    pub max_busy: f32,
    /// KiB of graphics memory (i915 GEM objects + NVIDIA allocations).
    pub mem: u64,
    pub spark: Vec<f32>,
    pub age_s: u64,
}

#[derive(Clone, Default, Debug)]
pub struct GpuSnap {
    pub devices: Vec<GpuDevice>,
    pub apps: Vec<GpuApp>,
    pub measuring: bool,
}

impl GpuSnap {
    /// Busy % of `app` on its busiest device, for the CPU tab's GPU column.
    pub fn app_busy(&self, name: &str) -> Option<f32> {
        self.apps.iter().find(|a| a.name == name).map(|a| a.max_busy)
    }
}

pub struct GpuCollector {
    /// (pdev/client-id, engine) -> ns at the previous sample.
    prev: HashMap<(String, String), u64>,
    prev_at: Option<Instant>,
    history: HashMap<String, VecDeque<f32>>,
    pushes: u64,
    app_hist: HashMap<String, VecDeque<f32>>,
    nvml: Option<Nvml>,
    nvml_failed: bool,
    nv_last_ts: u64,
}

struct Client {
    pid: i32,
    driver: String,
    pdev: String,
    key: String,
    engines: Vec<(String, u64, u32)>, // name, ns, capacity
}

fn read_clients(fds: &[(i32, String)], names: &HashMap<i32, &str>) -> Vec<Client> {
    let mut by_key: HashMap<String, Client> = HashMap::new();
    for (pid, fd) in fds {
        let Ok(info) = fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")) else { continue };
        let (mut driver, mut pdev, mut id) = (String::new(), String::new(), String::new());
        let mut ns: Vec<(String, u64)> = Vec::new();
        let mut caps: HashMap<String, u32> = HashMap::new();
        for l in info.lines() {
            let Some((k, v)) = l.split_once(':') else { continue };
            let v = v.trim();
            match k {
                "drm-driver" => driver = v.into(),
                "drm-pdev" => pdev = v.into(),
                "drm-client-id" => id = v.into(),
                _ => {
                    if let Some(e) = k.strip_prefix("drm-engine-capacity-") {
                        caps.insert(e.into(), v.parse().unwrap_or(1));
                    } else if let Some(e) = k.strip_prefix("drm-engine-") {
                        if let Some(n) = v.strip_suffix(" ns").and_then(|n| n.trim().parse().ok()) {
                            ns.push((e.into(), n));
                        }
                    }
                }
            }
        }
        if id.is_empty() || ns.is_empty() {
            continue;
        }
        let key = format!("{pdev}/{id}");
        let is_systemd = names.get(pid).is_some_and(|n| *n == "systemd");
        let engines = ns.into_iter().map(|(e, n)| {
            let cap = caps.get(&e).copied().unwrap_or(1).max(1);
            (e, n, cap)
        });
        match by_key.get_mut(&key) {
            // the same client is often open in several fds; keep one, and
            // prefer a holder that isn't systemd's fd store
            Some(c) if names.get(&c.pid).is_some_and(|n| *n == "systemd") && !is_systemd => c.pid = *pid,
            Some(_) => {}
            None => {
                by_key.insert(key.clone(), Client { pid: *pid, driver, pdev, key, engines: engines.collect() });
            }
        }
    }
    by_key.into_values().collect()
}

fn pretty_engine(e: &str) -> String {
    match e {
        "render" => "Render".into(),
        "copy" => "Copy".into(),
        "video" => "Video".into(),
        "video-enhance" => "Video enhance".into(),
        "compute" => "Compute".into(),
        other => other.to_string(),
    }
}

/// /sys/class/drm/cardN for a PCI address.
fn card_for(pdev: &str) -> Option<String> {
    for e in fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if !n.starts_with("card") || n.contains('-') {
            continue;
        }
        let dev = fs::read_link(e.path().join("device")).ok()?;
        if dev.file_name().is_some_and(|f| f.to_string_lossy() == pdev) {
            return Some(e.path().to_string_lossy().to_string());
        }
    }
    None
}

fn read_u32(p: &str) -> Option<u32> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

fn device_name(driver: &str) -> String {
    match driver {
        "i915" | "xe" => "Intel GPU".into(),
        "amdgpu" => "AMD GPU".into(),
        other => format!("{other} GPU"),
    }
}

/// PCI addresses of NVIDIA devices and whether each is runtime-active.
fn nvidia_devices() -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir("/sys/bus/pci/drivers/nvidia") else { return out };
    for e in dir.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.contains(':') && n.contains('.') {
            let st = fs::read_to_string(e.path().join("power/runtime_status")).unwrap_or_default();
            out.push((n, st.trim() != "suspended"));
        }
    }
    out.sort();
    out
}

impl GpuCollector {
    pub fn new() -> Self {
        Self {
            prev: HashMap::new(),
            prev_at: None,
            history: HashMap::new(),
            pushes: 0,
            app_hist: HashMap::new(),
            nvml: None,
            nvml_failed: false,
            nv_last_ts: 0,
        }
    }

    pub fn reset(&mut self) {
        self.prev.clear();
        self.prev_at = None;
        self.app_hist.clear();
    }

    /// `fds`: every open DRM fd; `procs`: (pid, app name, age); `gem`: i915
    /// GEM KiB per pid.
    pub fn sample(&mut self, fds: &[(i32, String)], procs: &[(i32, &str, u64)], gem: &HashMap<i32, u64>) -> GpuSnap {
        let now = Instant::now();
        let dt = self.prev_at.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        self.prev_at = Some(now);
        let names: HashMap<i32, &str> = procs.iter().map(|(p, n, _)| (*p, *n)).collect();
        let ages: HashMap<&str, u64> = procs.iter().fold(HashMap::new(), |mut m, (_, n, a)| {
            let e = m.entry(*n).or_insert(0);
            *e = (*e).max(*a);
            m
        });

        // --- DRM fdinfo devices (Intel, AMD, ...) ---
        let clients = read_clients(fds, &names);
        let mut devices: Vec<GpuDevice> = Vec::new();
        let mut dev_index: HashMap<String, usize> = HashMap::new();
        // (device idx, app name) -> engine -> busy
        let mut app_eng: HashMap<(usize, String), HashMap<String, f32>> = HashMap::new();
        let mut next_prev = HashMap::new();
        for c in &clients {
            let di = *dev_index.entry(c.pdev.clone()).or_insert_with(|| {
                let card = card_for(&c.pdev);
                // act reads 0 while the GT is in RC6 between frames; cur is the requested clock.
                let freq = card.as_ref().and_then(|cd| {
                    read_u32(&format!("{cd}/gt_act_freq_mhz"))
                        .filter(|&f| f > 0)
                        .or_else(|| read_u32(&format!("{cd}/gt_cur_freq_mhz")))
                });
                let max = card.as_ref().and_then(|cd| read_u32(&format!("{cd}/gt_RP0_freq_mhz")));
                devices.push(GpuDevice {
                    name: device_name(&c.driver),
                    pdev: c.pdev.clone(),
                    freq_mhz: freq,
                    max_mhz: max,
                    ..Default::default()
                });
                devices.len() - 1
            });
            let app = names.get(&c.pid).copied().unwrap_or("?").to_string();
            for (e, ns, cap) in &c.engines {
                let k = (c.key.clone(), e.clone());
                let busy = match self.prev.get(&k) {
                    Some(p) if dt > 0.0 => (ns.saturating_sub(*p) as f64 / (dt * 1e9 * *cap as f64) * 100.0) as f32,
                    _ => 0.0,
                };
                next_prev.insert(k, *ns);
                let d = &mut devices[di];
                match d.engines.iter_mut().find(|x| x.name == pretty_engine(e)) {
                    Some(x) => x.busy += busy,
                    None => d.engines.push(GpuEngine { name: pretty_engine(e), busy }),
                }
                *app_eng.entry((di, app.clone())).or_default().entry(e.clone()).or_insert(0.0) += busy;
            }
        }
        self.prev = next_prev;
        for d in &mut devices {
            for e in &mut d.engines {
                e.busy = e.busy.clamp(0.0, 100.0);
            }
            d.busy = d.engines.iter().map(|e| e.busy).fold(0.0, f32::max);
        }
        let mut app_busy: HashMap<(usize, String), f32> = app_eng
            .into_iter()
            .map(|(k, eng)| (k, eng.values().copied().fold(0.0, f32::max).clamp(0.0, 100.0)))
            .collect();

        // --- NVIDIA through NVML, only while awake ---
        let mut nv_mem: HashMap<i32, u64> = HashMap::new();
        for (idx, (pdev, awake)) in nvidia_devices().into_iter().enumerate() {
            let di = devices.len();
            let mut d = GpuDevice { name: "NVIDIA GPU".into(), pdev: pdev.clone(), asleep: !awake, ..Default::default() };
            if awake && self.nvml.is_none() && !self.nvml_failed {
                match Nvml::init() {
                    Ok(n) => self.nvml = Some(n),
                    Err(_) => self.nvml_failed = true,
                }
            }
            if let (true, Some(nvml)) = (awake, &self.nvml) {
                if let Ok(dev) = nvml.device_by_index(idx as u32) {
                    if let Ok(n) = dev.name() {
                        d.name = n.replace(" Laptop GPU", "");
                    }
                    if let Ok(u) = dev.utilization_rates() {
                        d.engines.push(GpuEngine { name: "Graphics and compute".into(), busy: u.gpu as f32 });
                        d.engines.push(GpuEngine { name: "Memory controller".into(), busy: u.memory as f32 });
                        d.busy = u.gpu as f32;
                    }
                    if let Ok(m) = dev.memory_info() {
                        d.mem_used = Some(m.used / 1024);
                        d.mem_total = Some(m.total / 1024);
                    }
                    d.temp_c = dev.temperature(TemperatureSensor::Gpu).ok().map(|t| t as f32);
                    d.power_w = dev.power_usage().ok().map(|mw| mw as f32 / 1000.0);
                    d.freq_mhz = dev.clock_info(Clock::Graphics).ok();
                    d.max_mhz = dev.max_clock_info(Clock::Graphics).ok();
                    if let Ok(samples) = dev.process_utilization_stats(self.nv_last_ts) {
                        for s in samples {
                            self.nv_last_ts = self.nv_last_ts.max(s.timestamp);
                            let app = names.get(&(s.pid as i32)).copied().unwrap_or("?").to_string();
                            let e = app_busy.entry((di, app)).or_insert(0.0);
                            *e = (*e + s.sm_util as f32).min(100.0);
                        }
                    }
                    for p in dev
                        .running_graphics_processes()
                        .unwrap_or_default()
                        .into_iter()
                        .chain(dev.running_compute_processes().unwrap_or_default())
                    {
                        if let UsedGpuMemory::Used(b) = p.used_gpu_memory {
                            *nv_mem.entry(p.pid as i32).or_insert(0) += b / 1024;
                        }
                    }
                }
            }
            devices.push(d);
        }

        // --- history per device ---
        self.pushes += 1;
        for d in &mut devices {
            let h = self.history.entry(d.pdev.clone()).or_default();
            if h.len() == HISTORY_POINTS {
                h.pop_front();
            }
            h.push_back(d.busy);
            d.history = h.iter().copied().collect();
            d.pushes = self.pushes;
        }

        // --- per app ---
        let mut apps: HashMap<String, GpuApp> = HashMap::new();
        for (pid, name, _) in procs {
            let mem = gem.get(pid).copied().unwrap_or(0) + nv_mem.get(pid).copied().unwrap_or(0);
            if mem > 0 {
                let a = apps.entry(name.to_string()).or_insert_with(|| GpuApp { name: name.to_string(), ..Default::default() });
                a.mem += mem;
                a.pids.push(*pid);
            }
        }
        for ((di, name), busy) in app_busy {
            let a = apps.entry(name.clone()).or_insert_with(|| GpuApp { name: name.clone(), ..Default::default() });
            if a.busy.len() < devices.len() {
                a.busy.resize(devices.len(), 0.0);
            }
            a.busy[di] += busy;
        }
        let mut apps: Vec<GpuApp> = apps
            .into_values()
            .filter(|a| a.name != "?")
            .map(|mut a| {
                a.busy.resize(devices.len(), 0.0);
                a.max_busy = a.busy.iter().copied().fold(0.0, f32::max);
                a.age_s = ages.get(a.name.as_str()).copied().unwrap_or(0);
                if a.pids.is_empty() {
                    a.pids = procs.iter().filter(|(_, n, _)| *n == a.name).map(|(p, _, _)| *p).collect();
                }
                a
            })
            .collect();
        for a in &mut apps {
            let h = self.app_hist.entry(a.name.clone()).or_default();
            if h.len() == APP_POINTS {
                h.pop_front();
            }
            h.push_back(a.max_busy);
            a.spark = h.iter().copied().collect();
        }
        self.app_hist.retain(|k, _| apps.iter().any(|a| &a.name == k));
        apps.sort_by(|a, b| b.max_busy.total_cmp(&a.max_busy).then(b.mem.cmp(&a.mem)));
        GpuSnap { devices, apps, measuring: dt == 0.0 }
    }
}

pub fn insights(s: &GpuSnap) -> Vec<Insight> {
    let mut v = Vec::new();
    for (i, d) in s.devices.iter().enumerate() {
        if d.asleep {
            continue;
        }
        if d.busy >= 80.0 {
            let top = s.apps.iter().filter(|a| a.busy.get(i).copied().unwrap_or(0.0) >= 1.0).max_by(|a, b| {
                a.busy[i].total_cmp(&b.busy[i])
            });
            let mut text = format!("The {} is {:.0}% busy.", d.name, d.busy);
            if let Some(a) = top {
                text.push_str(&format!(" {} accounts for {:.0}% of it.", a.name, a.busy[i]));
                if a.name == "gnome-shell" {
                    text.push_str(
                        " That covers everything the desktop draws, including extensions such as animated wallpapers.",
                    );
                }
            }
            v.push(Insight { level: if d.busy >= 95.0 { Level::Bad } else { Level::Warn }, text });
        }
        if d.temp_c.is_some_and(|t| t >= 85.0) {
            v.push(Insight { level: Level::Warn, text: format!("The {} is at {:.0} °C.", d.name, d.temp_c.unwrap()) });
        }
        if let (Some(u), Some(t)) = (d.mem_used, d.mem_total) {
            if u * 10 >= t * 9 && t > GIB {
                v.push(Insight {
                    level: Level::Warn,
                    text: format!("The {} has {} of {} video memory in use.", d.name, fmt_kib(u), fmt_kib(t)),
                });
            }
        }
    }
    if v.is_empty() {
        let parts: Vec<String> = s
            .devices
            .iter()
            .map(|d| if d.asleep { format!("{} asleep", d.name) } else { format!("{} {:.0}% busy", d.name, d.busy) })
            .collect();
        v.push(Insight { level: Level::Info, text: format!("Nothing needs attention. {}.", parts.join(", ")) });
    }
    v
}

/// The board's "near the GPU" sensor and the second fan belong to the
/// discrete card, never to the iGPU.
pub fn is_discrete(d: &GpuDevice) -> bool {
    d.name.starts_with("NVIDIA") || d.mem_total.is_some()
}
