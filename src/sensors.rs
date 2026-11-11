//! Board sensors from the laptop firmware: temperature near the memory slots and
//! near the dGPU, and fan speeds. On Dell laptops these come from `dell_ddv`
//! (WMI). Its readings are cached for about a second, and a fresh read costs
//! 70-80 ms, about 36 ms of it CPU, so they are read on their own thread every
//! 10 s and only while the window is open. `dell_smm` exposes the same values
//! through SMM calls, which briefly stall every core, so it is not used.
//!
//! On other machines nothing is found and every field stays empty.

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PERIOD: Duration = Duration::from_secs(10);

#[derive(Clone, Default, Debug, PartialEq)]
pub struct Board {
    /// Board sensor next to the memory slots (not on the modules themselves).
    pub ram_area_c: Option<f32>,
    /// Board sensor next to the discrete GPU. Readable while the dGPU sleeps.
    pub gpu_area_c: Option<f32>,
    pub cpu_fan_rpm: Option<u32>,
    pub gpu_fan_rpm: Option<u32>,
}

impl Board {
    pub fn is_empty(&self) -> bool {
        *self == Board::default()
    }
}

/// Which files to read, found once at startup.
#[derive(Default)]
struct Paths {
    ram: Option<PathBuf>,
    gpu: Option<PathBuf>,
    cpu_fan: Option<PathBuf>,
    gpu_fan: Option<PathBuf>,
}

fn find() -> Paths {
    let mut p = Paths::default();
    let Ok(rd) = fs::read_dir("/sys/class/hwmon") else { return p };
    for h in rd.flatten().map(|e| e.path()) {
        if fs::read_to_string(h.join("name")).map(|n| n.trim() != "dell_ddv").unwrap_or(true) {
            continue;
        }
        let Ok(files) = fs::read_dir(&h) else { continue };
        for f in files.flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix("_label") else { continue };
            let label = fs::read_to_string(f.path()).unwrap_or_default();
            let input = h.join(format!("{stem}_input"));
            match (stem.starts_with("temp"), stem.starts_with("fan"), label.trim()) {
                (true, _, "SODIMM") => p.ram = Some(input),
                (true, _, "Video") => p.gpu = Some(input),
                (_, true, "CPU Fan") => p.cpu_fan = Some(input),
                (_, true, "Video Fan") => p.gpu_fan = Some(input),
                _ => {}
            }
        }
    }
    p
}

fn num(p: &Option<PathBuf>) -> Option<i64> {
    fs::read_to_string(p.as_ref()?).ok()?.trim().parse().ok()
}

fn read(p: &Paths) -> Board {
    // millidegrees; drop implausible values (unplugged sensors read 0 or junk)
    let temp = |v: Option<i64>| v.map(|m| m as f32 / 1000.0).filter(|t| (5.0..130.0).contains(t));
    Board {
        ram_area_c: temp(num(&p.ram)),
        gpu_area_c: temp(num(&p.gpu)),
        cpu_fan_rpm: num(&p.cpu_fan).map(|r| r.max(0) as u32),
        gpu_fan_rpm: num(&p.gpu_fan).map(|r| r.max(0) as u32),
    }
}

/// Starts the reader thread. Send `true` when the window opens (reads at once,
/// then every PERIOD) and `false` when it closes (stops reading). Returns
/// `None` when the machine has no supported sensors.
pub fn spawn(out: Arc<Mutex<Board>>) -> Option<Sender<bool>> {
    let paths = find();
    if paths.ram.is_none() && paths.gpu.is_none() && paths.cpu_fan.is_none() && paths.gpu_fan.is_none() {
        return None;
    }
    let (tx, rx) = channel::<bool>();
    std::thread::Builder::new()
        .name("board-sensors".into())
        .spawn(move || {
            let mut open = false;
            loop {
                if open {
                    let b = read(&paths);
                    *out.lock().unwrap() = b;
                    match rx.recv_timeout(PERIOD) {
                        Ok(v) => open = v,
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                } else {
                    match rx.recv() {
                        Ok(v) => open = v,
                        Err(_) => return,
                    }
                }
            }
        })
        .ok()?;
    Some(tx)
}

/// One synchronous read, for `--dump`.
pub fn read_once() -> Board {
    read(&find())
}
