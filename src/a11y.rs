//! Accessibility: gpui 0.2.2 has none, so Headroom publishes its own AT-SPI
//! tree through AccessKit. Screen readers and automation tools (agent-desktop)
//! can then read every number on screen, switch tabs and press Quit.
//!
//! The tree is rebuilt from `Monitor` state only while an assistive client is
//! listening (`update_if_active`), so it costs nothing otherwise.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use accesskit::{Action, ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, Node, NodeId, Role, TreeId, TreeInfo, TreeUpdate};
use futures::channel::mpsc::UnboundedSender;

use crate::collect::{fmt_age, fmt_kib};
use crate::{Cmd, Monitor, Tab};

/// What a clickable node does.
#[derive(Clone, Debug)]
pub enum A11yAct {
    Tab(Tab),
    Quit(String),
}

type Actions = Arc<Mutex<HashMap<NodeId, A11yAct>>>;

struct OnActivate(UnboundedSender<Cmd>);
impl ActivationHandler for OnActivate {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        // The tree lives on the UI thread; ask it to push one (the adapter
        // stays Pending until then).
        let _ = self.0.unbounded_send(Cmd::A11ySync);
        None
    }
}

struct OnAction(UnboundedSender<Cmd>, Actions);
impl ActionHandler for OnAction {
    fn do_action(&mut self, req: ActionRequest) {
        if !matches!(req.action, Action::Click | Action::Focus) {
            return;
        }
        if let Some(act) = self.1.lock().unwrap().get(&req.target_node).cloned() {
            let _ = self.0.unbounded_send(Cmd::A11y(act));
        }
    }
}

struct OnDeactivate;
impl DeactivationHandler for OnDeactivate {
    fn deactivate_accessibility(&mut self) {}
}

pub struct A11y {
    adapter: accesskit_unix::Adapter,
    actions: Actions,
}

impl A11y {
    pub fn new(tx: UnboundedSender<Cmd>) -> Self {
        let actions: Actions = Arc::new(Mutex::new(HashMap::new()));
        let adapter =
            accesskit_unix::Adapter::new(OnActivate(tx.clone()), OnAction(tx, actions.clone()), OnDeactivate);
        Self { adapter, actions }
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.adapter.update_window_focus_state(focused);
    }

    /// Pushes a fresh tree if an assistive client is listening.
    pub fn sync(&mut self, m: &Monitor) {
        let actions = self.actions.clone();
        self.adapter.update_if_active(|| {
            let (update, acts) = build(m);
            *actions.lock().unwrap() = acts;
            update
        });
    }
}

struct Builder {
    nodes: Vec<(NodeId, Node)>,
    acts: HashMap<NodeId, A11yAct>,
    next: u64,
}

impl Builder {
    fn id(&mut self) -> NodeId {
        self.next += 1;
        NodeId(self.next)
    }

    fn add(&mut self, node: Node) -> NodeId {
        let id = self.id();
        self.nodes.push((id, node));
        id
    }

    fn label(&mut self, text: impl Into<String>) -> NodeId {
        let mut n = Node::new(Role::Label);
        n.set_value(text.into());
        self.add(n)
    }

    fn group(&mut self, role: Role, name: &str, children: Vec<NodeId>) -> NodeId {
        let mut n = Node::new(role);
        n.set_label(name.to_string());
        n.set_children(children);
        self.add(n)
    }

    fn button(&mut self, name: String, act: A11yAct) -> NodeId {
        let mut n = Node::new(Role::Button);
        n.set_label(name);
        n.add_action(Action::Click);
        n.add_action(Action::Focus);
        let id = self.add(n);
        self.acts.insert(id, act);
        id
    }

    /// One table row: a name cell, one cell per "column: value", optional Quit.
    fn row(&mut self, name: &str, cells: Vec<(&str, String)>, quit: Option<A11yAct>, confirming: bool) -> NodeId {
        // AT-SPI shows a cell's name, not its value, so the name carries both.
        let mut kids = vec![{
            let mut n = Node::new(Role::Cell);
            n.set_label(name.to_string());
            self.add(n)
        }];
        for (col, v) in cells {
            let mut n = Node::new(Role::Cell);
            n.set_label(format!("{col}: {v}"));
            kids.push(self.add(n));
        }
        if let Some(act) = quit {
            let label = if confirming { format!("Confirm quit {name}") } else { format!("Quit {name}") };
            kids.push(self.button(label, act));
        }
        let mut n = Node::new(Role::Row);
        n.set_label(name.to_string());
        n.set_children(kids);
        self.add(n)
    }
}

