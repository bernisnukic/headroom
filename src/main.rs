mod a11y;
mod anim;
mod collect;
mod cpu;
mod gpu;
mod sensors;
mod tray;
mod ui;

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    AnyWindowHandle, App, AppContext, Application, Bounds, Context, Entity, TitlebarOptions,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowOptions, px, size,
};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};

use collect::{Collector, Insight, Kill, Level, OomdPolicy, Snapshot, fmt_kib};

pub enum Cmd {
    Show,
    ShowTab(Tab),
    /// An assistive client connected and wants the tree.
    A11ySync,
    /// A click from an assistive client.
    A11y(a11y::A11yAct),
    Quit,
    Kill(Kill),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Tab {
    #[default]
    Memory,
    Cpu,
    Gpu,
}

pub struct Monitor {
    a11y: Option<a11y::A11y>,
    /// Wakes the refresh loop early (tab switch), instead of waiting for the 2 s tick.
    refresh: Option<UnboundedSender<()>>,
    pub tab: Tab,
    pub cpu_insights: Vec<Insight>,
    pub gpu_insights: Vec<Insight>,
    pub snap: Snapshot,
    pub kills: Vec<Kill>,
    pub insights: Vec<Insight>,
    pub policy: OomdPolicy,
    pub pending_quit: Option<(String, Instant)>,
    window: Option<AnyWindowHandle>,
    kills_seen: Option<String>,
    last_pressure_alert: Option<Instant>,
}

fn notify(urgency: &str, title: &str, body: &str) {
    let _ = Command::new("notify-send")
        .args(["-a", "Headroom", "-i", "utilities-system-monitor", "-u", urgency, title, body])
        .spawn();
}

impl Monitor {
    fn apply(&mut self, snap: Snapshot, kills: Option<Vec<Kill>>) {
        if let Some(kills) = kills {
            let newest = kills.first().map(|k| k.sort.clone());
            if let Some(seen) = &self.kills_seen {
                for k in kills.iter().filter(|k| &k.sort > seen) {
                    let top = snap.apps.first().map(|a| format!("Biggest now: {} ({}).", a.name, fmt_kib(a.total())));
                    notify(
                        "critical",
                        &format!("{} was killed for memory", k.name),
                        &format!("{}: {}. {}", if k.how == "kernel" { "Kernel OOM killer" } else { "systemd-oomd" }, k.detail, top.unwrap_or_default()),
                    );
                }
            }
            if newest.is_some() || self.kills_seen.is_none() {
                self.kills_seen = Some(newest.unwrap_or_default());
            }
            self.kills = kills;
        }
        // warn before oomd acts
        let p = snap.user_psi.full10;
        if p >= self.policy.limit_pct * 0.6
            && self.last_pressure_alert.is_none_or(|t| t.elapsed() > Duration::from_secs(300))
        {
            self.last_pressure_alert = Some(Instant::now());
            let top: Vec<String> =
                snap.apps.iter().take(3).map(|a| format!("{} {}", a.name, fmt_kib(a.total()))).collect();
            notify(
                "critical",
                &format!("Memory pressure {p:.0}%, an app may be killed soon"),
                &format!("systemd-oomd kills at {:.0}%. Biggest: {}", self.policy.limit_pct, top.join(", ")),
            );
        }
        self.insights = collect::insights(&snap, self.policy);
        self.cpu_insights = cpu::insights(&snap.cpu);
        self.gpu_insights = gpu::insights(&snap.gpu);
        self.snap = snap;
        if self.pending_quit.as_ref().is_some_and(|(_, t)| t.elapsed() > Duration::from_secs(4)) {
            self.pending_quit = None;
        }
    }

    /// A kill reported live by the journal watcher.
    fn add_kill(&mut self, k: Kill) {
        if self.kills.iter().any(|x| x.sort == k.sort && x.name == k.name) {
            return;
        }
        let top = self.snap.apps.first().map(|a| format!("Biggest now: {} ({}).", a.name, fmt_kib(a.total())));
        notify(
            "critical",
            &format!("{} was killed for memory", k.name),
            &format!(
                "{}: {}. {}",
                if k.how == "kernel" { "Kernel OOM killer" } else { "systemd-oomd" },
                k.detail,
                top.unwrap_or_default()
            ),
        );
        self.kills.insert(0, k);
        self.kills.truncate(50);
    }

    /// Pushes the accessibility tree if an assistive client is listening.
    fn sync_a11y(&mut self) {
        if let Some(mut a) = self.a11y.take() {
            a.sync(self);
            self.a11y = Some(a);
        }
    }

