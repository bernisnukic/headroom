//! CPU: whole-system and per-core usage from /proc/stat, load, CPU pressure,
//! package temperature and clock speed. Per-app usage (from /proc/PID/stat)
//! is only measured while the CPU tab is visible.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use crate::collect::{Insight, Level};

pub const HISTORY_POINTS: usize = 90; // 3 min at 2 s
pub const APP_POINTS: usize = 60; // 2 min at 2 s

#[derive(Clone, Default, Debug)]
pub struct CpuApp {
    pub name: String,
    pub pids: Vec<i32>,
    /// Percent of one core (top-style: 200% = two cores busy).
    pub pct: f32,
    pub spark: Vec<f32>,
    pub cpu_secs: u64,
    pub threads: u32,
    pub age_s: u64,
}

#[derive(Clone, Default, Debug)]
pub struct CpuSnap {
    /// Percent of the whole machine.
    pub total: f32,
    pub cores: Vec<f32>,
    pub load: [f32; 3],
    pub psi_some10: f32,
    pub freq_mhz: u32,
    pub max_mhz: u32,
    pub temp_c: Option<f32>,
    /// One value per sample, oldest first.
    pub history: Vec<f32>,
    /// Counts pushes, so the UI can tell a new bar arrived and slide the chart.
    pub pushes: u64,
    pub apps: Vec<CpuApp>,
    pub measuring_apps: bool,
}

pub struct CpuCollector {
    prev: Vec<(u64, u64)>,
    history: VecDeque<f32>,
    pushes: u64,
    prev_pid: HashMap<i32, u64>,
    prev_at: Option<Instant>,
    app_hist: HashMap<String, VecDeque<f32>>,
    temp_path: Option<PathBuf>,
    max_mhz: u32,
    tck: f32,
}

/// (busy, total) jiffies for "cpu" then each "cpuN".
fn stat_times() -> Vec<(u64, u64)> {
    let s = fs::read_to_string("/proc/stat").unwrap_or_default();
    s.lines()
        .take_while(|l| l.starts_with("cpu"))
        .map(|l| {
            let v: Vec<u64> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            let total: u64 = v.iter().take(8).sum();
            let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
            (total.saturating_sub(idle), total)
        })
        .collect()
}

fn find_temp() -> Option<PathBuf> {
    for e in fs::read_dir("/sys/class/hwmon").ok()?.flatten() {
        let name = fs::read_to_string(e.path().join("name")).unwrap_or_default();
        if matches!(name.trim(), "coretemp" | "k10temp" | "zenpower") {
            // temp1 is the package (Intel) or Tctl (AMD)
            return Some(e.path().join("temp1_input"));
        }
    }
    None
}

fn read_u64(p: &str) -> Option<u64> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

fn loadavg() -> [f32; 3] {
    let s = fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let mut it = s.split_whitespace().map(|x| x.parse().unwrap_or(0.0));
    [it.next().unwrap_or(0.0), it.next().unwrap_or(0.0), it.next().unwrap_or(0.0)]
}

fn cpu_psi_some10() -> f32 {
    fs::read_to_string("/proc/pressure/cpu")
        .ok()
        .and_then(|s| {
            s.lines()
                .next()?
                .split_whitespace()
                .find_map(|kv| kv.strip_prefix("avg10="))?
                .parse()
                .ok()
        })
        .unwrap_or(0.0)
}

fn pct(prev: (u64, u64), now: (u64, u64)) -> f32 {
    let dt = now.1.saturating_sub(prev.1);
    if dt == 0 {
        return 0.0;
    }
    (now.0.saturating_sub(prev.0) as f32 / dt as f32 * 100.0).clamp(0.0, 100.0)
}

impl CpuCollector {
    pub fn new() -> Self {
        let max_mhz = read_u64("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq").unwrap_or(0) / 1000;
        let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f32;
        Self {
            prev: stat_times(),
            history: VecDeque::with_capacity(HISTORY_POINTS),
            pushes: 0,
            prev_pid: HashMap::new(),
            prev_at: None,
            app_hist: HashMap::new(),
            temp_path: find_temp(),
            max_mhz: max_mhz as u32,
            tck,
        }
    }

    /// Whole-system numbers: one read of /proc/stat, plus clock speed and
    /// temperature from sysfs when `detail` (only needed with the window open).
    pub fn sample_system(&mut self, detail: bool) -> CpuSnap {
        let now = stat_times();
        let total = now.first().zip(self.prev.first()).map(|(n, p)| pct(*p, *n)).unwrap_or(0.0);
        let cores: Vec<f32> = now.iter().skip(1).zip(self.prev.iter().skip(1)).map(|(n, p)| pct(*p, *n)).collect();
        self.prev = now;
        if self.history.len() == HISTORY_POINTS {
            self.history.pop_front();
        }
        self.history.push_back(total);
        self.pushes += 1;


        let n = cores.len().max(1);
        let freq_sum: u64 = (0..if detail { n } else { 0 })
            .filter_map(|i| read_u64(&format!("/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq")))
            .sum();
        CpuSnap {
            total,
            cores,
            load: loadavg(),
            psi_some10: cpu_psi_some10(),
            freq_mhz: (freq_sum / n as u64 / 1000) as u32,
            max_mhz: self.max_mhz,
            temp_c: if !detail { None } else { self
                .temp_path
                .as_ref()
                .and_then(|p| fs::read_to_string(p).ok())
                .and_then(|s| s.trim().parse::<f32>().ok())
                .map(|m| m / 1000.0) },
            history: self.history.iter().copied().collect(),
            pushes: self.pushes,
            apps: Vec::new(),
            measuring_apps: false,
        }
    }

