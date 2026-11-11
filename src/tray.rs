//! Top-bar icon (StatusNotifierItem — shown by Ubuntu's AppIndicator extension).
//! Draws "used %" in a 3x5 pixel font with a pressure-coloured bar under it.

use futures::channel::mpsc::UnboundedSender as Sender;

use ksni::blocking::{Handle, TrayMethods};

use crate::Cmd;

pub struct RamTray {
    pub used_pct: u8,
    /// 0 = fine, 1 = warn, 2 = bad
    pub level: u8,
    pub tip: String,
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

pub fn draw(used_pct: u8, level: u8) -> ksni::Icon {
    let mut px = vec![0u8; (N * N * 4) as usize];
    let mut put = |x: i32, y: i32, argb: [u8; 4]| {
        if (0..N).contains(&x) && (0..N).contains(&y) {
            let i = ((y * N + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&argb);
        }
    };
    let pct = used_pct.min(99);
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
    // bar
    let fill = (used_pct as i32 * N / 100).clamp(1, N);
    let c = level_rgb(level);
    for y in 25..31 {
        for x in 0..N {
            if x < fill {
                put(x, y, [0xff, c[0], c[1], c[2]]);
            } else {
                put(x, y, [0x70, 0xff, 0xff, 0xff]);
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
        format!("RAM {}%", self.used_pct)
    }
    fn category(&self) -> ksni::Category {
        ksni::Category::SystemServices
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![draw(self.used_pct, self.level)]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: self.title(), description: self.tip.clone(), ..Default::default() }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.unbounded_send(Cmd::Show);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: format!("Open Headroom ({}% used)", self.used_pct),
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
    RamTray { used_pct: 0, level: 0, tip: String::new(), tx }
        .spawn()
        .map_err(|e| eprintln!("headroom: tray unavailable: {e}"))
        .ok()
}