    pub fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.sync_a11y();
        if let Some(r) = &self.refresh {
            let _ = r.unbounded_send(());
        }
        cx.notify();
    }

    pub fn quit_click(&mut self, name: &str, cx: &mut Context<Self>) {
        let confirmed =
            self.pending_quit.as_ref().is_some_and(|(n, t)| n == name && t.elapsed() < Duration::from_secs(4));
        if confirmed {
            if let Some(a) = self.snap.apps.iter().find(|a| a.name == name) {
                for pid in &a.pids {
                    unsafe { libc::kill(*pid, libc::SIGTERM) };
                }
            }
            self.pending_quit = None;
        } else {
            self.pending_quit = Some((name.to_string(), Instant::now()));
        }
        self.sync_a11y();
        cx.notify();
    }

    fn tray_state(&self) -> (u8, u8, String) {
        let m = &self.snap.mem;
        let used = (m.used() as f64 / m.total.max(1) as f64 * 100.0).round() as u8;
        let level = self
            .insights
            .iter()
            .map(|i| match i.level {
                Level::Info => 0,
                Level::Warn => 1,
                Level::Bad => 2,
            })
            .max()
            .unwrap_or(0)
            .max(if used >= 90 { 2 } else if used >= 80 { 1 } else { 0 });
        let tip = self
            .snap
            .apps
            .iter()
            .take(3)
            .map(|a| format!("{} {}", a.name, fmt_kib(a.total())))
            .collect::<Vec<_>>()
            .join("\n");
        (used, level, tip)
    }
}

fn show_window(cx: &mut App, monitor: &Entity<Monitor>) {
    // GNOME (Wayland) refuses to raise an existing window without an
    // xdg-activation token and posts a "<title> is ready" notice instead.
    // A freshly mapped window does get focus, so close and reopen.
    if let Some(h) = monitor.read(cx).window {
        let _ = h.update(cx, |_, w, _| w.remove_window());
    }
    let bounds = Bounds::centered(None, size(px(760.), px(900.)), cx);
    let m = monitor.clone();
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions { title: Some("Headroom".into()), ..Default::default() }),
                app_id: Some("headroom".into()),
                window_min_size: Some(size(px(640.), px(420.))),
                window_decorations: Some(WindowDecorations::Client),
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            |window, cx| {
                window.set_window_title("Headroom");
                cx.new(|cx| ui::RamView::new(m, cx))
            },
        )
        .ok();
    monitor.update(cx, |m, _| m.window = handle.map(|h| h.into()));
}

/// Without XDG_RUNTIME_DIR: /tmp/headroom-<uid>, which must be a real
/// directory owned by us with mode 0700. A shared /tmp path would let another
/// user create the socket first and squat it. If that name is taken by
/// something else, mkdtemp makes a fresh random one (single-instance then only
/// covers this process); if even that fails, there is no socket at all.
fn private_tmp() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let uid = unsafe { libc::getuid() };
    let ok = |p: &PathBuf| {
        std::fs::symlink_metadata(p)
            .map(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o077 == 0)
            .unwrap_or(false)
    };
    let base = PathBuf::from(format!("/tmp/headroom-{uid}"));
    // AlreadyExists is fine only if ok() below accepts what is there
    let _ = std::fs::DirBuilder::new().mode(0o700).create(&base);
    if ok(&base) {
        return Some(base);
    }
    let mut tmpl = format!("/tmp/headroom-{uid}-XXXXXX\0").into_bytes();
    let made = unsafe { libc::mkdtemp(tmpl.as_mut_ptr() as *mut libc::c_char) };
    if made.is_null() {
        return None;
    }
    tmpl.pop();
    let alt = PathBuf::from(std::ffi::OsString::from_vec(tmpl));
    ok(&alt).then_some(alt)
}

/// Worked out once: the fallback directory must not be re-created at exit.
fn socket_path() -> Option<PathBuf> {
    static PATH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| {
        let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(d) => PathBuf::from(d),
            None => private_tmp()?,
        };
        Some(dir.join(std::env::var("HEADROOM_SOCK").unwrap_or_else(|_| "headroom.sock".into())))
    })
    .clone()
}

