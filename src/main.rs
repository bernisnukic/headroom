#[cfg_attr(target_os = "macos", path = "macos/a11y.rs")]
mod a11y;
mod anim;
#[cfg_attr(target_os = "macos", path = "macos/collect.rs")]
mod collect;
#[cfg_attr(target_os = "macos", path = "macos/cpu.rs")]
mod cpu;
mod demo;
#[cfg_attr(target_os = "macos", path = "macos/gpu.rs")]
mod gpu;
// finds no sensors on macOS and stays idle
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod sensors;
mod settings;
#[cfg_attr(target_os = "macos", path = "macos/tray.rs")]
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
use settings::{BarItem, Settings};

/// Where snapshots come from: this machine, or the made-up one in demo mode.
enum Source {
    Real(Collector),
    Demo(demo::Demo),
}

impl Source {
    fn collect(&mut self, want_cpu: bool, want_gpu: bool) -> Snapshot {
        match self {
            Source::Real(c) => c.collect(want_cpu, want_gpu),
            Source::Demo(d) => d.snapshot(),
        }
    }

    fn collect_light(&mut self, open: bool) -> Snapshot {
        match self {
            Source::Real(c) => c.collect_light(open),
            Source::Demo(d) => d.snapshot(),
        }
    }
}

/// What the top-bar item shows. The tray is only redrawn when this changes,
/// so it carries just the values that are on display.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct BarState {
    /// RAM in use, %; also named in the tray menu.
    pub used_pct: u8,
    /// The chosen values, in RAM, CPU, GPU order.
    pub items: Vec<(BarItem, u8)>,
    /// Memory: 0 = fine, 1 = warn, 2 = bad.
    pub level: u8,
    pub tip: String,
}

impl BarState {
    /// "RAM 62%  CPU 8%"
    pub fn text(&self) -> String {
        if self.items.is_empty() {
            return format!("RAM {}%", self.used_pct);
        }
        self.items.iter().map(|(i, v)| format!("{} {v}%", i.label())).collect::<Vec<_>>().join("  ")
    }
}

// macOS has no kill log and no AT-SPI, so some variants are never sent there
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub enum Cmd {
    Show,
    ShowTab(Tab),
    ShowSettings,
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
    pub settings: Settings,
    /// Whether the start-at-login entry exists (kept in memory in demo mode).
    pub autostart: bool,
    pub show_settings: bool,
}

#[cfg(not(target_os = "macos"))]
fn notify(urgency: &str, title: &str, body: &str) {
    if demo::on() {
        return;
    }
    let _ = Command::new("notify-send")
        .args(["-a", "Headroom", "-i", "utilities-system-monitor", "-u", urgency, title, body])
        .spawn();
}

