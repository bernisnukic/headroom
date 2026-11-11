//! Window UI. Native-utility read: dense, calm, dark zinc with one blue accent.
//! Amber and red appear only for real warnings. One 4 px corner radius,
//! hairlines and spacing instead of boxed cards, monospaced numbers.

use gpui::{
    Bounds, Context, CursorStyle, Decorations, ElementId, Entity, FontWeight, HitboxBehavior, IntoElement,
    MouseButton, Pixels, Point, Render, ResizeEdge, Rgba, SharedString, Size, Subscription, Window, canvas, div,
    point, prelude::*, px, relative, rgb,
};

use crate::cpu::{CpuApp, CpuSnap};
use crate::gpu::{GpuApp, GpuSnap};
use crate::{Monitor, Tab};
use crate::collect::{App as AppUse, GIB, Level, MIB, fmt_age, fmt_kib};

// palette
const BG: u32 = 0x111214;
const SURFACE: u32 = 0x18191c;
const HOVER: u32 = 0x1e1f23;
const HAIR: u32 = 0x27292e;
const TEXT: u32 = 0xe4e4e7;
const DIM: u32 = 0x9a9aa3;
const FAINT: u32 = 0x6e6e78;
const ACCENT: u32 = 0x5b8cff;
const ACCENT_2: u32 = 0x3f63b8;
const ACCENT_3: u32 = 0x2b4378;
const KERNEL: u32 = 0x5c5d66;
const CACHE: u32 = 0x35363c;
const TRACK: u32 = 0x1f2024;
const OK: u32 = 0x4caf7a;
const WARN: u32 = 0xe0a23a;
const BAD: u32 = 0xf0524f;

const R: f32 = 4.0;
const SANS: &str = "Ubuntu Sans";
const MONO: &str = "Ubuntu Sans Mono";
const EDGE: f32 = 6.0; // resize grab margin

fn c(hex: u32) -> Rgba {
    rgb(hex)
}

fn level_color(l: Level) -> u32 {
    match l {
        Level::Info => OK,
        Level::Warn => WARN,
        Level::Bad => BAD,
    }
}

pub struct RamView {
    monitor: Entity<Monitor>,
    _sub: Subscription,
    tw: crate::anim::Tw,
}

impl RamView {
    pub fn new(monitor: Entity<Monitor>, cx: &mut Context<Self>) -> Self {
        let _sub = cx.observe(&monitor, |_, _, cx| cx.notify());
        Self { monitor, _sub, tw: crate::anim::Tw::new() }
    }
}

/// Right-aligned, fixed-width cell text: pads to `w` characters so columns
/// never shift when values change length (e.g. "985 MB" -> "1.3 GB").
fn pad(s: impl AsRef<str>, w: usize) -> String {
    format!("{:>w$}", s.as_ref())
}

fn mono(s: impl Into<SharedString>) -> gpui::Div {
    div().font_family(MONO).child(s.into())
}

fn heading(title: &str) -> gpui::Div {
    div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).text_color(c(TEXT)).child(title.to_string())
}

fn section(title: &str) -> gpui::Div {
    div().flex().flex_col().gap_2().pt_4().border_t_1().border_color(c(HAIR)).child(heading(title))
}

fn legend(color: u32, label: &str, value: String) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .w(px(206.))
        .child(div().w(px(8.)).h(px(8.)).rounded(px(2.)).bg(c(color)))
        .child(div().flex_1().text_color(c(DIM)).child(label.to_string()))
        .child(mono(value).text_color(c(TEXT)).pr_4())
}

fn stacked(parts: &[(u32, u64)], total: u64, h: f32) -> impl IntoElement {
    div()
        .flex()
        .w_full()
        .h(px(h))
        .rounded(px(R))
        .overflow_hidden()
        .bg(c(TRACK))
        .children(
            parts
                .iter()
                .filter(|(_, v)| *v > 0)
                .map(|(col, v)| div().h_full().w(relative(*v as f32 / total.max(1) as f32)).bg(c(*col))),
        )
}

/// Paints bars straight to the GPU with no layout nodes (a few hundred bar
/// divs per frame was most of the 60 fps cost). `vals[i]` is (height 0..1,
/// colour) or None for an empty slot. Slot width is width / `slots`; the
/// strip is shifted left by `shift` slots and clipped to the bounds.
fn bars(vals: Vec<Option<(f32, u32)>>, slots: usize, gap: f32, shift: f32) -> gpui::Canvas<()> {
    canvas(
        |_, _, _| (),
        move |b, _, window, _| {
            let (x0, y0) = (f32::from(b.origin.x), f32::from(b.origin.y));
            let (w, h) = (f32::from(b.size.width), f32::from(b.size.height));
            let slot = w / slots.max(1) as f32;
            for (i, v) in vals.iter().enumerate() {
                let Some((f, col)) = v else { continue };
                let x = x0 + (i as f32 - shift) * slot;
                let (l, r) = (x.max(x0), (x + slot - gap).min(x0 + w));
                if r <= l {
                    continue;
                }
                let bh = (h * f.clamp(0.0, 1.0)).max(1.0);
                window.paint_quad(gpui::fill(
                    Bounds::new(point(px(l), px(y0 + h - bh)), gpui::size(px(r - l), px(bh))),
                    rgb(*col),
                ));
            }
        },
    )
}

