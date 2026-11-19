//! GNOME's input-source switching rules for sway.
//!
//! One owner on the GLib main loop holds the switching state. sway's window,
//! workspace, input and tick events and the input-sources settings changes
//! reach it as messages through one channel and are processed in order.
//! Commands this module sends are fenced with a SEND_TICK: layout events seen
//! before the fence's tick are the daemon's own and are never taken for a
//! user's switch.
//!
//! The owner of the handlers tells it about every sway the daemon reaches,
//! once the source list is applied there. Each starts afresh, as at a first
//! launch: nothing the module knew about an earlier sway (its windows, its
//! keyboards) is kept, and an earlier sway's events are left alone.

use crate::input_sources::effective_sources;
use crate::sway::{self, Sway};
use gio::prelude::*;
use gio::Settings;
use glib::ToVariant;
use log::{debug, info, warn};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use swayipc::{
    Event, EventType, Fallible, InputChange, Node, NodeType, WindowChange, WorkspaceChange,
};

const FENCE: &str = "regolith-inputd:sources:";

/// sway's input event type, without the event bit.
const INPUT_EVENT: u32 = 21;

type Source = (String, String);

/// An input device as sway describes it. Decoded leniently: keyboards
/// without vendor/product ids (virtual keyboards, the wlroots X11/Wayland
/// backends' keyboards) are keyboards like any other.
#[derive(Deserialize)]
struct Device {
    identifier: String,
    #[serde(rename = "type")]
    input_type: String,
    #[serde(default)]
    xkb_layout_names: Vec<String>,
    xkb_active_layout_index: Option<i64>,
}

#[derive(Deserialize)]
struct DeviceEvent {
    change: InputChange,
    input: Device,
}

/// What the owner hears, in order. Events name the sway instance they come
/// from.
enum Msg {
    Sway(u64, Event),
    Input(u64, DeviceEvent),
    Setting(String),
    /// A newly reached sway, with the source list applied.
    Connected(Arc<Sway>),
}

/// Tells the source switching rules about every sway the daemon reaches.
pub struct SourceSwitching(glib::Sender<Msg>);

impl SourceSwitching {
    /// Follow `sway`, whose source list was just applied, as at a first
    /// launch.
    pub(crate) fn follow(&self, sway: Arc<Sway>) {
        let _ = self.0.send(Msg::Connected(sway));
    }
}

struct Keyboard {
    layouts: usize,
    index: i64,
}

struct Switcher {
    settings: Settings,
    /// The sway followed, if any.
    sway: Option<Arc<Sway>>,
    /// The owner's queue, which a new sway's events join.
    queue: glib::Sender<Msg>,
    sources: Vec<Source>,
    /// Listed sources that were chosen or made current, most recent first.
    /// The never-chosen ones are not in it.
    recent: Vec<Source>,
    current: Option<Source>,
    per_window: bool,
    windows: HashMap<i64, Source>,
    focused: Option<i64>,
    keyboards: HashMap<String, Keyboard>,
    fence_sent: u64,
    fence_seen: u64,
}

fn is_view(n: &Node) -> bool {
    matches!(n.node_type, NodeType::Con | NodeType::FloatingCon)
        && n.nodes.is_empty()
        && n.floating_nodes.is_empty()
        && (n.pid.is_some() || n.window.is_some() || n.app_id.is_some())
}

fn find_focused(n: &Node) -> Option<&Node> {
    if n.focused {
        return Some(n);
    }
    n.nodes
        .iter()
        .chain(n.floating_nodes.iter())
        .find_map(find_focused)
}

fn has_views(n: &Node) -> bool {
    n.nodes
        .iter()
        .chain(n.floating_nodes.iter())
        .any(|c| is_view(c) || has_views(c))
}

impl Switcher {
    fn new(queue: glib::Sender<Msg>) -> Switcher {
        let settings = Settings::new("org.gnome.desktop.input-sources");
        Switcher {
            settings,
            sway: None,
            queue,
            sources: Vec::new(),
            recent: Vec::new(),
            current: None,
            per_window: false,
            windows: HashMap::new(),
            focused: None,
            keyboards: HashMap::new(),
            fence_sent: 0,
            fence_seen: 0,
        }
    }

    /// The effective source list: what layout index i of sway's layouts is.
    fn read_sources(&self) -> Vec<Source> {
        effective_sources(self.settings.get::<Vec<Source>>("sources"))
            .into_iter()
            .map(|e| e.source)
            .collect()
    }

    fn index_of(&self, s: &Source) -> Option<usize> {
        self.sources.iter().position(|x| x == s)
    }

    /// Keep only the listed sources of `history`, once each, in order.
    fn set_recent(&mut self, history: Vec<Source>) {
        let mut recent: Vec<Source> = Vec::new();
        for s in history {
            if self.sources.contains(&s) && !recent.contains(&s) {
                recent.push(s);
            }
        }
        self.recent = recent;
    }

