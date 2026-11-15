//! The US-position fallback in the Wayland session.
//!
//! sway matches a `bindsym` binding against the keysyms of the layout in
//! use, so Latin layouts need nothing, but a layout without Latin letters
//! leaves letter shortcuts with no key. While such a layout is in use, the
//! daemon adds a `bindcode` binding for every keysym binding of the current
//! binding mode whose keysym the layout cannot produce, on the key that
//! produces it in the plain US layout; when a Latin layout is in use again,
//! it removes them. sway only changes the bindings of the mode it is in, so
//! each mode is brought up to date when it is entered.
//!
//! One owner on the GLib main loop holds the state, as for source switching;
//! sway's events and the input-sources settings changes reach it through one
//! channel.

use super::config::{Binding, Disk, Reader, Wm, MOD_NAMES};
use super::keys::{following_keys, Layouts};
use crate::input_sources::effective_sources;
use crate::utils;
use gio::prelude::*;
use gio::{BusType, Cancellable, DBusCallFlags, Settings};
use log::{debug, info, warn};
use serde::Deserialize;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::thread;
use std::time::Duration;
use swayipc::{CommandType, Connection, Fallible};

/// sway's event types, without the event bit.
const WORKSPACE_EVENT: u32 = 0;
const MODE_EVENT: u32 = 2;
const INPUT_EVENT: u32 = 21;

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
    change: String,
    input: Device,
}

enum Msg {
    /// sway loaded its configuration again.
    Reload,
    Mode(String),
    Input(DeviceEvent),
    /// The input sources or XKB options changed.
    Sources,
}

/// A fallback binding: a key code with the modifiers, flags and command of
/// the binding it stands in for.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Fallback {
    flags: Vec<String>,
    mods: u32,
    code: u32,
    command: String,
}

impl Fallback {
    fn combo(&self) -> String {
        let mut combo: String = (0..8)
            .filter(|i| self.mods & (1 << i) != 0)
            .map(|i| format!("{}+", MOD_NAMES[i]))
            .collect();
        combo.push_str(&self.code.to_string());
        combo
    }

    fn flags(&self) -> String {
        self.flags.iter().map(|f| format!("{f} ")).collect()
    }

    fn bind(&self) -> String {
        let no_warn = if self.flags.iter().any(|f| f == "--no-warn") {
            ""
        } else {
            "--no-warn "
        };
        format!(
            "bindcode {no_warn}{}{} {}",
            self.flags(),
            self.combo(),
            command_argument(&self.command)
        )
    }

    fn unbind(&self) -> String {
        format!("unbindcode {}{}", self.flags(), self.combo())
    }
}

