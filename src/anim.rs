//! 60 fps easing. Samples arrive every 250 ms (system-wide) or 2 s
//! (per-app); each frame, every displayed number eases toward its latest
//! sample with an exponential approach (time constant TAU), and rows ease
//! toward their sorted position. When everything has settled, no more frames
//! are requested, so an idle window draws nothing.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::Tab;
use crate::collect::Snapshot;

/// Time constant: about 63% of the way in 120 ms, 95% in 360 ms.
const TAU: f32 = 0.09;
pub const ROW_H: f32 = 30.0;

pub struct Tw {
    gpu_pushes: u64,
    vals: HashMap<String, f32>,
    /// CPU history push count last seen, to start the chart's slide.
    pushes: u64,
    seen: HashSet<String>,
    last: Instant,
    k: f32,
    /// True if anything is still moving after this frame.
    pub moving: bool,
}

impl Tw {
    pub fn new() -> Self {
        Self { gpu_pushes: 0, vals: HashMap::new(), pushes: 0, seen: HashSet::new(), last: Instant::now(), k: 1.0, moving: false }
    }

    fn v(&mut self, key: String, target: f32) -> f32 {
        // Stop once the rest of the move would not show: half a pixel for
        // row offsets, half a unit for %, MHz and degrees, 0.5% of byte sizes.
        self.v_tol(key, target, 0.5)
    }

    fn v_tol(&mut self, key: String, target: f32, abs: f32) -> f32 {
        let k = self.k;
        let e = self.vals.entry(key.clone()).or_insert(target);
        *e += (target - *e) * k;
        if !e.is_finite() || (target - *e).abs() <= 0.005 * target.abs() + abs {
            *e = target;
        } else {
            self.moving = true;
        }
        let out = *e;
        self.seen.insert(key);
        out
    }

    fn v64(&mut self, key: &str, target: u64) -> u64 {
        self.v(key.to_string(), target as f32).max(0.0) as u64
    }

    /// An eased copy of the snapshot, plus each visible app row's y offset.
    pub fn ease(&mut self, s: &Snapshot, tab: Tab) -> (Snapshot, HashMap<String, f32>) {
        let dt = self.last.elapsed().as_secs_f32().min(0.1);
        self.last = Instant::now();
        self.k = 1.0 - (-dt / TAU).exp();
        self.moving = false;
        // drop values nobody used last frame (apps that went away), then
        // start this frame's bookkeeping
        let seen = &self.seen;
        self.vals.retain(|k, _| seen.contains(k));
        self.seen.clear();

        let mut d = s.clone();
        let mut ys = HashMap::new();
        d.mem.anon = self.v64("m.anon", d.mem.anon);
        d.mem.shmem = self.v64("m.shmem", d.mem.shmem);
        d.mem.file_cache = self.v64("m.cache", d.mem.file_cache);
        d.mem.kernel = self.v64("m.kernel", d.mem.kernel);
        d.mem.free = self.v64("m.free", d.mem.free);
        d.mem.available = self.v64("m.avail", d.mem.available);
        d.mem.swap_used = self.v64("m.swap", d.mem.swap_used);
        d.tmpfs = self.v64("m.tmpfs", d.tmpfs);
        d.user_psi.full10 = self.v_tol("m.psi".into(), d.user_psi.full10, 0.05);
        d.cpu.total = self.v("c.total".into(), d.cpu.total);

        match tab {
            Tab::Memory => {
                for (i, a) in d.apps.iter_mut().take(20).enumerate() {
                    a.pss = self.v64(&format!("m.pss.{}", a.name), a.pss);
                    a.swap = self.v64(&format!("m.sw.{}", a.name), a.swap);
                    a.gpu = self.v64(&format!("m.gpu.{}", a.name), a.gpu);
                    let y = self.v(format!("y.m.{}", a.name), i as f32 * ROW_H);
                    ys.insert(a.name.clone(), y);
                }
            }
            Tab::Cpu => {
                let c = &mut d.cpu;
                for (i, p) in c.cores.iter_mut().enumerate() {
                    *p = self.v(format!("c.core.{i}"), *p);
                }
                // a new history bar: jump the chart's offset to one slot and
                // ease it back to zero, so the strip slides left once
                if c.pushes != self.pushes {
                    if self.pushes != 0 {
                        self.vals.insert("c.shift".into(), 1.0);
                    }
                    self.pushes = c.pushes;
                }
                c.freq_mhz = self.v("c.freq".into(), c.freq_mhz as f32) as u32;
                if let Some(t) = c.temp_c {
                    c.temp_c = Some(self.v("c.temp".into(), t));
                }
                for a in c.apps.iter_mut() {
                    a.pct = self.v(format!("c.p.{}", a.name), a.pct);
                }
                for a in d.gpu.apps.iter_mut() {
                    a.max_busy = self.v(format!("g.p.{}", a.name), a.max_busy);
                }
                // order by the eased value, so rows trade places gradually
                c.apps.sort_by(|a, b| b.pct.total_cmp(&a.pct).then(b.cpu_secs.cmp(&a.cpu_secs)));
                for (i, a) in c.apps.iter().take(20).enumerate() {
                    let y = self.v(format!("y.c.{}", a.name), i as f32 * ROW_H);
                    ys.insert(a.name.clone(), y);
                }
            }
            Tab::Gpu => {
                let g = &mut d.gpu;
                let pushes = g.devices.first().map(|x| x.pushes).unwrap_or(0);
                let new_bar = pushes != self.gpu_pushes && self.gpu_pushes != 0;
                self.gpu_pushes = pushes;
                for (i, dev) in g.devices.iter_mut().enumerate() {
                    if new_bar {
                        self.vals.insert(format!("g.shift.{i}"), 1.0);
                    }
                    dev.busy = self.v(format!("g.busy.{i}"), dev.busy);
                    for e in dev.engines.iter_mut() {
                        e.busy = self.v(format!("g.e.{i}.{}", e.name), e.busy);
                    }
                }
                for a in g.apps.iter_mut() {
                    for (i, b) in a.busy.iter_mut().enumerate() {
                        *b = self.v(format!("g.b.{i}.{}", a.name), *b);
                    }
                    a.max_busy = self.v(format!("g.p.{}", a.name), a.max_busy);
                    a.mem = self.v64(&format!("g.m.{}", a.name), a.mem);
                }
                g.apps.sort_by(|a, b| b.max_busy.total_cmp(&a.max_busy).then(b.mem.cmp(&a.mem)));
                for (i, a) in g.apps.iter().take(20).enumerate() {
                    let y = self.v(format!("y.g.{}", a.name), i as f32 * ROW_H);
                    ys.insert(a.name.clone(), y);
                }
            }
        }
        (d, ys)
    }

    /// The CPU chart's remaining slide, 1 (just arrived) to 0 (settled).
    pub fn chart_shift(&mut self) -> f32 {
        self.v_tol("c.shift".into(), 0.0, 0.02)
    }

    /// GPU history chart slide for device `i`, like `chart_shift`.
    pub fn gpu_shift(&mut self, i: usize) -> f32 {
        self.v_tol(format!("g.shift.{i}"), 0.0, 0.02)
    }

    /// Eases one free-standing value (chart ceiling).
    pub fn value(&mut self, key: &str, target: f32) -> f32 {
        self.v(key.to_string(), target)
    }
}