    /// Per-app CPU from /proc/PID/stat deltas. `procs` is (pid, app name, age).
    pub fn sample_apps(&mut self, snap: &mut CpuSnap, procs: &[(i32, &str, u64)]) {
        // First visit to the tab: take a baseline, wait 250 ms and measure,
        // so the list appears right away instead of after a 2 s refresh.
        if self.prev_at.is_none() {
            self.prev_pid = procs
                .iter()
                .filter_map(|&(pid, _, _)| {
                    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                    let f: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
                    Some((pid, f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?))
                })
                .collect();
            self.prev_at = Some(Instant::now());
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        let now = Instant::now();
        let dt = self.prev_at.map(|t| now.duration_since(t).as_secs_f32()).unwrap_or(0.0);
        self.prev_at = Some(now);
        let mut next: HashMap<i32, u64> = HashMap::with_capacity(procs.len());
        let mut apps: HashMap<&str, CpuApp> = HashMap::new();
        for &(pid, name, age_s) in procs {
            let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
            let Some(rest) = stat.rsplit_once(')').map(|x| x.1) else { continue };
            let f: Vec<&str> = rest.split_whitespace().collect();
            let ticks = f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
                + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            let threads: u32 = f.get(17).and_then(|v| v.parse().ok()).unwrap_or(0);
            let delta = self.prev_pid.get(&pid).map(|p| ticks.saturating_sub(*p)).unwrap_or(0);
            next.insert(pid, ticks);
            let a = apps.entry(name).or_insert_with(|| CpuApp { name: name.to_string(), ..Default::default() });
            a.pids.push(pid);
            a.threads += threads;
            a.cpu_secs += (ticks as f32 / self.tck) as u64;
            a.age_s = a.age_s.max(age_s);
            if dt > 0.0 {
                a.pct += delta as f32 / self.tck / dt * 100.0;
            }
        }
        self.prev_pid = next;
        let mut apps: Vec<CpuApp> = apps.into_values().collect();
        for a in &mut apps {
            let h = self.app_hist.entry(a.name.clone()).or_default();
            if h.len() == APP_POINTS {
                h.pop_front();
            }
            h.push_back(a.pct);
            a.spark = h.iter().copied().collect();
        }
        self.app_hist.retain(|k, _| apps.iter().any(|a| &a.name == k));
        apps.sort_by(|a, b| b.pct.total_cmp(&a.pct).then(b.cpu_secs.cmp(&a.cpu_secs)));
        snap.apps = apps;
        snap.measuring_apps = dt == 0.0;
    }

    /// Forget per-process state when the CPU tab is hidden, so the next
    /// visit doesn't compute a rate over a long gap.
    pub fn reset_apps(&mut self) {
        self.prev_pid.clear();
        self.prev_at = None;
        self.app_hist.clear();
    }
}

pub fn insights(s: &CpuSnap) -> Vec<Insight> {
    let mut v = Vec::new();
    let ncpu = s.cores.len().max(1) as f32;
    if let Some(t) = s.temp_c {
        if t >= 95.0 {
            v.push(Insight { level: Level::Bad, text: format!("The CPU is at {t:.0} °C and is throttling to protect itself.") });
        } else if t >= 85.0 {
            v.push(Insight { level: Level::Warn, text: format!("The CPU is at {t:.0} °C, close to where it starts throttling.") });
        }
    }
    if s.psi_some10 >= 40.0 {
        v.push(Insight {
            level: if s.psi_some10 >= 70.0 { Level::Bad } else { Level::Warn },
            text: format!(
                "CPU pressure is {:.0}%: something was waiting for a free core {:.0}% of the last 10 s.",
                s.psi_some10, s.psi_some10
            ),
        });
    }
    if s.load[0] > ncpu * 1.5 {
        v.push(Insight {
            level: Level::Warn,
            text: format!("Load is {:.1} on {} threads, so work is queuing up.", s.load[0], ncpu as u32),
        });
    }
    for a in s.apps.iter().take(5) {
        let recent = &a.spark[a.spark.len().saturating_sub(15)..];
        if recent.len() >= 15 && recent.iter().all(|p| *p >= 80.0) {
            let avg = recent.iter().sum::<f32>() / recent.len() as f32;
            v.push(Insight {
                level: Level::Warn,
                text: format!(
                    "{} has kept about {:.1} cores busy for the last 30 s ({:.0}% CPU).",
                    a.name,
                    avg / 100.0,
                    avg
                ),
            });
        }
    }
    if v.is_empty() {
        let temp = s.temp_c.map(|t| format!(" at {t:.0} °C")).unwrap_or_default();
        v.push(Insight { level: Level::Info, text: format!("Nothing needs attention. CPU is {:.0}% busy{temp}.", s.total) });
    }
    v
}