/// Notification Center through osascript; title and body go in as
/// arguments, so nothing in them is parsed as AppleScript.
#[cfg(target_os = "macos")]
fn notify(_urgency: &str, title: &str, body: &str) {
    if demo::on() {
        return;
    }
    let child = Command::new("osascript")
        .args(["-e", "on run argv", "-e", "display notification (item 2 of argv) with title (item 1 of argv)", "-e", "end run"])
        .args([title, body])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    // reap it off the UI thread
    if let Ok(mut c) = child {
        std::thread::spawn(move || c.wait());
    }
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
        // macOS: the kernel's own verdict, once it reaches critical
        #[cfg(target_os = "macos")]
        {
            if snap.pressure.level >= 4
                && self.last_pressure_alert.is_none_or(|t| t.elapsed() > Duration::from_secs(300))
            {
                self.last_pressure_alert = Some(Instant::now());
                let top: Vec<String> =
                    snap.apps.iter().take(3).map(|a| format!("{} {}", a.name, fmt_kib(a.total()))).collect();
                notify("critical", "Memory pressure is critical", &format!("Biggest: {}", top.join(", ")));
            }
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
        self.show_settings = false;
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
            // demo pids are made up and may belong to real processes
            if let Some(a) = self.snap.apps.iter().find(|a| a.name == name).filter(|_| !demo::on()) {
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

    pub fn open_settings(&mut self, open: bool, cx: &mut Context<Self>) {
        self.show_settings = open;
        cx.notify();
    }

    /// Shows or hides one top-bar value and saves the choice (memory only in
    /// demo mode). The refresh loop is woken so the tray updates at once.
    pub fn toggle_bar(&mut self, item: BarItem, cx: &mut Context<Self>) {
        if !self.settings.toggle(item) {
            return;
        }
        if !demo::on() {
            if let Err(e) = settings::save(&self.settings) {
                eprintln!("headroom: could not save settings: {e}");
            }
        }
        if let Some(r) = &self.refresh {
            let _ = r.unbounded_send(());
        }
        cx.notify();
    }

    pub fn toggle_autostart(&mut self, cx: &mut Context<Self>) {
        let on = !self.autostart;
        if demo::on() {
            self.autostart = on;
        } else {
            if let Err(e) = settings::set_autostart(on) {
                eprintln!("headroom: could not change start at login: {e}");
            }
            self.autostart = settings::autostart_enabled();
        }
        cx.notify();
    }

    fn tray_state(&self) -> BarState {
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
            // macOS keeps RAM nearly full on purpose (cache, compression), so
            // only the pressure-based insights colour the icon there
            .max(if cfg!(target_os = "macos") { 0 } else if used >= 90 { 2 } else if used >= 80 { 1 } else { 0 });
        let tip = self
            .snap
            .apps
            .iter()
            .take(3)
            .map(|a| format!("{} {}", a.name, fmt_kib(a.total())))
            .collect::<Vec<_>>()
            .join("\n");
        let gpu = self.snap.gpu.devices.iter().filter(|d| !d.asleep).map(|d| d.busy).reduce(f32::max);
        let items = BarItem::ALL
            .into_iter()
            .filter(|i| self.settings.shows(*i))
            .filter_map(|i| match i {
                BarItem::Ram => Some((i, used)),
                BarItem::Cpu => Some((i, self.snap.cpu.total.round().clamp(0.0, 100.0) as u8)),
                // not shown until the first GPU sample (or ever, without a GPU)
                BarItem::Gpu => gpu.map(|g| (i, g.round().clamp(0.0, 100.0) as u8)),
            })
            .collect();
        BarState { used_pct: used, items, level, tip }
    }
}

fn show_window(cx: &mut App, monitor: &Entity<Monitor>) {
    // macOS raises an open window without fuss
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = monitor.read(cx).window {
            if h.update(cx, |_, w, _| w.activate_window()).is_ok() {
                cx.activate(true);
                return;
            }
        }
    }
    // GNOME (Wayland) refuses to raise an existing window without an
    // xdg-activation token and posts a "<title> is ready" notice instead.
    // A freshly mapped window does get focus, so close and reopen.
    if let Some(h) = monitor.read(cx).window {
        let _ = h.update(cx, |_, w, _| w.remove_window());
    }
    let bounds = Bounds::centered(None, size(px(760.), px(900.)), cx);
    let m = monitor.clone();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions { title: Some("Headroom".into()), ..Default::default() }),
        app_id: Some("headroom".into()),
        window_min_size: Some(size(px(640.), px(420.))),
        window_decorations: Some(WindowDecorations::Client),
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };
    // The tab bar fills a transparent title bar, with the traffic lights
    // centred in its 40 px.
    #[cfg(target_os = "macos")]
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Headroom".into()),
            appears_transparent: true,
            traffic_light_position: Some(gpui::point(px(14.), px(13.))),
        }),
        window_background: WindowBackgroundAppearance::Opaque,
        ..options
    };
    let handle = cx
        .open_window(options, |window, cx| {
            window.set_window_title("Headroom");
            cx.new(|cx| ui::RamView::new(m, cx))
        })
        .ok();
    monitor.update(cx, |m, _| m.window = handle.map(|h| h.into()));
    #[cfg(target_os = "macos")]
    cx.activate(true);
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
        // a demo copy must neither take over nor be taken over by a real one
        let name = if demo::on() { "headroom-demo.sock" } else { "headroom.sock" };
        Some(dir.join(std::env::var("HEADROOM_SOCK").unwrap_or_else(|_| name.into())))
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
        } else if std::env::args().any(|a| a == "--settings") {
            b"settings\n"
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
            } else if buf[..n].starts_with(b"settings") {
                Cmd::ShowSettings
            } else {
                Cmd::Show
            };
            let _ = tx.unbounded_send(cmd);
        }
    });
    true
}