/// Right-aligns `vals` in `slots` slots.
fn right_aligned<T: Copy>(vals: &[T], slots: usize) -> impl Iterator<Item = Option<T>> + '_ {
    let start = vals.len().saturating_sub(slots);
    std::iter::repeat_n(None, slots.saturating_sub(vals.len() - start)).chain(vals[start..].iter().map(|v| Some(*v)))
}

/// Memory trend. Scaled between 70% of the lowest and the highest value so
/// change is visible; a series that barely moves draws as a low flat line
/// instead of full-height blocks.
fn sparkline(vals: &[u64], color: u32) -> impl IntoElement {
    let max = vals.iter().copied().max().unwrap_or(1).max(1) as f32;
    let min = vals.iter().copied().min().unwrap_or(0) as f32;
    let flat = (max - min) / max < 0.03;
    let floor = (min * 0.7).min(max - 1.0);
    let pts = right_aligned(vals, 40)
        .map(|v| {
            v.map(|v| {
                let f = if flat { 0.3 } else { ((v as f32 - floor) / (max - floor)).clamp(0.08, 1.0) };
                (f, color)
            })
        })
        .collect();
    bars(pts, 40, 0.0, 0.0).w(px(80.)).h(px(14.))
}

pub const NO_QUIT: &[&str] =
    &["gnome-shell", "Xwayland", "systemd", "headroom", "pipewire", "wireplumber", "gnome-session-binary"];

const COLS: [f32; 6] = [70., 64., 64., 88., 72., 56.];

fn app_row(a: &AppUse, pending: Option<&str>, monitor: &Entity<Monitor>, wide: bool) -> impl IntoElement {
    let growth = a.growth_mb_h.unwrap_or(0.0);
    let _ = growth;
    let leaking = a.steady_growth();
    let trend_col = if leaking { WARN } else { ACCENT_2 };
    let name = a.name.clone();
    let confirming = pending == Some(a.name.as_str());
    let can_quit = !NO_QUIT.contains(&a.name.as_str()) && !a.pids.contains(&(std::process::id() as i32));
    let m = monitor.clone();
    let size_col = if a.total() > 4 * GIB { BAD } else { TEXT };
    let group: SharedString = format!("row-{name}").into();

    div()
        .id(ElementId::Name(format!("app-{name}").into()))
        .group(group.clone())
        .flex()
        .items_center()
        .gap_3()
        .h(px(30.))
        .px_2()
        .rounded(px(R))
        .hover(|s| s.bg(c(HOVER)))
        .whitespace_nowrap()
        .child(div().flex_1().min_w(px(140.)).overflow_hidden().truncate().text_color(c(TEXT)).child(a.name.clone()))
        .child(mono(pad(fmt_kib(a.pss), 7)).w(px(COLS[0])).text_right().text_color(c(size_col)))
        .child(
            mono(pad(if a.swap > 1024 { fmt_kib(a.swap) } else { "-".into() }, 7))
                .w(px(COLS[1]))
                .text_right()
                .text_color(c(if a.swap > 256 * MIB { WARN } else { DIM })),
        )
        .child(
            mono(pad(if a.gpu > 1024 { fmt_kib(a.gpu) } else { "-".into() }, 7))
                .w(px(COLS[2]))
                .text_right()
                .text_color(c(if a.gpu > 3 * GIB { BAD } else { DIM })),
        )
        .when(wide, |d| d.child(div().w(px(COLS[3])).flex().justify_end().when(a.spark.len() >= 3, |d| d.child(sparkline(&a.spark, trend_col))).when(a.spark.len() < 3, |d| d.child(mono(pad("-", 7)).text_color(c(DIM))))))
        .child(
            mono(pad(
                match a.growth_mb_h {
                    Some(g) if g.abs() >= 1.0 => format!("{g:+.0}/h"),
                    Some(_) => "flat".into(),
                    None => "-".into(),
                },
                7,
            ))
            .w(px(COLS[4]))
            .text_right()
            .text_color(c(if leaking { WARN } else { DIM })),
        )
        .when(wide, |d| d.child(mono(pad(fmt_age(a.age_s), 7)).w(px(COLS[5])).text_right().text_color(c(DIM))))
        .child(div().w(px(64.)).flex().justify_end().when(can_quit, |d| {
            d.child(
                div()
                    .id(ElementId::Name(format!("quit-{name}").into()))
                    .px_2()
                    .py(px(2.))
                    .rounded(px(R))
                    .text_size(px(11.5))
                    .cursor_pointer()
                    .when(confirming, |d| d.bg(c(BAD)).text_color(c(0x18090a)).child("Confirm"))
                    .when(!confirming, |d| {
                        d.border_1()
                            .border_color(c(HAIR))
                            .text_color(c(DIM))
                            .opacity(0.)
                            .group_hover(group.clone(), |s| s.opacity(1.))
                            .hover(|s| s.border_color(c(BAD)).text_color(c(BAD)))
                            .child("Quit")
                    })
                    .on_click(move |_, _, cx| {
                        let name = name.clone();
                        m.update(cx, |m, cx| m.quit_click(&name, cx));
                    }),
            )
        }))
}

fn control(id: &'static str, label: &'static str, danger: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w(px(34.))
        .h(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(R))
        .text_color(c(DIM))
        .text_size(px(14.))
        .cursor_pointer()
        .hover(move |s| if danger { s.bg(c(BAD)).text_color(c(0xffffff)) } else { s.bg(c(HOVER)).text_color(c(TEXT)) })
        .child(label)
}

