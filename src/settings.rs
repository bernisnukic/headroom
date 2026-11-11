//! User settings: which values the top bar shows, and whether Headroom starts
//! at login. Stored as `key=value` lines in $XDG_CONFIG_HOME/headroom/settings
//! (or ~/.config/headroom/settings). A missing or unreadable file means
//! defaults. Demo mode never calls load, save or the autostart functions.
//!
//! Start at login is not stored here: it is on exactly when the autostart
//! entry exists, so removing that file by hand turns it off.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarItem {
    Ram,
    Cpu,
    Gpu,
}

impl BarItem {
    pub const ALL: [BarItem; 3] = [BarItem::Ram, BarItem::Cpu, BarItem::Gpu];

    pub fn label(self) -> &'static str {
        match self {
            BarItem::Ram => "RAM",
            BarItem::Cpu => "CPU",
            BarItem::Gpu => "GPU",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub bar_ram: bool,
    pub bar_cpu: bool,
    pub bar_gpu: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { bar_ram: true, bar_cpu: false, bar_gpu: false }
    }
}

impl Settings {
    pub fn shows(&self, item: BarItem) -> bool {
        match item {
            BarItem::Ram => self.bar_ram,
            BarItem::Cpu => self.bar_cpu,
            BarItem::Gpu => self.bar_gpu,
        }
    }

    fn count(&self) -> usize {
        BarItem::ALL.iter().filter(|i| self.shows(**i)).count()
    }

    /// Whether `item` may be switched off: one value always stays shown.
    pub fn can_toggle(&self, item: BarItem) -> bool {
        !self.shows(item) || self.count() > 1
    }

    /// Flips `item`. Returns false (and changes nothing) for the last one on.
    pub fn toggle(&mut self, item: BarItem) -> bool {
        if !self.can_toggle(item) {
            return false;
        }
        let v = match item {
            BarItem::Ram => &mut self.bar_ram,
            BarItem::Cpu => &mut self.bar_cpu,
            BarItem::Gpu => &mut self.bar_gpu,
        };
        *v = !*v;
        true
    }

    fn parse(text: &str) -> Self {
        let mut s = Settings::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = match v.trim() {
                "true" => true,
                "false" => false,
                _ => continue,
            };
            match k.trim() {
                "bar_ram" => s.bar_ram = v,
                "bar_cpu" => s.bar_cpu = v,
                "bar_gpu" => s.bar_gpu = v,
                _ => {}
            }
        }
        if s.count() == 0 { Settings::default() } else { s }
    }

    fn to_text(self) -> String {
        format!("bar_ram={}\nbar_cpu={}\nbar_gpu={}\n", self.bar_ram, self.bar_cpu, self.bar_gpu)
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

fn config_dir() -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        Some(d) if d.is_absolute() => Some(d),
        _ => Some(home()?.join(".config")),
    }
}

pub fn path() -> Option<PathBuf> {
    Some(config_dir()?.join("headroom").join("settings"))
}

pub fn load() -> Settings {
    path().and_then(|p| std::fs::read_to_string(p).ok()).map(|t| Settings::parse(&t)).unwrap_or_default()
}

/// Writes to a temporary file first, so a crash never leaves half a file.
pub fn save(s: &Settings) -> io::Result<()> {
    let p = path().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?;
    write_atomic(&p, &s.to_text())
}

fn write_atomic(p: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("new");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, p)
}

/// The same entry install.sh writes for `--autostart`.
#[cfg(not(target_os = "macos"))]
fn autostart_file(home: &Path) -> PathBuf {
    home.join(".config/autostart/headroom.desktop")
}

#[cfg(not(target_os = "macos"))]
fn autostart_text(home: &Path) -> Option<String> {
    Some(format!(
        "[Desktop Entry]
Type=Application
Name=Headroom
Comment=How much room is left before Linux starts killing apps: memory, CPU and GPU
Exec={}/.local/bin/headroom --tray
Icon=utilities-system-monitor
Terminal=false
Categories=System;Monitor;
StartupWMClass=headroom
Keywords=memory;ram;cpu;gpu;oom;swap;
",
        home.display()
    ))
}

/// A per-user launch agent. launchd reads it at the next login, so nothing
/// is loaded or unloaded when the setting changes.
#[cfg(target_os = "macos")]
fn autostart_file(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents/com.bernisnukic.headroom.plist")
}

#[cfg(target_os = "macos")]
fn autostart_text(_home: &Path) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let exe = exe.to_str()?.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    Some(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.bernisnukic.headroom</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>--tray</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
</dict>
</plist>
"#
    ))
}

pub fn autostart_enabled() -> bool {
    home().is_some_and(|h| autostart_file(&h).exists())
}

pub fn set_autostart(on: bool) -> io::Result<()> {
    let home = home().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?;
    set_autostart_in(&home, on)
}

fn set_autostart_in(home: &Path, on: bool) -> io::Result<()> {
    let p = autostart_file(home);
    if on {
        let text = autostart_text(home).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no executable path"))?;
        write_atomic(&p, &text)
    } else {
        match std::fs::remove_file(&p) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_parsing() {
        assert_eq!(Settings::parse(""), Settings::default());
        // nothing shown is not allowed, so it falls back to the default
        assert_eq!(Settings::parse("bar_ram=false\nbar_cpu=false\nbar_gpu=false\n"), Settings::default());
        assert_eq!(Settings::parse("garbage\n=\nbar_cpu=maybe"), Settings::default());
        let s = Settings { bar_ram: false, bar_cpu: true, bar_gpu: true };
        assert_eq!(Settings::parse(&s.to_text()), s);
    }

    #[test]
    fn last_value_stays_on() {
        let mut s = Settings::default();
        assert!(!s.toggle(BarItem::Ram));
        assert!(s.bar_ram);
        assert!(s.toggle(BarItem::Cpu) && s.toggle(BarItem::Gpu));
        assert!(s.toggle(BarItem::Ram) && !s.bar_ram);
        assert!(s.toggle(BarItem::Cpu));
        assert!(!s.toggle(BarItem::Gpu), "GPU is the last one on");
        assert_eq!(s, Settings { bar_ram: false, bar_cpu: false, bar_gpu: true });
    }

    #[test]
    fn autostart_on_then_off() {
        let home = std::env::temp_dir().join(format!("headroom-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let f = autostart_file(&home);
        set_autostart_in(&home, true).unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(text.contains("--tray"));
        set_autostart_in(&home, false).unwrap();
        assert!(!f.exists());
        // turning off twice is fine
        set_autostart_in(&home, false).unwrap();
        std::fs::remove_dir_all(&home).unwrap();
    }
}