fn main() {
    if demo::on() && std::env::args().any(|a| a == "--bench") {
        eprintln!("--bench times the real collectors and does not run with --demo");
        return;
    }
    if std::env::args().any(|a| a == "--bench") {
        collect::bench();
        return;
    }
    if demo::on() && std::env::args().any(|a| a == "--dump") {
        demo::dump();
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

    let app = Application::new();
    // Dock icon clicked with no window open
    #[cfg(target_os = "macos")]
    {
        let tx = tx.clone();
        app.on_reopen(move |_| {
            let _ = tx.unbounded_send(Cmd::Show);
        });
    }
    app.run(move |cx: &mut App| {
        #[cfg(target_os = "macos")]
        mac_menus(cx);
        // demo mode reads nothing from the system: no collectors, journal,
        // sensors, settings file or autostart entry
        let demo = demo::on();
        let policy = if demo { demo::policy() } else { collect::oomd_policy() };
        let mut collector = if demo { Source::Demo(demo::Demo::new()) } else { Source::Real(Collector::new()) };
        let (settings, autostart) =
            if demo { (Settings::default(), false) } else { (settings::load(), settings::autostart_enabled()) };
        // a GPU value in the top bar should not wait for the first tray scan
        let first = collector.collect(false, settings.bar_gpu);
        let first_kills = if demo { demo::kills() } else { collect::kills() };
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
                settings,
                autostart,
                show_settings: std::env::args().any(|a| a == "--settings"),
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
        let board_tx = if demo { None } else { sensors::spawn(board.clone()) };
        let m = monitor.clone();
        let tx_kills = tx.clone();
        if !demo {
            collect::watch_kills(move |k| {
                let _ = tx_kills.unbounded_send(Cmd::Kill(k));
            });
        }
        let has_tray = tray.is_some();

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
                // and the top bar, if it shows GPU: then it is read at every
                // full scan, which is every 20 s while the window is closed
                let bar_gpu = has_tray && m.read_with(cx, |m, _| m.settings.bar_gpu).unwrap_or(false);
                let want_gpu = open && matches!(tab, Tab::Cpu | Tab::Gpu) || bar_gpu;
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
                        if open && !demo {
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
                        t.update(move |t| t.state = state);
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
                    Cmd::ShowSettings => {
                        m.update(cx, |m, cx| m.open_settings(true, cx));
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

#[cfg(target_os = "macos")]
gpui::actions!(headroom, [Quit, CloseWindow]);

/// The app menu, so Cmd-Q and Cmd-W behave as on any Mac app.
#[cfg(target_os = "macos")]
fn mac_menus(cx: &mut App) {
    use gpui::{KeyBinding, Menu, MenuItem};
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &CloseWindow, cx| {
        if let Some(w) = cx.active_window() {
            let _ = w.update(cx, |_, w, _| w.remove_window());
        }
    });
    cx.bind_keys([KeyBinding::new("cmd-q", Quit, None), KeyBinding::new("cmd-w", CloseWindow, None)]);
    cx.set_menus(vec![Menu {
        name: "Headroom".into(),
        items: vec![MenuItem::action("Close Window", CloseWindow), MenuItem::separator(), MenuItem::action("Quit Headroom", Quit)],
    }]);
}

#[cfg(target_os = "macos")]
use collect::dump;

/// Terminal report: `headroom --dump`
#[cfg(not(target_os = "macos"))]
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