    /// The value `mru-sources` gets: every listed source, the most recently
    /// chosen first, then the never-chosen ones in list order.
    fn history(&self) -> Vec<Source> {
        let mut h = self.recent.clone();
        h.extend(
            self.sources
                .iter()
                .filter(|s| !self.recent.contains(s))
                .cloned(),
        );
        h
    }

    fn startup(&mut self) {
        self.sources = self.read_sources();
        self.per_window = self.settings.boolean("per-window");
        let saved: Vec<Source> = self.settings.get::<Vec<Source>>("mru-sources");
        self.set_recent(saved);
        self.refresh_keyboards();
        self.focused = self.query_focused();
        let first = self.recent.first().or(self.sources.first()).cloned();
        if let Some(first) = first {
            info!("Starting on input source {first:?}");
            self.activate(first, false);
        }
    }

    /// Follow a sway that was just reached, forgetting the earlier one.
    fn connected(&mut self, sway: Arc<Sway>) {
        self.sway = None;
        self.current = None;
        self.windows.clear();
        self.focused = None;
        self.keyboards.clear();
        self.fence_sent = 0;
        self.fence_seen = 0;
        // Subscribe before reading the initial state, so no event is missed.
        let generation = sway.generation();
        let queue = self.queue.clone();
        let followed = sway.follow(&subscription(), move |kind, payload| {
            match decode(generation, (kind, payload)) {
                Ok(msg) => queue.send(msg).is_ok(),
                // A message that doesn't decode was still read whole: skip
                // it, the stream stays in step.
                Err(e) => {
                    debug!("{e}");
                    true
                }
            }
        });
        if let Err(e) = followed {
            // The owner of the handlers looks for sway again.
            return debug!("Not following input sources: {e}");
        }
        self.sway = Some(sway);
        self.startup();
    }

    /// Whether events of sway instance `generation` are followed.
    fn follows(&self, generation: u64) -> bool {
        self.sway
            .as_ref()
            .is_some_and(|sway| sway.generation() == generation)
    }

    fn query_focused(&mut self) -> Option<i64> {
        let tree = self.sway.as_ref()?.get_tree().ok()?;
        let n = find_focused(&tree)?;
        if is_view(n) {
            Some(n.id)
        } else {
            None
        }
    }

    fn refresh_keyboards(&mut self) {
        let inputs = match self.sway.as_ref().ok_or_else(sway::gone) {
            Ok(sway) => sway.get_inputs_json(),
            Err(e) => Err(e),
        };
        let inputs = match inputs {
            Ok(i) => i,
            Err(e) => {
                if !sway::is_gone(&e) {
                    warn!("{e}");
                }
                return;
            }
        };
        self.keyboards.clear();
        let devices = inputs
            .into_iter()
            .filter_map(|i| serde_json::from_value::<Device>(i).ok());
        for i in devices.filter(|i| i.input_type == "keyboard") {
            self.keyboards.insert(
                i.identifier,
                Keyboard {
                    layouts: i.xkb_layout_names.len(),
                    index: i.xkb_active_layout_index.unwrap_or(0),
                },
            );
        }
    }

    fn fenced(&self) -> bool {
        self.fence_seen < self.fence_sent
    }

    fn send_fence(&mut self) {
        self.fence_sent += 1;
        let payload = format!("{FENCE}{}", self.fence_sent);
        if let Some(c) = self.sway.as_ref() {
            if c.send_tick(&payload).is_err() {
                // No tick support (or no sway): nothing will clear it.
                self.fence_seen = self.fence_sent;
            }
        } else {
            self.fence_seen = self.fence_sent;
        }
    }

    /// Put every keyboard that has the source list's layouts on `index`.
    fn switch_all(&mut self, index: usize) {
        let n = self.sources.len();
        let needed = self
            .keyboards
            .values()
            .any(|k| k.layouts == n && k.index != index as i64);
        if !needed {
            return;
        }
        let cmd = format!("input type:keyboard xkb_switch_layout {index}");
        debug!("{cmd}");
        if let Some(c) = self.sway.as_ref() {
            match c.run_command(&cmd) {
                Err(e) if !sway::is_gone(&e) => warn!("{e}"),
                _ => (),
            }
        }
        self.send_fence();
    }

    fn activate(&mut self, s: Source, interactive: bool) {
        let Some(index) = self.index_of(&s) else {
            return;
        };
        self.current = Some(s.clone());
        self.recent.retain(|x| x != &s);
        self.recent.insert(0, s.clone());
        if interactive {
            let v = self.history().to_variant();
            if let Err(e) = self.settings.set_value("mru-sources", &v) {
                warn!("{e}");
            }
        }
        if self.per_window {
            if let Some(w) = self.focused {
                self.windows.insert(w, s);
            }
        }
        self.switch_all(index);
    }

    fn window_source(&self, id: i64) -> Option<Source> {
        match self.windows.get(&id) {
            Some(s) if self.sources.contains(s) => Some(s.clone()),
            _ => self.sources.first().cloned(),
        }
    }

