//! Menu bar item (NSStatusItem): the values chosen in Settings as short text
//! ("RAM 63%  CPU 8%"), amber or red when memory needs attention, with a menu
//! to open the window or quit. Created and updated on the main thread only
//! (the refresh loop runs there), and redrawn only when the text changes.

use std::cell::RefCell;
use std::ffi::CString;
use std::sync::OnceLock;

use futures::channel::mpsc::UnboundedSender as Sender;
use objc::declare::ClassDecl;
use objc::runtime::{Object, Sel};
use objc::{class, msg_send, sel, sel_impl};

use crate::{BarState, Cmd};

type Id = *mut Object;

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSForegroundColorAttributeName: Id;
    static NSFontAttributeName: Id;
}

static TX: OnceLock<Sender<Cmd>> = OnceLock::new();

pub struct RamTray {
    pub state: BarState,
}

pub struct Handle {
    item: Id,
    open_item: Id,
    tray: RefCell<RamTray>,
    /// What is on screen now: title, level, tooltip, menu label.
    shown: RefCell<Option<(String, u8, String, u8)>>,
}

/// An autoreleased NSString.
unsafe fn ns_string(s: &str) -> Id {
    let c = CString::new(s.replace('\0', "")).unwrap_or_default();
    unsafe { msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()] }
}

extern "C" fn on_open(_: &Object, _: Sel, _: Id) {
    if let Some(tx) = TX.get() {
        let _ = tx.unbounded_send(Cmd::Show);
    }
}

extern "C" fn on_quit(_: &Object, _: Sel, _: Id) {
    if let Some(tx) = TX.get() {
        let _ = tx.unbounded_send(Cmd::Quit);
    }
}

unsafe fn menu_item(title: &str, action: Sel, target: Id) -> Id {
    unsafe {
        let it: Id = msg_send![class!(NSMenuItem), alloc];
        let it: Id = msg_send![it, initWithTitle: ns_string(title) action: action keyEquivalent: ns_string("")];
        let _: () = msg_send![it, setTarget: target];
        it
    }
}

impl Handle {
    pub fn update(&self, f: impl FnOnce(&mut RamTray)) {
        let mut t = self.tray.borrow_mut();
        f(&mut t);
        let s = &t.state;
        let now = (s.text(), s.level, s.tip.clone(), s.used_pct);
        if self.shown.borrow().as_ref() == Some(&now) {
            return;
        }
        objc::rc::autoreleasepool(|| unsafe { self.draw(s) });
        *self.shown.borrow_mut() = Some(now);
    }

    unsafe fn draw(&self, s: &BarState) {
        unsafe {
            let button: Id = msg_send![self.item, button];
            if button.is_null() {
                return;
            }
            let color: Id = match s.level {
                0 => msg_send![class!(NSColor), labelColor],
                1 => msg_send![class!(NSColor), systemOrangeColor],
                _ => msg_send![class!(NSColor), systemRedColor],
            };
            let size: f64 = msg_send![class!(NSFont), systemFontSize];
            // fixed-width digits, so the item doesn't twitch as the number changes
            let font: Id = msg_send![class!(NSFont), monospacedDigitSystemFontOfSize: size weight: 0.0f64];
            let keys = [NSForegroundColorAttributeName, NSFontAttributeName];
            let vals = [color, font];
            let attrs: Id =
                msg_send![class!(NSDictionary), dictionaryWithObjects: vals.as_ptr() forKeys: keys.as_ptr() count: 2usize];
            let title: Id = msg_send![class!(NSAttributedString), alloc];
            let title: Id = msg_send![title, initWithString: ns_string(&s.text()) attributes: attrs];
            let _: () = msg_send![button, setAttributedTitle: title];
            let _: () = msg_send![title, release];
            let _: () = msg_send![button, setToolTip: ns_string(&s.tip)];
            let open = format!("Open Headroom ({}% used)", s.used_pct);
            let _: () = msg_send![self.open_item, setTitle: ns_string(&open)];
        }
    }
}

pub fn spawn(tx: Sender<Cmd>) -> Option<Handle> {
    let _ = TX.set(tx);
    let mut decl = ClassDecl::new("HeadroomMenuTarget", class!(NSObject))?;
    unsafe {
        decl.add_method(sel!(openWindow:), on_open as extern "C" fn(&Object, Sel, Id));
        decl.add_method(sel!(quitApp:), on_quit as extern "C" fn(&Object, Sel, Id));
    }
    let target_class = decl.register();
    let handle = objc::rc::autoreleasepool(|| unsafe {
        let bar: Id = msg_send![class!(NSStatusBar), systemStatusBar];
        // NSVariableStatusItemLength
        let item: Id = msg_send![bar, statusItemWithLength: -1.0f64];
        if item.is_null() {
            return None;
        }
        // The item, its menu and the target live as long as the app.
        let item: Id = msg_send![item, retain];
        let target: Id = msg_send![target_class, new];
        let menu: Id = msg_send![class!(NSMenu), new];
        let _: () = msg_send![menu, setAutoenablesItems: false];
        let open_item = menu_item("Open Headroom", sel!(openWindow:), target);
        let quit_item = menu_item("Quit Headroom", sel!(quitApp:), target);
        let sep: Id = msg_send![class!(NSMenuItem), separatorItem];
        let _: () = msg_send![menu, addItem: open_item];
        let _: () = msg_send![menu, addItem: sep];
        let _: () = msg_send![menu, addItem: quit_item];
        let _: () = msg_send![item, setMenu: menu];
        Some(Handle {
            item,
            open_item,
            tray: RefCell::new(RamTray { state: BarState::default() }),
            shown: RefCell::new(None),
        })
    })?;
    handle.update(|_| {});
    Some(handle)
}