/// Returns false if another instance is running (it was asked to show itself).
fn single_instance(tx: UnboundedSender<Cmd>) -> bool {
    // no safe place for the socket: run without single-instance
    let Some(path) = socket_path() else { return true };
    if let Ok(mut s) = UnixStream::connect(&path) {
        let msg: &[u8] = if std::env::args().any(|a| a == "--cpu") {
            b"cpu\n"
        } else if std::env::args().any(|a| a == "--gpu") {
            b"gpu\n"
        } else {
            b"show\n"
        };
        let _ = s.write_all(msg);
        return false;
    }
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else { return true };
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut buf = [0u8; 16];
            let n = s.read(&mut buf).unwrap_or(0);
            let cmd = if buf[..n].starts_with(b"cpu") {
                Cmd::ShowTab(Tab::Cpu)
            } else if buf[..n].starts_with(b"gpu") {
                Cmd::ShowTab(Tab::Gpu)
            } else {
                Cmd::Show
            };
            let _ = tx.unbounded_send(cmd);
        }
    });
    true
}

fn main() {
    if std::env::args().any(|a| a == "--bench") {
        collect::bench();
        return;
    }
    if std::env::args().any(|a| a == "--dump") {
        dump();
        return;
    }
    let start_hidden = std::env::args().any(|a| a == "--tray");
    let (tx, mut rx) = unbounded::<Cmd>();
    if !single_instance(tx.clone()) {
        return;
    }

    Application::new().run(move |cx: &mut App| {
        let policy = collect::oomd_policy();
        let mut collector = Collector::new();
        let first = collector.collect(false, false);
        let first_kills = collect::kills();
        let monitor = cx.new(|_| {
            let mut m = Monitor {
                a11y: None,
                refresh: None,
                tab: if std::env::args().any(|a| a == "--cpu") {
                    Tab::Cpu
                } else if std::env::args().any(|a| a == "--gpu") {
                    Tab::Gpu
                } else {
                    Tab::Memory
                },
                cpu_insights: Vec::new(),
                gpu_insights: Vec::new(),
                snap: Snapshot::default(),
                kills: Vec::new(),
                insights: Vec::new(),
                policy,
                pending_quit: None,
                window: None,
                kills_seen: None,
                last_pressure_alert: None,
            };
            m.apply(first, Some(first_kills));
            m
        });

        let tray = if std::env::var_os("HEADROOM_NO_TRAY").is_some() { None } else { tray::spawn(tx.clone()) };
        if !start_hidden || tray.is_none() {
            show_window(cx, &monitor);
        }

        // refresh loop: collect on a background thread, apply on the UI thread
        let collector = Arc::new(Mutex::new(collector));
        let board = Arc::new(Mutex::new(sensors::Board::default()));
        let board_tx = sensors::spawn(board.clone());
        let m = monitor.clone();
        let tx_kills = tx.clone();
        collect::watch_kills(move |k| {
            let _ = tx_kills.unbounded_send(Cmd::Kill(k));
        });

        let a11y_tx = tx.clone();
        monitor.update(cx, |m, _| {
            let mut a = a11y::A11y::new(a11y_tx);
            a.set_focused(true);
            m.a11y = Some(a);
        });
        let (refresh_tx, mut refresh_rx) = unbounded::<()>();
        monitor.update(cx, |m, _| m.refresh = Some(refresh_tx));
        cx.spawn(async move |cx| {
            let mut full_at = Instant::now();
            let mut tray_shown = None;
            let mut board_open = false;
            loop {
                let tick = cx.background_executor().timer(Duration::from_secs(2));
                let woke = matches!(
                    futures::future::select(tick, refresh_rx.next()).await,
                    futures::future::Either::Right(_)
                );
                let c = collector.clone();
                let board = board.clone();
                let open = cx.update(|cx| !cx.windows().is_empty()).unwrap_or(false);
                if open != board_open {
                    board_open = open;
                    if let Some(t) = &board_tx {
                        let _ = t.send(open);
                    }
                }
                let tab = m.read_with(cx, |m, _| m.tab).unwrap_or_default();
                let want_cpu = open && tab == Tab::Cpu;
                // GPU busy also feeds the CPU tab's GPU column
                let want_gpu = open && matches!(tab, Tab::Cpu | Tab::Gpu);
                let pressured = m
                    .read_with(cx, |m, _| m.snap.user_psi.some10 > 5.0 || m.snap.mem.available * 10 < m.snap.mem.total)
                    .unwrap_or(false);
                // full (per-process) scan every 2 s while open or under pressure,
                // every 20 s in the tray, and right away after a tab switch
                let full = woke || open || pressured || full_at.elapsed() >= Duration::from_secs(20);
                if full {
                    full_at = Instant::now();
                }
                let snap = cx
                    .background_executor()
                    .spawn(async move {
                        let mut c = c.lock().unwrap();
                        let mut s = if full { c.collect(want_cpu, want_gpu) } else { c.collect_light(open) };
                        if open {
                            s.board = board.lock().unwrap().clone();
                        }
                        s
                    })
                    .await;
                let Ok(state) = m.update(cx, |m, cx| {
                    m.apply(snap, None);
                    if open {
                        m.sync_a11y();
                    }
                    if open {
                        cx.notify();
                    }
                    m.tray_state()
                }) else {
                    break;
                };
                // D-Bus round trip + top-bar redraw only when the icon changes
                if let Some(t) = &tray {
                    if tray_shown.as_ref() != Some(&state) {
                        tray_shown = Some(state.clone());
                        t.update(|t| {
                            (t.used_pct, t.level, t.tip) = state;
                        });
                    }
                }
            }
        })
        .detach();

        // commands from the tray, second launches and the kill watcher;
        // awaits the channel, so it costs no wakeups while idle
        let m = monitor.clone();
        cx.spawn(async move |cx| {
            while let Some(cmd) = rx.next().await {
                let _ = cx.update(|cx| match cmd {
                    Cmd::Show => show_window(cx, &m),
                    Cmd::A11ySync => m.update(cx, |m, _| m.sync_a11y()),
                    Cmd::A11y(a11y::A11yAct::Tab(t)) => m.update(cx, |m, cx| m.set_tab(t, cx)),
                    Cmd::A11y(a11y::A11yAct::Quit(name)) => m.update(cx, |m, cx| m.quit_click(&name, cx)),
                    Cmd::ShowTab(t) => {
                        m.update(cx, |m, cx| m.set_tab(t, cx));
                        show_window(cx, &m);
                    }
                    Cmd::Quit => cx.quit(),
                    Cmd::Kill(k) => m.update(cx, |m, cx| {
                        m.add_kill(k);
                        cx.notify();
                    }),
                });
            }
        })
        .detach();
    });
    if let Some(p) = socket_path() {
        let _ = std::fs::remove_file(p);
    }
}