fn tab_button(label: &'static str, tab: Tab, active: bool, monitor: &Entity<Monitor>) -> impl IntoElement {
    let m = monitor.clone();
    div()
        .id(label)
        .px_3()
        .h(px(24.))
        .flex()
        .items_center()
        .rounded(px(R))
        .cursor_pointer()
        .text_size(px(12.5))
        .when(active, |d| d.bg(c(HOVER)).text_color(c(TEXT)))
        .when(!active, |d| d.text_color(c(DIM)).hover(|s| s.text_color(c(TEXT))))
        .child(label)
        .on_click(move |_, _, cx| m.update(cx, |m, cx| m.set_tab(tab, cx)))
}

fn titlebar(summary: String, tab: Tab, monitor: &Entity<Monitor>) -> impl IntoElement {
    div()
        .id("titlebar")
        .flex()
        .flex_none()
        .items_center()
        .justify_between()
        .h(px(40.))
        .pl_4()
        .pr_2()
        .bg(c(SURFACE))
        .border_b_1()
        .border_color(c(HAIR))
        .on_mouse_down(MouseButton::Left, |e, window, _| {
            if e.click_count >= 2 {
                window.zoom_window();
            } else {
                window.start_window_move();
            }
        })
        .on_click(|e, window, _| {
            if e.is_right_click() {
                window.show_window_menu(e.position());
            }
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(c(TEXT)).child("Headroom"))
                .child(
                    div()
                        .flex()
                        .gap_1()
                        .p(px(2.))
                        .ml_2()
                        .rounded(px(R + 2.))
                        .bg(c(BG))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(tab_button("Memory", Tab::Memory, tab == Tab::Memory, monitor))
                        .child(tab_button("CPU", Tab::Cpu, tab == Tab::Cpu, monitor))
                        .child(tab_button("GPU", Tab::Gpu, tab == Tab::Gpu, monitor)),
                )
                .child(mono(summary).ml_2().text_color(c(DIM)).text_size(px(12.))),
        )
        .child(
            div()
                .flex()
                .gap_1()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(control("min", "\u{2212}", false).on_click(|_, w, _| w.minimize_window()))
                .child(control("max", "\u{25a1}", false).on_click(|_, w, _| w.zoom_window()))
                .child(control("close", "\u{00d7}", true).on_click(|_, w, _| w.remove_window())),
        )
}

