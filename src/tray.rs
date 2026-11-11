//! Top-bar icon (StatusNotifierItem — shown by Ubuntu's AppIndicator extension).
//! Draws "used %" in a 3x5 pixel font with a pressure-coloured bar under it.
//! With CPU or GPU also chosen in Settings, the digits show the first chosen
//! value and each chosen value gets its own thinner bar; the title and tooltip
//! spell all of them out.

use futures::channel::mpsc::UnboundedSender as Sender;

use ksni::blocking::{Handle, TrayMethods};

use crate::settings::BarItem;
use crate::{BarState, Cmd};

pub struct RamTray {
    pub state: BarState,
    tx: Sender<Cmd>,
}

const DIGITS: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b001, 0b001, 0b001],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

const N: i32 = 32;

fn level_rgb(level: u8) -> [u8; 3] {
    match level {
        0 => [0x3f, 0xb9, 0x50],
        1 => [0xe3, 0xb3, 0x41],
        _ => [0xf8, 0x51, 0x49],
    }
}

/// CPU and GPU have no insight levels of their own here; colour by value.
fn busy_level(v: u8) -> u8 {
    if v >= 90 {
        2
    } else if v >= 75 {
        1
    } else {
        0
    }
}

pub fn draw(s: &BarState) -> ksni::Icon {
    let items = if s.items.is_empty() { vec![(BarItem::Ram, s.used_pct)] } else { s.items.clone() };
    let lvl = |item: BarItem, v: u8| if item == BarItem::Ram { s.level } else { busy_level(v) };
    let (first_item, first) = items[0];
    let level = lvl(first_item, first);
    let mut px = vec![0u8; (N * N * 4) as usize];
    let mut put = |x: i32, y: i32, argb: [u8; 4]| {
        if (0..N).contains(&x) && (0..N).contains(&y) {
            let i = ((y * N + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&argb);
        }
    };
    let pct = first.min(99);
    let digits = [pct / 10, pct % 10];
    let (sx, sy) = (4, 4); // pixel scale
    let text = if level == 0 { [0xff, 0xff, 0xff] } else { level_rgb(level) };
    for (n, d) in digits.iter().enumerate() {
        let ox = 3 + n as i32 * (3 * sx + 2);
        for (row, bits) in DIGITS[*d as usize].iter().enumerate() {
            for col in 0..3 {
                if bits & (0b100 >> col) != 0 {
                    for dy in 0..sy {
                        for dx in 0..sx {
                            put(ox + col * sx + dx, 1 + row as i32 * sy + dy, [0xff, text[0], text[1], text[2]]);
                        }
                    }
                }
            }
        }
    }
    // bars: one full-height bar as always, or two or three thinner ones
    let rows: &[(i32, i32)] = match items.len() {
        1 => &[(25, 31)],
        2 => &[(23, 26), (28, 31)],
        _ => &[(23, 25), (26, 28), (29, 31)],
    };
    for (&(item, v), &(y0, y1)) in items.iter().zip(rows) {
        let fill = (v as i32 * N / 100).clamp(1, N);
        let c = level_rgb(lvl(item, v));
        for y in y0..y1 {
            for x in 0..N {
                if x < fill {
                    put(x, y, [0xff, c[0], c[1], c[2]]);
                } else {
                    put(x, y, [0x70, 0xff, 0xff, 0xff]);
                }
            }
        }
    }
    ksni::Icon { width: N, height: N, data: px }
}

impl ksni::Tray for RamTray {
    fn id(&self) -> String {
        "headroom".into()
    }
    fn title(&self) -> String {
        self.state.text()
    }
    fn category(&self) -> ksni::Category {
        ksni::Category::SystemServices
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![draw(&self.state)]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: self.title(), description: self.state.tip.clone(), ..Default::default() }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.unbounded_send(Cmd::Show);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: format!("Open Headroom ({}% used)", self.state.used_pct),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.unbounded_send(Cmd::Show);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit Headroom".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.unbounded_send(Cmd::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub fn spawn(tx: Sender<Cmd>) -> Option<Handle<RamTray>> {
    RamTray { state: BarState::default(), tx }
        .spawn()
        .map_err(|e| eprintln!("headroom: tray unavailable: {e}"))
        .ok()
}
