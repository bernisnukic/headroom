//! macOS CPU: per-core load from host_processor_info, the load average, and
//! per-app CPU from the process CPU time that the memory scan already reads.
//! Clock speed and temperature need private interfaces on Apple Silicon, so
//! they stay empty and the UI leaves them out.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

// CpuSnap, CpuApp and the insights are shared with Linux; that file's
// /proc reader is not used here.
#[allow(dead_code)]
#[path = "../cpu.rs"]
mod shared;
use shared::APP_POINTS;
pub use shared::{CpuApp, CpuSnap, HISTORY_POINTS, insights};

/// One of the user's processes as the memory scan saw it.
pub struct ProcTime<'a> {
    pub pid: i32,
    pub name: &'a str,
    pub age_s: u64,
    /// User + system CPU time so far, ns.
    pub cpu_ns: u64,
}

pub struct CpuCollector {
    /// (busy, total) ticks per core. The kernel's counters are 32 bit and
    /// wrap, so deltas use wrapping arithmetic.
    prev: Vec<(u32, u32)>,
    history: VecDeque<f32>,
    pushes: u64,
    prev_pid: HashMap<i32, u64>,
    prev_at: Option<Instant>,
    app_hist: HashMap<String, VecDeque<f32>>,
}

fn core_ticks() -> Vec<(u32, u32)> {
    let mut ncpu: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = std::ptr::null_mut();
    let mut count: libc::mach_msg_type_number_t = 0;
    let kr = unsafe {
        libc::host_processor_info(
            crate::collect::host(),
            libc::PROCESSOR_CPU_LOAD_INFO,
            &mut ncpu,
            &mut info,
            &mut count,
        )
    };
    if kr != libc::KERN_SUCCESS || info.is_null() {
        return Vec::new();
    }
    let ticks = unsafe { std::slice::from_raw_parts(info as *const u32, count as usize) };
    let (user, system, idle, nice) = (
        libc::CPU_STATE_USER as usize,
        libc::CPU_STATE_SYSTEM as usize,
        libc::CPU_STATE_IDLE as usize,
        libc::CPU_STATE_NICE as usize,
    );
    let out = ticks
        .chunks_exact(libc::CPU_STATE_MAX as usize)
        .take(ncpu as usize)
        .map(|t| {
            let busy = t[user].wrapping_add(t[system]).wrapping_add(t[nice]);
            (busy, busy.wrapping_add(t[idle]))
        })
        .collect();
    unsafe {
        libc::vm_deallocate(
            crate::collect::task_self(),
            info as libc::vm_address_t,
            count as libc::vm_size_t * size_of::<libc::integer_t>(),
        )
    };
    out
}

fn loadavg() -> [f32; 3] {
    let mut l = [0f64; 3];
    if unsafe { libc::getloadavg(l.as_mut_ptr(), 3) } != 3 {
        return [0.0; 3];
    }
    [l[0] as f32, l[1] as f32, l[2] as f32]
}

/// Thread count from the task info. It walks the task's threads, so it is
/// only asked while the CPU tab shows the column.
fn threads(pid: i32) -> u32 {
    let mut ti: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_taskinfo>() as i32;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut ti).cast(), size) };
    if n == size { ti.pti_threadnum.max(0) as u32 } else { 0 }
}

impl CpuCollector {
    pub fn new() -> Self {
        Self {
            prev: core_ticks(),
            history: VecDeque::with_capacity(HISTORY_POINTS),
            pushes: 0,
            prev_pid: HashMap::new(),
            prev_at: None,
            app_hist: HashMap::new(),
        }
    }

    /// Whole-system numbers: one host_processor_info call and getloadavg.
    /// The flag matches the Linux collector, where it adds clock and
    /// temperature; macOS has neither.
    pub fn sample_system(&mut self, _detail: bool) -> CpuSnap {
        let now = core_ticks();
        let (mut busy, mut all) = (0u64, 0u64);
        let cores: Vec<f32> = now
            .iter()
            .zip(&self.prev)
            .map(|(n, p)| {
                let (db, dt) = (n.0.wrapping_sub(p.0), n.1.wrapping_sub(p.1));
                busy += db as u64;
                all += dt as u64;
                if dt == 0 { 0.0 } else { (db as f32 / dt as f32 * 100.0).clamp(0.0, 100.0) }
            })
            .collect();
        let total = if all == 0 { 0.0 } else { (busy as f32 / all as f32 * 100.0).clamp(0.0, 100.0) };
        self.prev = now;
        if self.history.len() == HISTORY_POINTS {
            self.history.pop_front();
        }
        self.history.push_back(total);
        self.pushes += 1;
        CpuSnap {
            total,
            cores,
            load: loadavg(),
            history: self.history.iter().copied().collect(),
            pushes: self.pushes,
            ..Default::default()
        }
    }

    /// Per-app CPU from CPU-time deltas between scans.
    pub fn sample_apps(&mut self, snap: &mut CpuSnap, procs: &[ProcTime]) {
        // First visit to the tab: take a baseline, wait 250 ms and measure,
        // so the list appears right away instead of after a 2 s refresh.
        let fresh: Option<HashMap<i32, u64>> = if self.prev_at.is_none() {
            self.prev_pid = procs.iter().map(|p| (p.pid, p.cpu_ns)).collect();
            self.prev_at = Some(Instant::now());
            std::thread::sleep(Duration::from_millis(250));
            Some(procs.iter().filter_map(|p| Some((p.pid, crate::collect::cpu_ns(p.pid)?))).collect())
        } else {
            None
        };
        let now = Instant::now();
        let dt = self.prev_at.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        self.prev_at = Some(now);
        let mut next: HashMap<i32, u64> = HashMap::with_capacity(procs.len());
        let mut apps: HashMap<&str, CpuApp> = HashMap::new();
        for p in procs {
            let ns = fresh.as_ref().and_then(|f| f.get(&p.pid).copied()).unwrap_or(p.cpu_ns);
            let delta = self.prev_pid.get(&p.pid).map(|prev| ns.saturating_sub(*prev)).unwrap_or(0);
            next.insert(p.pid, ns);
            let a = apps.entry(p.name).or_insert_with(|| CpuApp { name: p.name.to_string(), ..Default::default() });
            a.pids.push(p.pid);
            a.threads += threads(p.pid);
            a.cpu_secs += ns / 1_000_000_000;
            a.age_s = a.age_s.max(p.age_s);
            if dt > 0.0 {
                a.pct += (delta as f64 / 1e9 / dt * 100.0) as f32;
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