fn build(m: &Monitor) -> (TreeUpdate, HashMap<NodeId, A11yAct>) {
    let mut b = Builder { nodes: Vec::new(), acts: HashMap::new(), next: 1 };
    let root = NodeId(1);
    let s = &m.snap;
    let pending = m.pending_quit.as_ref().map(|(n, _)| n.as_str());

    // tabs
    let mut tabs = Vec::new();
    for (name, tab) in [("Memory", Tab::Memory), ("CPU", Tab::Cpu), ("GPU", Tab::Gpu)] {
        let mut n = Node::new(Role::Tab);
        n.set_label(name.to_string());
        n.set_selected(m.tab == tab);
        n.add_action(Action::Click);
        n.add_action(Action::Focus);
        let id = b.add(n);
        b.acts.insert(id, A11yAct::Tab(tab));
        tabs.push(id);
    }
    let tablist = b.group(Role::TabList, "View", tabs);
    let mut sections = vec![tablist];

    let insight_texts: Vec<String>;
    match m.tab {
        Tab::Memory => {
            let mem = &s.mem;
            let summary = vec![
                b.label(format!("RAM in use: {} of {}", fmt_kib(mem.used()), fmt_kib(mem.total))),
                b.label(format!("Available: {}", fmt_kib(mem.available))),
                b.label(format!(
                    "Memory pressure: {:.0}%, apps get killed at {:.0}%",
                    s.user_psi.full10, m.policy.limit_pct
                )),
                b.label(format!("Swap: {} of {}", fmt_kib(mem.swap_used), fmt_kib(mem.swap_total))),
            ];
            let mut summary = summary;
            if let Some(t) = s.board.ram_area_c {
                summary.push(b.label(format!("Temperature near the RAM: {t:.0} °C")));
            }
            sections.push(b.group(Role::Group, "Summary", summary));
            let tmpfs = s.tmpfs.min(mem.shmem);
            let parts = vec![
                b.label(format!("Apps: {}", fmt_kib(mem.anon))),
                b.label(format!("GPU and shared: {}", fmt_kib(mem.shmem - tmpfs))),
                b.label(format!("/tmp in RAM: {}", fmt_kib(tmpfs))),
                b.label(format!("Kernel: {}", fmt_kib(mem.kernel))),
                b.label(format!("Cache, freeable: {}", fmt_kib(mem.file_cache))),
                b.label(format!("Free: {}", fmt_kib(mem.free))),
            ];
            sections.push(b.group(Role::Group, "Where it goes", parts));
            insight_texts = m.insights.iter().map(|i| i.text.clone()).collect();
            let rows: Vec<NodeId> = s
                .apps
                .iter()
                .take(20)
                .map(|a| {
                    let quit = (!crate::ui::NO_QUIT.contains(&a.name.as_str())).then(|| A11yAct::Quit(a.name.clone()));
                    b.row(
                        &a.name,
                        vec![
                            ("RAM", fmt_kib(a.pss)),
                            ("Swap", fmt_kib(a.swap)),
                            ("GPU", fmt_kib(a.gpu)),
                            ("MB per hour", a.growth_mb_h.map(|g| format!("{g:+.0}")).unwrap_or_else(|| "-".into())),
                            ("Running", fmt_age(a.age_s)),
                        ],
                        quit,
                        pending == Some(a.name.as_str()),
                    )
                })
                .collect();
            let insights_ids: Vec<NodeId> = insight_texts.iter().map(|t| b.label(t.clone())).collect();
            sections.push(b.group(Role::Group, "What needs attention", insights_ids));
            sections.push(b.group(Role::Table, "Apps", rows));
            let kills: Vec<NodeId> = m
                .kills
                .iter()
                .take(10)
                .map(|k| b.label(format!("{}: {} killed by {}, {}", k.when, k.name, k.how, k.detail)))
                .collect();
            sections.push(b.group(Role::List, "Killed for memory, last 7 days", kills));
        }
        Tab::Cpu => {
            let c = &s.cpu;
            let mut summary = vec![
                b.label(format!("CPU busy: {:.0}% across {} threads", c.total, c.cores.len())),
                b.label(format!("Load: {:.2}, {:.2}, {:.2} over 1, 5 and 15 min", c.load[0], c.load[1], c.load[2])),
            ];
            // clock and temperature are only read with the window open
            if c.freq_mhz > 0 {
                summary.push(b.label(format!(
                    "Clock: {:.1} of {:.1} GHz",
                    c.freq_mhz as f32 / 1000.0,
                    c.max_mhz as f32 / 1000.0
                )));
            }
            if let Some(t) = c.temp_c {
                summary.push(b.label(format!("Temperature: {t:.0} °C")));
            }
            if let Some(r) = s.board.cpu_fan_rpm {
                summary.push(b.label(format!("CPU fan: {r} rpm")));
            }
            sections.push(b.group(Role::Group, "Summary", summary));
            let cores: Vec<NodeId> =
                c.cores.iter().enumerate().map(|(i, p)| b.label(format!("Core {i}: {p:.0}%"))).collect();
            sections.push(b.group(Role::Group, "Cores", cores));
            let insights_ids: Vec<NodeId> = m.cpu_insights.iter().map(|i| b.label(i.text.clone())).collect();
            sections.push(b.group(Role::Group, "What needs attention", insights_ids));
            let rows: Vec<NodeId> = c
                .apps
                .iter()
                .take(20)
                .map(|a| {
                    let quit = (!crate::ui::NO_QUIT.contains(&a.name.as_str())).then(|| A11yAct::Quit(a.name.clone()));
                    b.row(
                        &a.name,
                        vec![
                            ("CPU", format!("{:.1}%", a.pct)),
                            ("CPU time", format!("{} s", a.cpu_secs)),
                            ("Threads", a.threads.to_string()),
                            ("Running", fmt_age(a.age_s)),
                        ],
                        quit,
                        pending == Some(a.name.as_str()),
                    )
                })
                .collect();
            let rows = if rows.is_empty() {
                vec![b.label("Per-app CPU is measured while the Headroom window is open on the CPU tab.")]
            } else {
                rows
            };
            sections.push(b.group(Role::Table, "Apps", rows));
        }
        Tab::Gpu => {
            let g = &s.gpu;
            for d in &g.devices {
                let mut kids = Vec::new();
                let dgpu = crate::gpu::is_discrete(d);
                if d.asleep {
                    kids.push(b.label("Asleep and saving power; not queried"));
                } else {
                    kids.push(b.label(format!("Busy: {:.0}%", d.busy)));
                    for e in &d.engines {
                        kids.push(b.label(format!("{}: {:.0}%", e.name, e.busy)));
                    }
                    if let (Some(f), Some(mx)) = (d.freq_mhz, d.max_mhz) {
                        kids.push(b.label(format!("Clock: {f} of {mx} MHz")));
                    }
                    if let (Some(u), Some(t)) = (d.mem_used, d.mem_total) {
                        kids.push(b.label(format!("Video memory: {} of {}", fmt_kib(u), fmt_kib(t))));
                    }
                    if let Some(t) = d.temp_c {
                        kids.push(b.label(format!("Temperature: {t:.0} °C")));
                    }
                    if let Some(w) = d.power_w {
                        kids.push(b.label(format!("Power: {w:.1} W")));
                    }
                }
                if dgpu {
                    if let Some(t) = s.board.gpu_area_c {
                        kids.push(b.label(format!("Temperature beside it: {t:.0} °C")));
                    }
                    if let Some(r) = s.board.gpu_fan_rpm {
                        kids.push(b.label(format!("Fan: {r} rpm")));
                    }
                }
                sections.push(b.group(Role::Group, &d.name, kids));
            }
            let ids: Vec<NodeId> = m.gpu_insights.iter().map(|i| b.label(i.text.clone())).collect();
            sections.push(b.group(Role::Group, "What needs attention", ids));
            let names: Vec<String> = g.devices.iter().map(|d| d.name.clone()).collect();
            let rows: Vec<NodeId> = g
                .apps
                .iter()
                .take(20)
                .map(|a| {
                    let quit = (!crate::ui::NO_QUIT.contains(&a.name.as_str())).then(|| A11yAct::Quit(a.name.clone()));
                    let mut cells: Vec<(&str, String)> =
                        names.iter().zip(&a.busy).map(|(n, b)| (n.as_str(), format!("{b:.0}%"))).collect();
                    cells.push(("Graphics memory", fmt_kib(a.mem)));
                    cells.push(("Running", fmt_age(a.age_s)));
                    b.row(&a.name, cells, quit, pending == Some(a.name.as_str()))
                })
                .collect();
            let rows = if rows.is_empty() {
                vec![b.label("Per-app GPU use is measured while the window is open on the CPU or GPU tab.")]
            } else {
                rows
            };
            sections.push(b.group(Role::Table, "Apps", rows));
        }
    }

    let mut win = Node::new(Role::Window);
    win.set_label("Headroom".to_string());
    win.set_children(sections);
    b.nodes.push((root, win));

    let update = TreeUpdate {
        nodes: b.nodes,
        tree: Some(TreeInfo { root, toolkit_name: Some("gpui".into()), toolkit_version: Some("0.2.2".into()) }),
        tree_id: TreeId::ROOT,
        focus: root,
    };
    (update, b.acts)
}