/// Terminal report: `headroom --dump`
fn dump() {
    let mut c = Collector::new();
    let t = Instant::now();
    let s = c.collect(false, false);
    let full = t.elapsed();
    let t = Instant::now();
    let _ = collect::meminfo();
    let light = t.elapsed();
    let m = &s.mem;
    println!("RAM {} used of {} ({} available) · swap {} / {}", fmt_kib(m.used()), fmt_kib(m.total), fmt_kib(m.available), fmt_kib(m.swap_used), fmt_kib(m.swap_total));
    println!("apps {} · GPU&shared {} · /tmp {} · kernel {} · cache {} · free {}", fmt_kib(m.anon), fmt_kib(m.shmem.saturating_sub(s.tmpfs)), fmt_kib(s.tmpfs.min(m.shmem)), fmt_kib(m.kernel), fmt_kib(m.file_cache), fmt_kib(m.free));
    let b = sensors::read_once();
    if !b.is_empty() {
        let t = |v: Option<f32>| v.map(|t| format!("{t:.0} °C")).unwrap_or_else(|| "-".into());
        let r = |v: Option<u32>| v.map(|r| format!("{r} rpm")).unwrap_or_else(|| "-".into());
        println!("near the RAM {} · near the GPU {} · CPU fan {} · GPU fan {}", t(b.ram_area_c), t(b.gpu_area_c), r(b.cpu_fan_rpm), r(b.gpu_fan_rpm));
    }
    println!("pressure: system full10 {:.1}% · your session full10 {:.1}% (oomd limit {:.0}%)", s.psi.full10, s.user_psi.full10, collect::oomd_policy().limit_pct);
    println!("\n{:>9} {:>9} {:>9} {:>8}  APP", "RAM", "SWAP", "GPU", "UP");
    for a in s.apps.iter().take(15) {
        println!("{:>9} {:>9} {:>9} {:>8}  {} ({} procs)", fmt_kib(a.pss), fmt_kib(a.swap), fmt_kib(a.gpu), collect::fmt_age(a.age_s), a.name, a.pids.len());
    }
    println!("\nWhy:");
    for i in collect::insights(&s, collect::oomd_policy()) {
        println!("  [{:?}] {}", i.level, i.text);
    }
    println!("\nKilled for memory (7 days):");
    for k in collect::kills().iter().take(10) {
        println!("  {:<14} {:<7} {:<16} {}", k.when, k.how, k.name, k.detail);
    }
    println!("\nfull scan {} ms · meminfo only {} µs", full.as_millis(), light.as_micros());
}