fn fmt_cpu_time(s: u64) -> String {
    if s >= 3600 {
        format!("{}h {}m", s / 3600, s % 3600 / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// CPU history, one bar per 2 s sample. When a sample lands, `shift` jumps
/// to 1 and eases to 0: the strip (one extra bar wide) starts where it was
/// and slides left by one slot, instead of every bar stepping at once.
fn bar_chart(vals: &[f32], shift: f32, max: f32, h: f32, slots: usize) -> impl IntoElement {
    let pts: Vec<Option<(f32, u32)>> = right_aligned(vals, slots + 1)
        .map(|v| v.map(|v| ((v / max).max(0.02), if v >= 90.0 { WARN } else { ACCENT })))
        .collect();
    bars(pts, slots, 1.0, 1.0 - shift).w_full().h(px(h))
}

/// Rows stacked by absolute position so they glide when the order changes.
fn sliding_rows(n: usize, rows: Vec<(f32, gpui::AnyElement)>) -> impl IntoElement {
    div()
        .relative()
        .w_full()
        .h(px(n as f32 * crate::anim::ROW_H))
        .children(rows.into_iter().map(|(y, row)| div().absolute().left_0().w_full().top(px(y)).child(row)))
}

/// Per-app CPU trend, scaled to the app's own recent peak (at least 5%) so
/// its shape is readable; the CPU column gives the absolute number.
fn spark_f(vals: &[f32], color: u32) -> impl IntoElement {
    let max = vals.iter().copied().fold(5.0_f32, f32::max);
    let pts = right_aligned(vals, 60).map(|v| v.map(|v| ((v / max).clamp(0.08, 1.0), color))).collect();
    bars(pts, 60, 0.0, 0.0).w(px(120.)).h(px(14.))
}

/// Smallest of 10/25/50/100 that fits the peak, so a quiet machine's
/// history isn't a strip of 2 px bars under a 100% ceiling.
fn nice_ceiling(peak: f32) -> f32 {
    [10.0, 25.0, 50.0, 100.0].into_iter().find(|c| peak <= *c).unwrap_or(100.0)
}

const CPU_COLS: [f32; 5] = [64., 124., 76., 60., 56.];

const GPU_COL: f32 = 52.;

fn cpu_row(a: &CpuApp, gpu: Option<f32>, pending: Option<&str>, monitor: &Entity<Monitor>, wide: bool) -> impl IntoElement {
    let name = a.name.clone();
    let confirming = pending == Some(a.name.as_str());
    let can_quit = !NO_QUIT.contains(&a.name.as_str()) && !a.pids.contains(&(std::process::id() as i32));
    let m = monitor.clone();
    let hot = a.pct >= 90.0;
    let group: SharedString = format!("crow-{name}").into();
    div()
        .id(ElementId::Name(format!("cpu-{name}").into()))
        .group(group.clone())
        .flex()
        .items_center()
        .gap_3()
        .h(px(30.))
        .px_2()
        .rounded(px(R))
        .whitespace_nowrap()
        .hover(|s| s.bg(c(HOVER)))
        .child(div().flex_1().min_w(px(140.)).overflow_hidden().truncate().text_color(c(TEXT)).child(a.name.clone()))
        .child(
            mono(pad(format!("{:.1}%", a.pct), 7))
                .w(px(CPU_COLS[0]))
                .text_right()
                .text_color(c(if hot { WARN } else if a.pct < 0.05 { DIM } else { TEXT })),
        )
        .child(
            mono(pad(gpu.filter(|g| *g >= 0.5).map(|g| format!("{g:.0}%")).unwrap_or_else(|| "-".into()), 5))
                .w(px(GPU_COL))
                .text_right()
                .text_color(c(if gpu.is_some_and(|g| g >= 80.0) { WARN } else { DIM })),
        )
        .when(wide, |d| {
            d.child(div().w(px(CPU_COLS[1])).flex().justify_end().child(spark_f(&a.spark, if hot { WARN } else { ACCENT_2 })))
        })
        .child(mono(pad(fmt_cpu_time(a.cpu_secs), 8)).w(px(CPU_COLS[2])).text_right().text_color(c(DIM)))
        .when(wide, |d| d.child(mono(pad(a.threads.to_string(), 5)).w(px(CPU_COLS[3])).text_right().text_color(c(DIM))))
        .child(mono(pad(fmt_age(a.age_s), 7)).w(px(CPU_COLS[4])).text_right().text_color(c(DIM)))
        .child(div().w(px(64.)).flex().justify_end().when(can_quit, |d| {
            d.child(
                div()
                    .id(ElementId::Name(format!("cquit-{name}").into()))
                    .px_2()
                    .py(px(2.))
                    .rounded(px(R))
                    .text_size(px(11.5))
                    .cursor_pointer()
                    .when(confirming, |d| d.bg(c(BAD)).text_color(c(0x18090a)).child("Confirm"))
                    .when(!confirming, |d| {
                        d.border_1()
                            .border_color(c(HAIR))
                            .text_color(c(DIM))
                            .opacity(0.)
                            .group_hover(group.clone(), |s| s.opacity(1.))
                            .hover(|s| s.border_color(c(BAD)).text_color(c(BAD)))
                            .child("Quit")
                    })
                    .on_click(move |_, _, cx| {
                        let name = name.clone();
                        m.update(cx, |m, cx| m.quit_click(&name, cx));
                    }),
            )
        }))
}

fn insight_list(items: &[crate::collect::Insight]) -> gpui::Div {
    section("What needs attention").children(items.iter().map(|ins| {
        div()
            .flex()
            .gap_3()
            .child(div().w(px(2.)).flex_none().rounded(px(1.)).bg(c(level_color(ins.level))))
            .child(div().flex_1().text_color(c(TEXT)).line_height(px(19.)).child(ins.text.clone()))
    }))
}

fn cpu_view(
    s: &CpuSnap,
    insights: &[crate::collect::Insight],
    pending: Option<&str>,
    monitor: &Entity<Monitor>,
    wide: bool,
    ys: &std::collections::HashMap<String, f32>,
    ceiling: f32,
    shift: f32,
    g: &GpuSnap,
    board: &crate::sensors::Board,
) -> gpui::Div {
    let n = s.cores.len().max(1);
    let temp_col = match s.temp_c {
        Some(t) if t >= 95.0 => BAD,
        Some(t) if t >= 85.0 => WARN,
        _ => TEXT,
    };
    let overview = div()
        .flex()
        .justify_between()
        .items_end()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_end()
                        .gap_2()
                        .child(
                            mono(format!("{:.0}%", s.total))
                                .text_size(px(34.))
                                .line_height(px(38.))
                                .font_weight(FontWeight::BOLD)
                                .text_color(c(if s.total >= 90.0 { WARN } else { TEXT })),
                        )
                        .child(div().pb(px(5.)).text_color(c(DIM)).child(format!("busy across {n} threads"))),
                )
                .child(div().text_color(c(DIM)).child(format!(
                    "Load {:.2}, {:.2}, {:.2} over 1, 5 and 15 min",
                    s.load[0], s.load[1], s.load[2]
                )))
                .when(!g.devices.is_empty(), |d| d.child(div().text_color(c(DIM)).child(gpu_summary(g)))),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .items_end()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_1()
                        .text_color(c(temp_col))
                        .child(
                            mono(s.temp_c.map(|t| format!("{t:.0}")).unwrap_or_else(|| "-".into()))
                                .text_size(px(26.))
                                .line_height(px(30.))
                                .font_weight(FontWeight::BOLD),
                        )
                        .when(s.temp_c.is_some(), |d| d.child(div().text_size(px(16.)).child("°C"))),
                )
                .child(div().text_color(c(DIM)).child(match board.cpu_fan_rpm {
                    Some(r) => format!(
                        "{:.1} of {:.1} GHz, fan {}",
                        s.freq_mhz as f32 / 1000.0,
                        s.max_mhz as f32 / 1000.0,
                        fmt_rpm(r)
                    ),
                    None => format!("{:.1} of {:.1} GHz", s.freq_mhz as f32 / 1000.0, s.max_mhz as f32 / 1000.0),
                })),
        );

    let history = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(bar_chart(&s.history, shift, ceiling, 56., crate::cpu::HISTORY_POINTS))
        .child(
            div()
                .flex()
                .justify_between()
                .text_size(px(11.5))
                .text_color(c(FAINT))
                .child("3 min ago")
                .child(format!("Top of chart is {:.0}%", nice_ceiling(ceiling - 0.5)))
                .child("now"),
        );

    let cores = section("Cores").child(div().flex().gap_1().children(s.cores.iter().enumerate().map(|(i, p)| {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .child(
                div()
                    .w_full()
                    .h(px(40.))
                    .rounded(px(R))
                    .overflow_hidden()
                    .bg(c(TRACK))
                    .flex()
                    .items_end()
                    .child(div().w_full().h(relative((p / 100.0).clamp(0.0, 1.0))).bg(c(if *p >= 90.0 { WARN } else { ACCENT }))),
            )
            .child(mono(format!("{p:.0}")).text_size(px(10.5)).text_color(c(DIM)))
            .id(ElementId::Integer(i as u64))
    })));

    let col_head = |w: f32, t: &'static str| div().w(px(w)).text_right().child(t);
    let apps = section("Apps").child(
        div()
            .flex()
            .gap_3()
            .px_2()
            .text_size(px(11.5))
            .text_color(c(FAINT))
            .whitespace_nowrap()
            .child(div().flex_1().min_w(px(140.)).child("Name"))
            .child(col_head(CPU_COLS[0], "CPU"))
            .child(col_head(GPU_COL, "GPU"))
            .when(wide, |d| d.child(col_head(CPU_COLS[1], "Last 2 min")))
            .child(col_head(CPU_COLS[2], "CPU time"))
            .when(wide, |d| d.child(col_head(CPU_COLS[3], "Threads")))
            .child(col_head(CPU_COLS[4], "Running"))
            .child(div().w(px(64.))),
    );
    let apps = if s.apps.is_empty() || s.measuring_apps {
        apps.child(div().px_2().text_color(c(DIM)).child("Measuring per-app CPU, this takes 2 seconds."))
    } else {
        let shown: Vec<_> = s.apps.iter().take(20).collect();
        apps.child(sliding_rows(
            shown.len(),
            shown
                .iter()
                .map(|a| {
                    (
                        ys.get(&a.name).copied().unwrap_or(0.0),
                        cpu_row(a, g.app_busy(&a.name), pending, monitor, wide).into_any_element(),
                    )
                })
                .collect(),
        ))
    };

    div()
        .flex()
        .flex_col()
        .gap_5()
        .child(overview)
        .child(history)
        .child(cores)
        .child(insight_list(insights))
        .child(apps)
}