/// A binding's command as one argument of a runtime `bindcode`: quoted so
/// that sway neither splits it at `;` and `,` nor strips its own quotes, with
/// `$` doubled so that variables are not expanded a second time.
fn command_argument(command: &str) -> String {
    format!("'{}'", command.replace('$', "$$").replace('\'', r#"'"'"'"#))
}

struct Keyboard {
    layouts: usize,
    index: usize,
}

struct Owner {
    settings: Settings,
    conn: Option<Connection>,
    layouts: Option<Layouts>,
    us: Option<Layouts>,
    sources: usize,
    keyboards: HashMap<String, Keyboard>,
    mode: String,
    bindings: Vec<Binding>,
    /// The fallback bindings added to each mode.
    added: HashMap<String, Vec<Fallback>>,
    /// Fallback bindings that may have outlived a reload: those added while
    /// sway was already reading its new configuration.
    stale: Vec<Fallback>,
}

impl Owner {
    fn new() -> Owner {
        Owner {
            settings: Settings::new("org.gnome.desktop.input-sources"),
            conn: Connection::new().ok(),
            layouts: None,
            us: Layouts::us(),
            sources: 0,
            keyboards: HashMap::new(),
            mode: String::from("default"),
            bindings: Vec::new(),
            added: HashMap::new(),
            stale: Vec::new(),
        }
    }

    fn startup(&mut self) {
        self.read_sources();
        self.read_keyboards();
        self.read_config();
        self.update();
    }

    /// The layouts the daemon hands to sway, as `apply_input_sources` does.
    fn read_sources(&mut self) {
        let sources = effective_sources(self.settings.get::<Vec<(String, String)>>("sources"));
        let options = self.settings.get::<Vec<String>>("xkb-options").join(",");
        self.sources = sources.len();
        self.layouts = if sources.is_empty() {
            None
        } else {
            let layouts: Vec<&str> = sources.iter().map(|s| s.layout.as_str()).collect();
            let variants: Vec<&str> = sources.iter().map(|s| s.variant.as_str()).collect();
            Layouts::from_names(&layouts.join(","), &variants.join(","), &options)
        };
    }

    fn read_keyboards(&mut self) {
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
        for d in devices.filter(|d| d.input_type == "keyboard") {
            self.keyboards.insert(
                d.identifier,
                Keyboard {
                    layouts: d.xkb_layout_names.len(),
                    index: d.xkb_active_layout_index.unwrap_or(0).max(0) as usize,
                },
            );
        }
    }

    /// sway's running configuration and its binding mode.
    fn read_config(&mut self) {
        let mut conn = match Connection::new() {
            Ok(c) => c,
            Err(e) => {
                warn!("{e}");
                return;
            }
        };
        self.mode = conn
            .get_binding_state()
            .unwrap_or_else(|_| String::from("default"));
        match conn.get_config() {
            Ok(config) => {
                // sway reports the main file only; relative includes are
                // taken from the user's sway configuration directory.
                let dir = env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
                    .unwrap_or_default()
                    .join("sway");
                self.bindings = Reader::new(Wm::Sway, &Disk, true)
                    .resources(resources())
                    .read(&config.config, None, &dir);
            }
            Err(e) => debug!("Cannot read sway's configuration: {e}"),
        }
    }

    /// The group in use: that of the keyboards that have every source.
    fn active_group(&self) -> Option<u32> {
        self.keyboards
            .values()
            .find(|k| k.layouts == self.sources && k.layouts > 0)
            .map(|k| k.index as u32)
    }

    /// The fallback the current mode needs.
    fn wanted(&self) -> Vec<Fallback> {
        let (Some(layouts), Some(us), Some(group)) = (&self.layouts, &self.us, self.active_group())
        else {
            return Vec::new();
        };
        if group >= layouts.groups() || layouts.latin(group) {
            return Vec::new();
        }
        let in_mode: Vec<&Binding> = self
            .bindings
            .iter()
            .filter(|b| b.mode == self.mode)
            .collect();
        // Keys the user bound by code stay theirs.
        let mut taken: HashSet<(u32, u32, bool)> = in_mode
            .iter()
            .filter_map(|b| match b.key {
                super::config::Key::Code(code) if b.group.is_none() => {
                    Some((code, b.mods, b.release()))
                }
                _ => None,
            })
            .collect();
        let mut wanted = Vec::new();
        for b in in_mode {
            let Some(sym) = b.follows_layout() else {
                continue;
            };
            // sway itself handles the keysyms the layout can produce.
            if layouts.produces(group, sym) {
                continue;
            }
            for (code, mods) in following_keys(layouts, us, sym, b.mods, group) {
                if taken.insert((code, mods, b.release())) {
                    wanted.push(Fallback {
                        flags: b.flags.clone(),
                        mods,
                        code,
                        command: b.command.clone(),
                    });
                }
            }
        }
        wanted
    }

    /// Bring the current mode's fallback up to date.
    fn update(&mut self) {
        let wanted = self.wanted();
        let mut added = self.added.remove(&self.mode).unwrap_or_default();
        let mut remove: Vec<Fallback> = Vec::new();
        for f in std::mem::take(&mut self.stale)
            .into_iter()
            .chain(added.iter().cloned())
        {
            if !wanted.contains(&f) && !remove.contains(&f) {
                remove.push(f);
            }
        }
        let add: Vec<Fallback> = wanted
            .iter()
            .filter(|f| !added.contains(f))
            .cloned()
            .collect();
        if !remove.is_empty() || !add.is_empty() {
            info!(
                "Layout fallback in mode {}: {} added, {} removed",
                self.mode,
                add.len(),
                remove.len()
            );
            let cmds: Vec<String> = remove
                .iter()
                .map(Fallback::unbind)
                .chain(add.iter().map(Fallback::bind))
                .collect();
            // One command list, so that nothing runs between its commands.
            match self.run(&cmds.join("; ")) {
                Ok(outcomes) => {
                    added.retain(|f| !remove.contains(f));
                    for (i, f) in add.into_iter().enumerate() {
                        match outcomes.get(remove.len() + i) {
                            Some(Ok(())) => added.push(f),
                            Some(Err(e)) => warn!("{}: {e}", f.bind()),
                            None => warn!("{}: not run", f.bind()),
                        }
                    }
                }
                Err(e) => warn!("{e}"),
            }
        }
        self.added.insert(self.mode.clone(), added);
    }

    fn run(&mut self, cmd: &str) -> Fallible<Vec<Fallible<()>>> {
        if let Some(Ok(outcomes)) = self.conn.as_mut().map(|c| c.run_command(cmd)) {
            return Ok(outcomes);
        }
        let mut conn = Connection::new()?;
        let outcomes = conn.run_command(cmd);
        self.conn = Some(conn);
        outcomes
    }

    fn on_input(&mut self, ev: DeviceEvent) {
        if ev.input.input_type != "keyboard" {
            return;
        }
        if ev.change == "removed" {
            self.keyboards.remove(&ev.input.identifier);
        } else {
            self.keyboards.insert(
                ev.input.identifier,
                Keyboard {
                    layouts: ev.input.xkb_layout_names.len(),
                    index: ev.input.xkb_active_layout_index.unwrap_or(0).max(0) as usize,
                },
            );
        }
        self.update();
    }

    fn on_reload(&mut self) {
        // A reload drops the bindings added at runtime, except those added
        // in reply to the events sway sent while it was reloading; they are
        // in the mode sway is in now.
        self.stale = self.added.drain().flat_map(|(_, f)| f).collect();
        self.read_keyboards();
        self.read_config();
        self.update();
    }
}

/// The resource database of trawld, which sway-regolith's
/// `set_from_resource` reads; empty when trawld does not answer, so that the
/// variables take their defaults, as in sway.
fn resources() -> HashMap<String, String> {
    let reply = gio::bus_get_sync(BusType::Session, Cancellable::NONE).and_then(|bus| {
        bus.call_sync(
            Some("org.regolith.Trawl"),
            "/org/regolith/Trawl",
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&("org.regolith.trawl1", "Resources").to_variant()),
            None,
            DBusCallFlags::NONE,
            2000,
            Cancellable::NONE,
        )
    });
    match reply {
        Ok(reply) => reply
            .try_child_value(0)
            .and_then(|v| v.as_variant())
            .and_then(|v| v.get())
            .unwrap_or_default(),
        Err(e) => {
            debug!("Cannot read the resources from trawld: {e}");
            HashMap::new()
        }
    }
}

/// Subscribe to the events the fallback follows.
fn subscribe() -> Fallible<UnixStream> {
    let mut stream: UnixStream = Connection::new()?.into();
    stream.write_all(&CommandType::Subscribe.encode_with(r#"["workspace","mode","input"]"#))?;
    utils::receive_message(&mut stream)?;
    Ok(stream)
}

fn decode(kind: u32, payload: &[u8]) -> Option<Msg> {
    match kind & 0x7fff_ffff {
        INPUT_EVENT => serde_json::from_slice(payload).ok().map(Msg::Input),
        kind => {
            let v: Value = serde_json::from_slice(payload).ok()?;
            let change = v["change"].as_str()?;
            match kind {
                WORKSPACE_EVENT if change == "reload" => Some(Msg::Reload),
                MODE_EVENT => Some(Msg::Mode(change.to_string())),
                _ => None,
            }
        }
    }
}

/// Start keeping the US-position fallback. Must run on the thread that runs
/// the default GLib main loop, after the source list was applied.
pub fn start() {
    let owner = Rc::new(RefCell::new(Owner::new()));
    let (tx, rx) = glib::MainContext::channel::<Msg>(glib::PRIORITY_DEFAULT);

    let sway_tx = tx.clone();
    let first = subscribe();
    thread::spawn(move || {
        let mut stream = first;
        let mut reconnected = false;
        loop {
            if let Ok(events) = stream.as_mut() {
                if reconnected && sway_tx.send(Msg::Reload).is_err() {
                    return;
                }
                while let Ok((kind, payload)) = utils::receive_message(events) {
                    if let Some(msg) = decode(kind, &payload) {
                        if sway_tx.send(msg).is_err() {
                            return;
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(500));
            stream = subscribe();
            reconnected = true;
        }
    });

    owner
        .borrow()
        .settings
        .connect_changed(None, move |_, key| {
            if key == "sources" || key == "xkb-options" {
                let _ = tx.send(Msg::Sources);
            }
        });

    let o = owner.clone();
    rx.attach(None, move |msg| {
        let mut o = o.borrow_mut();
        match msg {
            Msg::Reload => o.on_reload(),
            Msg::Mode(mode) => {
                o.mode = mode;
                o.update();
            }
            Msg::Input(ev) => o.on_input(ev),
            Msg::Sources => {
                o.read_sources();
                o.update();
            }
        }
        glib::Continue(true)
    });
    owner.borrow_mut().startup();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_survive_sway_parsing_unchanged() {
        assert_eq!(
            command_argument(r#"workspace "layout room""#),
            r#"'workspace "layout room"'"#
        );
        assert_eq!(command_argument("exec echo $HOME"), "'exec echo $$HOME'");
        assert_eq!(
            command_argument("exec notify-send 'a b'; mode default"),
            r#"'exec notify-send '"'"'a b'"'"'; mode default'"#
        );
        let f = Fallback {
            flags: vec!["--release".into()],
            mods: (1 << 6) | 1,
            code: 52,
            command: "nop".into(),
        };
        assert_eq!(f.bind(), "bindcode --no-warn --release Shift+Mod4+52 'nop'");
        assert_eq!(f.unbind(), "unbindcode --release Shift+Mod4+52");
    }
}
