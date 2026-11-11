//! macOS GPU: utilisation and memory from the driver's IOAccelerator entry
//! in the IORegistry ("PerformanceStatistics"). No root needed. Per-app GPU
//! use and the GPU's clock, temperature and power are only exposed through
//! private interfaces, so they are left out.
//!
//! Measured only while the GPU tab is visible.

use std::collections::{HashMap, VecDeque};
use std::ffi::{CString, c_char};

use core_foundation::base::{CFAllocatorRef, CFType, CFTypeRef, TCFType, kCFAllocatorDefault};
use core_foundation::data::CFData;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};

use crate::collect::{GIB, Insight, Level, fmt_kib};

pub const HISTORY_POINTS: usize = 90; // 3 min at 2 s

#[derive(Clone, Default, Debug)]
pub struct GpuEngine {
    pub name: String,
    pub busy: f32,
}

#[derive(Clone, Default, Debug)]
pub struct GpuDevice {
    pub name: String,
    /// Stable key for the history.
    pub pdev: String,
    pub busy: f32,
    pub engines: Vec<GpuEngine>,
    pub freq_mhz: Option<u32>,
    pub max_mhz: Option<u32>,
    /// KiB. Apple GPUs share system memory: `mem_used` is what the driver
    /// has in use and there is no separate total.
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    pub temp_c: Option<f32>,
    pub power_w: Option<f32>,
    pub asleep: bool,
    pub history: Vec<f32>,
    pub pushes: u64,
}

/// Per-app GPU use. Not available on macOS; the type is shared with the UI.
#[allow(dead_code)]
#[derive(Clone, Default, Debug)]
pub struct GpuApp {
    pub name: String,
    pub pids: Vec<i32>,
    pub busy: Vec<f32>,
    pub max_busy: f32,
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

type IoObject = libc::mach_port_t;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFDictionaryRef;
    fn IOServiceGetMatchingServices(main_port: libc::mach_port_t, matching: CFDictionaryRef, existing: *mut IoObject) -> i32;
    fn IOIteratorNext(iterator: IoObject) -> IoObject;
    fn IOObjectRelease(object: IoObject) -> i32;
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: CFStringRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> CFTypeRef;
}

fn property(entry: IoObject, key: &'static str) -> Option<CFType> {
    let key = CFString::from_static_string(key);
    let r = unsafe { IORegistryEntryCreateCFProperty(entry, key.as_concrete_TypeRef(), kCFAllocatorDefault, 0) };
    (!r.is_null()).then(|| unsafe { CFType::wrap_under_create_rule(r) })
}

fn as_i64(t: &CFType) -> Option<i64> {
    t.downcast::<CFNumber>()?.to_i64()
}

fn as_string(t: &CFType) -> Option<String> {
    if let Some(s) = t.downcast::<CFString>() {
        return Some(s.to_string());
    }
    let d = t.downcast::<CFData>()?;
    Some(String::from_utf8_lossy(d.bytes()).trim_end_matches('\0').to_string())
}

/// Every IOAccelerator service (one per GPU), retained until the collector
/// is dropped.
fn accelerators() -> Vec<IoObject> {
    let mut out = Vec::new();
    let class = CString::new("IOAccelerator").unwrap();
    let matching = unsafe { IOServiceMatching(class.as_ptr()) };
    if matching.is_null() {
        return out;
    }
    let mut it: IoObject = 0;
    // consumes `matching`
    if unsafe { IOServiceGetMatchingServices(0, matching, &mut it) } != 0 {
        return out;
    }
    loop {
        let s = unsafe { IOIteratorNext(it) };
        if s == 0 {
            break;
        }
        out.push(s);
    }
    unsafe { IOObjectRelease(it) };
    out
}

struct Accel {
    entry: IoObject,
    name: String,
    key: String,
}

pub struct GpuCollector {
    accels: Option<Vec<Accel>>,
    history: HashMap<String, VecDeque<f32>>,
    pushes: u64,
}

impl Drop for GpuCollector {
    fn drop(&mut self) {
        for a in self.accels.iter().flatten() {
            unsafe { IOObjectRelease(a.entry) };
        }
    }
}

impl GpuCollector {
    pub fn new() -> Self {
        Self { accels: None, history: HashMap::new(), pushes: 0 }
    }

    pub fn reset(&mut self) {}

    /// Reads each GPU's statistics. The registry lookup runs once; after
    /// that each sample is one property read per GPU.
    pub fn sample(&mut self) -> GpuSnap {
        let accels = self.accels.get_or_insert_with(|| {
            accelerators()
                .into_iter()
                .enumerate()
                .map(|(i, entry)| {
                    let model = property(entry, "model").and_then(|t| as_string(&t));
                    let cores = property(entry, "gpu-core-count").and_then(|t| as_i64(&t));
                    let name = match (model, cores) {
                        (Some(m), Some(c)) => format!("{m} GPU ({c}-core)"),
                        (Some(m), None) => format!("{m} GPU"),
                        _ => "GPU".to_string(),
                    };
                    Accel { entry, name, key: format!("accel{i}") }
                })
                .collect()
        });
        let mut devices = Vec::with_capacity(accels.len());
        for a in accels.iter() {
            let mut d = GpuDevice { name: a.name.clone(), pdev: a.key.clone(), ..Default::default() };
            let stats = property(a.entry, "PerformanceStatistics")
                .filter(|t| t.instance_of::<CFDictionary>())
                .map(|t| unsafe { CFDictionary::<CFString, CFType>::wrap_under_get_rule(t.as_CFTypeRef() as CFDictionaryRef) });
            if let Some(stats) = stats {
                let get = |k: &'static str| stats.find(CFString::from_static_string(k)).and_then(|v| as_i64(&v));
                d.busy = get("Device Utilization %").unwrap_or(0).clamp(0, 100) as f32;
                for (label, key) in [("Renderer", "Renderer Utilization %"), ("Tiler", "Tiler Utilization %")] {
                    if let Some(v) = get(key) {
                        d.engines.push(GpuEngine { name: label.into(), busy: v.clamp(0, 100) as f32 });
                    }
                }
                d.mem_used = get("In use system memory").map(|b| b.max(0) as u64 / 1024);
            }
            devices.push(d);
        }
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
        GpuSnap { devices, apps: Vec::new(), measuring: false }
    }
}

pub fn insights(s: &GpuSnap) -> Vec<Insight> {
    let mut v = Vec::new();
    for d in &s.devices {
        if d.busy >= 80.0 {
            v.push(Insight {
                level: if d.busy >= 95.0 { Level::Bad } else { Level::Warn },
                text: format!("The {} is {:.0}% busy.", d.name, d.busy),
            });
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
        let parts: Vec<String> = s.devices.iter().map(|d| format!("{} {:.0}% busy", d.name, d.busy)).collect();
        v.push(Insight { level: Level::Info, text: format!("Nothing needs attention. {}.", parts.join(", ")) });
    }
    v
}

/// Board sensors near a discrete card are a Linux laptop feature.
pub fn is_discrete(_d: &GpuDevice) -> bool {
    false
}
