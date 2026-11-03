//! GNOME's input-source switching rules for sway.
//!
//! One owner on the GLib main loop holds the switching state. sway's window,
//! workspace, input and tick events and the input-sources settings changes
//! reach it as messages through one channel and are processed in order.
//! Commands this module sends are fenced with a SEND_TICK: layout events seen
//! before the fence's tick are the daemon's own and are never taken for a
//! user's switch.

use crate::input_sources::effective_sources;
use crate::utils;
use gio::prelude::*;
use gio::Settings;
use glib::ToVariant;
use log::{debug, info, warn};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::thread;
use std::time::Duration;
use swayipc::{
    CommandType, Connection, Error as SwayError, Event, EventType, Fallible, InputChange, Node,
    NodeType, Success, WindowChange, WorkspaceChange,
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

enum Msg {
    Sway(Event),
    Input(DeviceEvent),
    Setting(String),
}

struct Keyboard {
    layouts: usize,
    index: i64,
}

struct Switcher {
    settings: Settings,
    conn: Option<Connection>,
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
    fn new() -> Switcher {
        let settings = Settings::new("org.gnome.desktop.input-sources");
        let conn = Connection::new().ok();
        Switcher {
            settings,
            conn,
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

    fn query_focused(&mut self) -> Option<i64> {
        let tree = self.conn.as_mut()?.get_tree().ok()?;
        let n = find_focused(&tree)?;
        if is_view(n) {
            Some(n.id)
        } else {
            None
        }
    }

    fn refresh_keyboards(&mut self) {
        let inputs = match utils::get_inputs_json() {
            Ok(i) => i,
            Err(e) => {
                warn!("{e}");
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
        if let Some(c) = self.conn.as_mut() {
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
        if let Some(c) = self.conn.as_mut() {
            if let Err(e) = c.run_command(&cmd) {
                warn!("{e}");
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

/// Subscribe to the events the switcher follows. The stream is read by hand
/// rather than through swayipc's `EventStream`, whose typed input events
/// require vendor/product ids.
fn subscribe() -> Fallible<UnixStream> {
    let mut stream: UnixStream = Connection::new()?.into();
    let events = serde_json::to_string(&[
        EventType::Window,
        EventType::Workspace,
        EventType::Input,
        EventType::Tick,
    ])?;
    stream.write_all(&CommandType::Subscribe.encode_with(&events))?;
    let reply: Success = CommandType::Subscribe.decode(utils::receive_message(&mut stream)?)?;
    if !reply.success {
        return Err(SwayError::SubscriptionFailed(events));
    }
    Ok(stream)
}

fn decode((kind, payload): (u32, Vec<u8>)) -> Fallible<Msg> {
    if kind & 0x7fff_ffff == INPUT_EVENT {
        Ok(Msg::Input(serde_json::from_slice(&payload)?))
    } else {
        Ok(Msg::Sway(Event::decode((kind, payload))?))
    }
}

/// Start following GNOME's input-source rules. Must run on the thread that
/// runs the default GLib main loop, after the source list was applied.
pub fn start() {
    let sw = Rc::new(RefCell::new(Switcher::new()));
    let (tx, rx) = glib::MainContext::channel::<Msg>(glib::PRIORITY_DEFAULT);

    // Subscribe before reading the initial state, so no event is missed.
    let first = subscribe();
    let sway_tx = tx.clone();
    thread::spawn(move || {
        let mut stream = first;
        loop {
            match stream.as_mut() {
                Ok(events) => loop {
                    match utils::receive_message(events) {
                        // A message that doesn't decode was still read
                        // whole: skip it, the stream stays in step.
                        Ok(message) => match decode(message) {
                            Ok(msg) => {
                                if sway_tx.send(msg).is_err() {
                                    return;
                                }
                            }
                            Err(e) => debug!("{e}"),
                        },
                        Err(e) => {
                            warn!("{e}");
                            break;
                        }
                    }
                },
                Err(e) => warn!("{e}"),
            }
            thread::sleep(Duration::from_millis(500));
            stream = subscribe();
        }
    });

    // Settings changes join the same queue. The owner's own mru-sources write
    // emits "changed" too; only the keys it follows are queued.
    sw.borrow().settings.connect_changed(None, move |_, key| {
        if key == "sources" || key == "per-window" {
            let _ = tx.send(Msg::Setting(key.to_string()));
        }
    });

    let owner = sw.clone();
    rx.attach(None, move |msg| {
        let mut s = owner.borrow_mut();
        match msg {
            Msg::Sway(ev) => s.on_event(ev),
            Msg::Input(ev) => s.on_input(ev),
            Msg::Setting(key) if key == "sources" => s.sources_changed(),
            Msg::Setting(_) => s.per_window_changed(),
        }
        glib::Continue(true)
    });
    sw.borrow_mut().startup();
}