    fn reassert(&mut self) {
        if let Some(i) = self.current.as_ref().and_then(|c| self.index_of(c)) {
            self.switch_all(i);
        }
    }

    fn sources_changed(&mut self) {
        self.sources = self.read_sources();
        let history = std::mem::take(&mut self.recent);
        self.set_recent(history);
        let keep = self.current.clone().filter(|c| self.sources.contains(c));
        let fallback = || self.recent.first().or(self.sources.first()).cloned();
        match keep.or_else(fallback) {
            Some(s) => {
                self.current = None;
                self.activate(s, false);
            }
            None => self.current = None,
        }
        // The keymap event (or the fence) re-asserts once sway has the new list.
        self.send_fence();
    }

    fn per_window_changed(&mut self) {
        self.per_window = self.settings.boolean("per-window");
        self.windows.clear();
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Window(w) => match w.change {
                WindowChange::Focus => {
                    self.focused = Some(w.container.id);
                    if self.per_window {
                        if let Some(s) = self.window_source(w.container.id) {
                            self.activate(s, false);
                        }
                    }
                }
                WindowChange::Close => {
                    self.windows.remove(&w.container.id);
                    if self.focused == Some(w.container.id) {
                        self.focused = None;
                    }
                }
                _ => {}
            },
            Event::Workspace(ws) => {
                if ws.change == WorkspaceChange::Focus {
                    if let Some(cur) = ws.current.as_ref() {
                        if !has_views(cur) {
                            self.focused = None;
                        }
                    }
                }
            }
            Event::Tick(t) => {
                if let Some(n) = t.payload.strip_prefix(FENCE) {
                    if let Ok(n) = n.parse::<u64>() {
                        self.fence_seen = self.fence_seen.max(n);
                        if !self.fenced() {
                            self.reassert();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn on_input(&mut self, ie: DeviceEvent) {
        if ie.input.input_type != "keyboard" {
            return;
        }
        let id = ie.input.identifier;
        let layouts = ie.input.xkb_layout_names.len();
        let index = ie.input.xkb_active_layout_index.unwrap_or(0);
        match ie.change {
            InputChange::Removed => {
                self.keyboards.remove(&id);
            }
            InputChange::Added | InputChange::XkbKeymap => {
                self.keyboards.insert(id, Keyboard { layouts, index });
                // Hotplug or a new keymap: sway may have reset layouts.
                self.send_fence();
            }
            InputChange::XkbLayout => {
                self.keyboards.insert(id, Keyboard { layouts, index });
                if self.fenced() || layouts != self.sources.len() {
                    return;
                }
                let cur = self.current.as_ref().and_then(|c| self.index_of(c));
                if cur != Some(index as usize) {
                    if let Some(s) = self.sources.get(index as usize).cloned() {
                        info!("User switched to {s:?}");
                        self.activate(s, true);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The events the switcher follows. The stream is read by hand rather than
/// through swayipc's `EventStream`, whose typed input events require
/// vendor/product ids.
fn subscription() -> String {
    let events = [
        EventType::Window,
        EventType::Workspace,
        EventType::Input,
        EventType::Tick,
    ];
    serde_json::to_string(&events).unwrap_or_default()
}

fn decode(generation: u64, (kind, payload): (u32, Vec<u8>)) -> Fallible<Msg> {
    if kind & 0x7fff_ffff == INPUT_EVENT {
        Ok(Msg::Input(generation, serde_json::from_slice(&payload)?))
    } else {
        Ok(Msg::Sway(generation, Event::decode((kind, payload))?))
    }
}

/// Start following GNOME's input-source rules, on each sway the returned
/// handle is told about. Must run on the thread that runs the default GLib
/// main loop.
pub fn start() -> SourceSwitching {
    let (tx, rx) = glib::MainContext::channel::<Msg>(glib::PRIORITY_DEFAULT);
    let sw = Rc::new(RefCell::new(Switcher::new(tx.clone())));
    let switching = SourceSwitching(tx.clone());

    // Settings changes join the same queue. The owner's own mru-sources write
    // emits "changed" too; only the keys it follows are queued.
    sw.borrow().settings.connect_changed(None, move |_, key| {
        if key == "sources" || key == "per-window" {
            let _ = tx.send(Msg::Setting(key.to_string()));
        }
    });

    let owner = sw;
    rx.attach(None, move |msg| {
        let mut s = owner.borrow_mut();
        match msg {
            Msg::Sway(generation, ev) if s.follows(generation) => s.on_event(ev),
            Msg::Input(generation, ev) if s.follows(generation) => s.on_input(ev),
            // An earlier sway's.
            Msg::Sway(..) | Msg::Input(..) => (),
            Msg::Setting(key) if key == "sources" => s.sources_changed(),
            Msg::Setting(_) => s.per_window_changed(),
            Msg::Connected(sway) => s.connected(sway),
        }
        glib::Continue(true)
    });
    switching
}