fn short_gpu(name: &str) -> String {
    name.split_whitespace().next().unwrap_or(name).to_string()
}

fn gpu_summary(g: &GpuSnap) -> String {
    let parts: Vec<String> = g
        .devices
        .iter()
        .map(|d| if d.asleep { format!("{} asleep", d.name) } else { format!("{} {:.0}% busy", d.name, d.busy) })
        .collect();
    format!("GPU: {}", parts.join(", "))
}

/// The Quit / Confirm button shared by the GPU table.
fn quit_button(id: String, name: String, confirming: bool, group: SharedString, monitor: &Entity<Monitor>) -> impl IntoElement {
    let m = monitor.clone();
    div()
        .id(ElementId::Name(id.into()))
        .px_2()
        .py(px(2.))
        .rounded(px(R))
        .text_size(px(11.5))
        .cursor_pointer()
        .when(confirming, |d| d.bg(c(BAD)).text_color(c(0x18090a)).child("Confirm"))
        .when(!confirming, |d| {
            d.border_1()
                .border_color(c(HAIR))
                .text_color(c(DIM))
                .opacity(0.)
                .group_hover(group, |s| s.opacity(1.))
                .hover(|s| s.border_color(c(BAD)).text_color(c(BAD)))
                .child("Quit")
        })
        .on_click(move |_, _, cx| {
            let name = name.clone();
            m.update(cx, |m, cx| m.quit_click(&name, cx));
        })
}

fn gpu_row(a: &GpuApp, pending: Option<&str>, monitor: &Entity<Monitor>, wide: bool) -> impl IntoElement {
    let can_quit = !NO_QUIT.contains(&a.name.as_str()) && !a.pids.contains(&(std::process::id() as i32));
    let hot = a.max_busy >= 80.0;
    let group: SharedString = format!("grow-{}", a.name).into();
    div()
        .id(ElementId::Name(format!("gpu-{}", a.name).into()))
        .group(group.clone())
        .flex()
        .items_center()
        .gap_3()
        .h(px(30.))
        .px_2()
        .rounded(px(R))
        .whitespace_nowrap()
        .hover(|s| s.bg(c(HOVER)))
        .child(div().flex_1().min_w(px(140.)).overflow_hidden().truncate().text_color(c(TEXT)).child(a.name.clone()))
        .children(a.busy.iter().map(|b| {
            mono(pad(if *b >= 0.5 { format!("{b:.0}%") } else { "-".into() }, 5))
                .w(px(64.))
                .text_right()
                .text_color(c(if *b >= 80.0 { WARN } else if *b >= 0.5 { TEXT } else { DIM }))
        }))
        .when(wide, |d| d.child(div().w(px(124.)).flex().justify_end().child(spark_f(&a.spark, if hot { WARN } else { ACCENT_2 }))))
        .child(mono(pad(if a.mem > 0 { fmt_kib(a.mem) } else { "-".into() }, 8)).w(px(76.)).text_right().text_color(c(DIM)))
        .when(wide, |d| d.child(mono(pad(fmt_age(a.age_s), 7)).w(px(56.)).text_right().text_color(c(DIM))))
        .child(div().w(px(64.)).flex().justify_end().when(can_quit, |d| {
            d.child(quit_button(
                format!("gquit-{}", a.name),
                a.name.clone(),
                pending == Some(a.name.as_str()),
                group.clone(),
                monitor,
            ))
        }))
}

