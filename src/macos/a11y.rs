//! The Linux build publishes its own AT-SPI tree. AT-SPI does not exist on
//! macOS, so this keeps the same interface and does nothing.

use futures::channel::mpsc::UnboundedSender;

use crate::{Cmd, Monitor, Tab};

/// What a clickable node does.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum A11yAct {
    Tab(Tab),
    Quit(String),
}

pub struct A11y;

impl A11y {
    pub fn new(_tx: UnboundedSender<Cmd>) -> Self {
        A11y
    }

    pub fn set_focused(&mut self, _focused: bool) {}

    pub fn sync(&mut self, _m: &Monitor) {}
}