fn gpu_view(
    g: &GpuSnap,
    insights: &[crate::collect::Insight],
    pending: Option<&str>,
    monitor: &Entity<Monitor>,
    wide: bool,
    ys: &std::collections::HashMap<String, f32>,
    shifts: &[f32],
    board: &crate::sensors::Board,
) -> gpui::Div {
    let mut out = div().flex().flex_col().gap_5();
    if g.devices.is_empty() {
        return out.child(div().text_color(c(DIM)).child("Reading the GPUs, this takes 2 seconds."));
    }
    for (i, d) in g.devices.iter().enumerate() {
        let mut facts: Vec<String> = Vec::new();
        if let (Some(f), Some(mx)) = (d.freq_mhz, d.max_mhz) {
            facts.push(format!("clock {:.2} of {:.2} GHz", f as f32 / 1000.0, mx as f32 / 1000.0));
        }
        if let (Some(u), Some(t)) = (d.mem_used, d.mem_total) {
            facts.push(format!("{} of {} video memory", fmt_kib(u), fmt_kib(t)));
        }
        let dgpu = crate::gpu::is_discrete(d);
        match (d.temp_c, dgpu.then_some(board.gpu_area_c).flatten()) {
            (Some(t), _) => facts.push(format!("{t:.0} °C")),
            (None, Some(t)) => facts.push(format!("{t:.0} °C beside it")),
            _ => {}
        }
        if let Some(w) = d.power_w {
            facts.push(format!("{w:.1} W"));
        }
        if let Some(r) = dgpu.then_some(board.gpu_fan_rpm).flatten() {
            facts.push(format!("fan {}", fmt_rpm(r)));
        }
        // a sleeping card can't be asked, but the board sensor and fan can
        let asleep_text = {
            let mut t = "Asleep and saving power. Headroom leaves it alone so it stays asleep.".to_string();
            if let Some(a) = dgpu.then_some(board.gpu_area_c).flatten() {
                t.push_str(&format!(" {a:.0} °C beside it"));
                if let Some(r) = board.gpu_fan_rpm {
                    t.push_str(&format!(", fan {}", fmt_rpm(r)));
                }
                t.push('.');
            }
            t
        };
        let head = div()
            .flex()
            .justify_between()
            .items_end()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(d.name.clone()))
                    .child(div().text_color(c(DIM)).child(if d.asleep {
                        asleep_text
                    } else {
                        facts.join(", ")
                    })),
            )
            .when(!d.asleep, |h| {
                h.child(
                    mono(format!("{:.0}%", d.busy))
                        .text_size(px(26.))
                        .line_height(px(30.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(c(if d.busy >= 90.0 { WARN } else { TEXT })),
                )
            });
        let mut block = div().flex().flex_col().gap_3().pt_4().border_t_1().border_color(c(HAIR)).child(head);
        if !d.asleep {
            let ceiling = nice_ceiling(d.history.iter().copied().fold(0.0, f32::max));
            block = block
                .child(bar_chart(&d.history, shifts.get(i).copied().unwrap_or(0.0), ceiling, 40., crate::gpu::HISTORY_POINTS))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .text_size(px(11.5))
                        .text_color(c(FAINT))
                        .child("3 min ago")
                        .child(format!("Top of chart is {ceiling:.0}%"))
                        .child("now"),
                )
                .child(div().flex().flex_col().gap_1().children(d.engines.iter().map(|e| {
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .child(div().w(px(150.)).text_color(c(DIM)).child(e.name.clone()))
                        .child(div().flex_1().child(stacked(&[(if e.busy >= 90.0 { WARN } else { ACCENT }, (e.busy * 10.0) as u64)], 1000, 6.)))
                        .child(mono(pad(format!("{:.0}%", e.busy), 5)).w(px(48.)).text_right().text_color(c(TEXT)))
                })));
        }
        out = out.child(block);
    }
    out = out.child(insight_list(insights));

    let col_head = |w: f32, t: String| div().w(px(w)).text_right().child(t);
    let mut header = div()
        .flex()
        .gap_3()
        .px_2()
        .text_size(px(11.5))
        .text_color(c(FAINT))
        .whitespace_nowrap()
        .child(div().flex_1().min_w(px(140.)).child("Name"));
    for d in &g.devices {
        header = header.child(col_head(64., short_gpu(&d.name)));
    }
    header = header
        .when(wide, |h| h.child(col_head(124., "Last 2 min".into())))
        .child(col_head(76., "Memory".into()))
        .when(wide, |h| h.child(col_head(56., "Running".into())))
        .child(div().w(px(64.)));
    let apps = section("Apps").child(header);
    let apps = if g.measuring || g.apps.is_empty() {
        apps.child(div().px_2().text_color(c(DIM)).child("Measuring per-app GPU use, this takes 2 seconds."))
    } else {
        let shown: Vec<_> = g.apps.iter().take(20).collect();
        apps.child(sliding_rows(
            shown.len(),
            shown
                .iter()
                .map(|a| (ys.get(&a.name).copied().unwrap_or(0.0), gpu_row(a, pending, monitor, wide).into_any_element()))
                .collect(),
        ))
    };
    out.child(apps)
}

fn resize_edge(pos: Point<Pixels>, size: Size<Pixels>) -> Option<ResizeEdge> {
    let e = px(EDGE);
    let (top, bottom) = (pos.y < e, pos.y > size.height - e);
    let (left, right) = (pos.x < e, pos.x > size.width - e);
    Some(match (top, bottom, left, right) {
        (true, _, true, _) => ResizeEdge::TopLeft,
        (true, _, _, true) => ResizeEdge::TopRight,
        (_, true, true, _) => ResizeEdge::BottomLeft,
        (_, true, _, true) => ResizeEdge::BottomRight,
        (true, ..) => ResizeEdge::Top,
        (_, true, ..) => ResizeEdge::Bottom,
        (_, _, true, _) => ResizeEdge::Left,
        (_, _, _, true) => ResizeEdge::Right,
        _ => return None,
    })
}

fn cursor_for(edge: ResizeEdge) -> CursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
    }
}

impl Render for RamView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let csd = matches!(window.window_decorations(), Decorations::Client { .. });
        let maximized = window.is_maximized();
        let wide = window.viewport_size().width >= px(720.);
        let m = self.monitor.read(cx);
        let (eased, ys) = self.tw.ease(&m.snap, m.tab);
        let s = &eased;
        let ceiling = self.tw.value(
            "c.ceiling",
            nice_ceiling(s.cpu.history.iter().copied().fold(0.0, f32::max)),
        );
        let shift = self.tw.chart_shift();
        let gpu_shifts: Vec<f32> = (0..s.gpu.devices.len()).map(|i| self.tw.gpu_shift(i)).collect();
        let mem = &s.mem;
        let used_frac = mem.used() as f32 / mem.total.max(1) as f32;
        let p = s.user_psi.full10;
        let lim = m.policy.limit_pct;
        let p_col = if p >= lim * 0.8 {
            BAD
        } else if p >= lim * 0.4 {
            WARN
        } else {
            TEXT
        };
        let head_col = if used_frac > 0.9 { BAD } else { TEXT };

        let tmpfs = s.tmpfs.min(mem.shmem);
        let shared = mem.shmem - tmpfs;
        let parts =
            [(ACCENT, mem.anon), (ACCENT_2, shared), (ACCENT_3, tmpfs), (KERNEL, mem.kernel), (CACHE, mem.file_cache)];

        let overview = div()
            .flex()
            .justify_between()
            .items_end()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap_2()
                            .child(
                                mono(fmt_kib(mem.used()))
                                    .text_size(px(34.))
                                    .line_height(px(38.))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(c(head_col)),
                            )
                            .child(div().pb(px(5.)).text_color(c(DIM)).child(format!("of {} in use", fmt_kib(mem.total)))),
                    )
                    .child(div().text_color(c(DIM)).child(match s.board.ram_area_c {
                        Some(t) => format!("{} available, {t:.0} °C near the RAM", fmt_kib(mem.available)),
                        None => format!("{} available", fmt_kib(mem.available)),
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_1()
                    .child(
                        mono(format!("{p:.0}%"))
                            .text_size(px(26.))
                            .line_height(px(30.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(c(p_col)),
                    )
                    .child(div().text_color(c(DIM)).child(format!("pressure, apps get killed at {lim:.0}%"))),
            );

        let breakdown = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(stacked(&parts, mem.total, 12.))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_y_2()
                    .child(legend(ACCENT, "Apps", fmt_kib(mem.anon)))
                    .child(legend(ACCENT_2, "GPU and shared", fmt_kib(shared)))
                    .child(legend(ACCENT_3, "/tmp in RAM", fmt_kib(tmpfs)))
                    .child(legend(KERNEL, "Kernel", fmt_kib(mem.kernel)))
                    .child(legend(CACHE, "Cache, freeable", fmt_kib(mem.file_cache)))
                    .child(legend(TRACK, "Free", fmt_kib(mem.free))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().w(px(40.)).text_color(c(DIM)).child("Swap"))
                    .child(div().flex_1().child(stacked(
                        &[(if mem.swap_used * 10 > mem.swap_total * 8 { WARN } else { KERNEL }, mem.swap_used)],
                        mem.swap_total,
                        6.,
                    )))
                    .child(
                        mono(format!("{} of {}", fmt_kib(mem.swap_used), fmt_kib(mem.swap_total)))
                            .text_color(c(DIM)),
                    ),
            );

        let why = insight_list(&m.insights);

        let pending = m.pending_quit.as_ref().map(|(n, _)| n.as_str());
        let col_head = |w: f32, t: &'static str| div().w(px(w)).text_right().child(t);
        let apps = section("Apps")
            .child(
                div()
                    .flex()
                    .gap_3()
                    .px_2()
                    .text_size(px(11.5))
                    .text_color(c(FAINT))
                    .whitespace_nowrap()
                    .child(div().flex_1().min_w(px(140.)).child("Name"))
                    .child(col_head(COLS[0], "RAM"))
                    .child(col_head(COLS[1], "Swap"))
                    .child(col_head(COLS[2], "GPU"))
                    .when(wide, |d| d.child(col_head(COLS[3], "Last hour")))
                    .child(col_head(COLS[4], "MB/h"))
                    .when(wide, |d| d.child(col_head(COLS[5], "Running")))
                    .child(div().w(px(64.))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child({
                        let shown: Vec<_> = s.apps.iter().take(20).collect();
                        sliding_rows(
                            shown.len(),
                            shown
                                .iter()
                                .map(|a| {
                                    (
                                        ys.get(&a.name).copied().unwrap_or(0.0),
                                        app_row(a, pending, &self.monitor, wide).into_any_element(),
                                    )
                                })
                                .collect(),
                        )
                    }),
            );

        let kills = section("Killed for memory, last 7 days").child(if m.kills.is_empty() {
            div().text_color(c(DIM)).child("Nothing was killed for memory in the last 7 days.")
        } else {
            div().flex().flex_col().gap_1().children(m.kills.iter().take(10).map(|k| {
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .h(px(24.))
                    .whitespace_nowrap()
                    .px_2()
                    .child(mono(k.when.clone()).w(px(104.)).text_color(c(DIM)))
                    .child(div().w(px(120.)).truncate().text_color(c(TEXT)).child(k.name.clone()))
                    .child(
                        div()
                            .w(px(104.))
                            .whitespace_nowrap()
                            .text_color(c(if k.how == "kernel" { BAD } else { WARN }))
                            .child(if k.how == "kernel" { "kernel OOM" } else { "systemd-oomd" }),
                    )
                    .child(div().flex_1().truncate().text_color(c(DIM)).child(k.detail.clone()))
            }))
        });

        let footer = mono(match m.tab {
            Tab::Memory => format!("Refreshes every 2 s. Last scan took {} ms.", s.took.as_millis().max(1)),
            Tab::Cpu => format!("Refreshes every 2 s. Per-app CPU: 100% is one full core. Scan {} ms.", s.took.as_millis().max(1)),
            Tab::Gpu => format!("Refreshes every 2 s. Per-app GPU: share of that GPU's busiest engine. Scan {} ms.", s.took.as_millis().max(1)),
        })
            .text_size(px(11.))
            .whitespace_nowrap()
            .text_color(c(FAINT));

        let gpu_max = s.gpu.devices.iter().filter(|d| !d.asleep).map(|d| d.busy).fold(None, |a: Option<f32>, b| Some(a.map_or(b, |a| a.max(b))));
        let summary = match gpu_max {
            Some(g) => format!("RAM {:.0}%  CPU {:.0}%  GPU {:.0}%", used_frac * 100.0, s.cpu.total, g),
            None => format!("RAM {:.0}%  CPU {:.0}%", used_frac * 100.0, s.cpu.total),
        };
        let tab = m.tab;
        let body = match tab {
            Tab::Memory => div().flex().flex_col().gap_5().child(overview).child(breakdown).child(why).child(apps).child(kills),
            Tab::Cpu => cpu_view(&s.cpu, &m.cpu_insights, pending, &self.monitor, wide, &ys, ceiling, shift, &s.gpu, &s.board),
            Tab::Gpu => gpu_view(&s.gpu, &m.gpu_insights, pending, &self.monitor, wide, &ys, &gpu_shifts, &s.board),
        };
        let content = div()
            .id("content")
            .flex_1()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .px_5()
                    .pt_5()
                    .pb_6()
                    .child(body)
                    .child(footer),
            );

        // Keep drawing only while something is still easing toward the
        // latest sample; once settled (about 0.4 s), no frames until the next.
        if self.tw.moving {
            window.request_animation_frame();
        }

        let frame = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(c(BG))
            .font_family(SANS)
            .text_size(px(13.))
            .text_color(c(TEXT))
            .cursor(CursorStyle::Arrow)
            .when(csd && !maximized, |d| d.border_1().border_color(c(HAIR)).rounded(px(8.)))
            .overflow_hidden()
            .when(csd, |d| d.child(titlebar(summary, tab, &self.monitor)))
            .child(content);

        if !csd || maximized {
            window.set_client_inset(px(0.));
            return div().size_full().child(frame);
        }
        // client-side decorations: a transparent EDGE-px margin around the
        // frame carries the resize cursor and grab area
        window.set_client_inset(px(EDGE));
        div()
            .size_full()
            .p(px(EDGE))
            .child(
                canvas(
                        |_b, window, _cx| {
                            window.insert_hitbox(
                                Bounds::new(point(px(0.), px(0.)), window.window_bounds().get_bounds().size),
                                HitboxBehavior::Normal,
                            )
                        },
                        |_b, hitbox, window, _cx| {
                            let size = window.window_bounds().get_bounds().size;
                            if let Some(edge) = resize_edge(window.mouse_position(), size) {
                                window.set_cursor_style(cursor_for(edge), &hitbox);
                            }
                        },
                    )
                    .size_full()
                    .absolute(),
            )
            .on_mouse_move(|_e, window, _cx| window.refresh())
            .on_mouse_down(MouseButton::Left, |e, window, _cx| {
                let size = window.window_bounds().get_bounds().size;
                if let Some(edge) = resize_edge(e.position, size) {
                    window.start_window_resize(edge);
                }
            })
            .child(
                frame.shadow(vec![gpui::BoxShadow {
                    color: gpui::hsla(0., 0., 0., 0.45),
                    blur_radius: px(EDGE),
                    spread_radius: px(0.),
                    offset: point(px(0.), px(1.)),
                }]),
            )
    }
}

/// "3,740 rpm" — fans only report to tens of rpm.
fn fmt_rpm(r: u32) -> String {
    let r = (r + 5) / 10 * 10;
    if r >= 1000 { format!("{},{:03} rpm", r / 1000, r % 1000) } else { format!("{r} rpm") }
}
