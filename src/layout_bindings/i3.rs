//! Layout-following shortcuts in the X11 session.
//!
//! i3 resolves a `bindsym` binding to keys once, in the first layout group,
//! and runs the binding from those keys whatever the group in use. Regolith's
//! i3 configuration includes `$XDG_RUNTIME_DIR/regolith/i3/layout-bindings`
//! ahead of every other binding, and i3 runs the first binding that matches;
//! the daemon writes there, for each group of the X server's keymap, a
//! `GroupN` `bindcode` for every key whose binding differs from what i3 does
//! on its own: the binding that key should run in that group, or `nop` when
//! it should run none. When the file's contents change, i3 reloads it.

use super::config::{Binding, Key, Reader, Wm, MOD_NAMES};
use super::keys::{following_keys, KeyPress, Layouts};
use crate::utils;
use log::{debug, info, warn};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};
use swayipc::{CommandType, Connection};
use x11rb::connection::Connection as _;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::Event as XEvent;
use x11rb::xcb_ffi::XCBConnection;
use xkbcommon::xkb as xkbc;

/// i3's workspace and shutdown events, without the event bit.
const WORKSPACE_EVENT: u32 = 0;
const SHUTDOWN_EVENT: u32 = 6;

/// How long i3 may take to come back after a restart, or to appear at all.
const RECONNECT: Duration = Duration::from_secs(10);

/// Changes that arrive together (a new keymap comes as several X events)
/// are handled once.
const SETTLE: Duration = Duration::from_millis(80);

enum Msg {
    /// The X server's keymap may have changed.
    Keymap,
    /// i3 (re)loaded its configuration.
    Config,
    /// i3 has gone.
    Exit,
}

/// The file Regolith's i3 configuration includes.
fn include_path() -> Option<PathBuf> {
    let dir = env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty())?;
    Some(PathBuf::from(dir).join("regolith/i3/layout-bindings"))
}

/// Keep i3's shortcuts on the layout in use, until i3 exits; then call
/// `exited`.
pub fn start(exited: impl FnOnce() + Send + 'static) {
    let (tx, rx) = channel();
    let path = include_path();
    if path.is_none() {
        warn!("XDG_RUNTIME_DIR is not set: i3's shortcuts are not kept on the layout in use");
    }
    let events_tx = tx.clone();
    thread::spawn(move || follow_i3(events_tx));
    thread::spawn(move || follow_keymap(tx));
    thread::spawn(move || {
        Worker::new(path).run(rx);
        exited();
    });
}

/// A connection to i3's IPC socket subscribed to its workspace and shutdown
/// events.
fn subscribe() -> swayipc::Fallible<UnixStream> {
    let mut stream: UnixStream = Connection::new()?.into();
    stream.write_all(&CommandType::Subscribe.encode_with(r#"["workspace","shutdown"]"#))?;
    utils::receive_message(&mut stream)?;
    Ok(stream)
}

fn reconnect() -> Option<UnixStream> {
    let start = Instant::now();
    while start.elapsed() < RECONNECT {
        if let Ok(stream) = subscribe() {
            return Some(stream);
        }
        thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Follow i3: its reloads and restarts, and its exit.
fn follow_i3(tx: Sender<Msg>) {
    let mut stream = reconnect();
    while let Some(mut events) = stream.take() {
        // On each connection (startup, or i3 back from a restart) the
        // configuration is read afresh.
        let _ = tx.send(Msg::Config);
        loop {
            let Ok((kind, payload)) = utils::receive_message(&mut events) else {
                break;
            };
            let change = serde_json::from_slice::<Value>(&payload)
                .ok()
                .and_then(|v| v["change"].as_str().map(str::to_string))
                .unwrap_or_default();
            match (kind & 0x7fff_ffff, change.as_str()) {
                (WORKSPACE_EVENT, "reload") => {
                    let _ = tx.send(Msg::Config);
                }
                (SHUTDOWN_EVENT, "exit") => {
                    let _ = tx.send(Msg::Exit);
                    return;
                }
                _ => {}
            }
        }
        // A restart, or a crash.
        stream = reconnect();
        if stream.is_some() {
            info!("Reconnected to i3");
        }
    }
    let _ = tx.send(Msg::Exit);
}

fn connect_x() -> Option<(XCBConnection, usize)> {
    let start = Instant::now();
    loop {
        match XCBConnection::connect(None) {
            Ok(c) => return Some(c),
            Err(e) if start.elapsed() > RECONNECT => {
                warn!("Cannot connect to the X server: {e}");
                return None;
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Follow the X server's keymap: new keymaps and the rules names that come
/// with them.
fn follow_keymap(tx: Sender<Msg>) {
    let Some((conn, screen)) = connect_x() else {
        let _ = tx.send(Msg::Exit);
        return;
    };
    let watched = || -> Result<u32, Box<dyn std::error::Error>> {
        conn.xkb_use_extension(1, 0)?.reply()?;
        let events = xkb::EventType::NEW_KEYBOARD_NOTIFY | xkb::EventType::MAP_NOTIFY;
        let parts = xkb::MapPart::KEY_TYPES
            | xkb::MapPart::KEY_SYMS
            | xkb::MapPart::MODIFIER_MAP
            | xkb::MapPart::EXPLICIT_COMPONENTS
            | xkb::MapPart::KEY_ACTIONS
            | xkb::MapPart::VIRTUAL_MODS
            | xkb::MapPart::VIRTUAL_MOD_MAP;
        conn.xkb_select_events(
            xkb::ID::USE_CORE_KBD.into(),
            0u16.into(),
            events,
            parts,
            parts,
            &xkb::SelectEventsAux::new(),
        )?
        .check()?;
        let root = conn.setup().roots[screen].root;
        let attributes =
            xproto::ChangeWindowAttributesAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE);
        conn.change_window_attributes(root, &attributes)?.check()?;
        Ok(conn.intern_atom(false, b"_XKB_RULES_NAMES")?.reply()?.atom)
    };
    let rules_names = match watched() {
        Ok(atom) => atom,
        Err(e) => {
            warn!("Cannot follow the X server's keymap: {e}");
            return;
        }
    };
    loop {
        match conn.wait_for_event() {
            Ok(XEvent::XkbNewKeyboardNotify(_) | XEvent::XkbMapNotify(_)) => {
                let _ = tx.send(Msg::Keymap);
            }
            Ok(XEvent::PropertyNotify(e)) if e.atom == rules_names => {
                let _ = tx.send(Msg::Keymap);
            }
            Ok(_) => {}
            Err(e) => {
                warn!("Lost the X server: {e}");
                let _ = tx.send(Msg::Exit);
                return;
            }
        }
    }
}

struct Worker {
    path: Option<PathBuf>,
    x: Option<(XCBConnection, i32)>,
    layouts: Option<Layouts>,
    us: Option<Layouts>,
    bindings: Option<Vec<Binding>>,
}

impl Worker {
    fn new(path: Option<PathBuf>) -> Worker {
        let x = connect_x().and_then(|(conn, _)| {
            let mut major = 0;
            let mut minor = 0;
            let mut event = 0;
            let mut error = 0;
            let ok = xkbc::x11::setup_xkb_extension(
                &conn,
                xkbc::x11::MIN_MAJOR_XKB_VERSION,
                xkbc::x11::MIN_MINOR_XKB_VERSION,
                xkbc::x11::SetupXkbExtensionFlags::NoFlags,
                &mut major,
                &mut minor,
                &mut event,
                &mut error,
            );
            let device = xkbc::x11::get_core_keyboard_device_id(&conn);
            (ok && device >= 0).then_some((conn, device))
        });
        if x.is_none() {
            warn!("The X server has no usable keyboard extension");
        }
        Worker {
            path,
            x,
            layouts: None,
            us: Layouts::us(),
            bindings: None,
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        self.read_keymap();
        self.read_config();
        self.update();
        while let Ok(first) = rx.recv() {
            let mut msgs = vec![first];
            while let Ok(msg) = rx.recv_timeout(SETTLE) {
                msgs.push(msg);
            }
            if msgs.iter().any(|m| matches!(m, Msg::Exit)) {
                info!("i3 has exited");
                return;
            }
            if msgs.iter().any(|m| matches!(m, Msg::Keymap)) {
                self.read_keymap();
            }
            if msgs.iter().any(|m| matches!(m, Msg::Config)) {
                self.read_config();
            }
            self.update();
        }
    }

    fn read_keymap(&mut self) {
        let Some((conn, device)) = &self.x else {
            return;
        };
        let context = xkbc::Context::new(xkbc::CONTEXT_NO_FLAGS);
        let keymap = xkbc::x11::keymap_new_from_device(
            &context,
            conn,
            *device,
            xkbc::KEYMAP_COMPILE_NO_FLAGS,
        );
        if keymap.get_raw_ptr().is_null() {
            warn!("Cannot read the X server's keymap");
            return;
        }
        self.layouts = Some(Layouts::new(keymap));
    }

    fn read_config(&mut self) {
        match running_config(self.path.clone()) {
            Ok(bindings) => self.bindings = Some(bindings),
            Err(e) => debug!("Cannot read i3's configuration: {e}"),
        }
    }

    /// Bring the include up to date, and have i3 reload it if it changed.
    fn update(&mut self) {
        let (Some(path), Some(layouts), Some(us), Some(bindings)) =
            (&self.path, &self.layouts, &self.us, &self.bindings)
        else {
            return;
        };
        let text = include(bindings, layouts, us);
        let current = fs::read_to_string(path).unwrap_or_default();
        if text == current {
            return;
        }
        if let Err(e) = replace(path, &text) {
            warn!(
                "Cannot write {}: {e}; i3's shortcuts are not kept on the layout in use",
                path.display()
            );
            return;
        }
        info!("Updated {}, reloading i3", path.display());
        if let Err(e) = reload() {
            warn!("Cannot reload i3: {e}");
        }
    }
}

/// i3's running configuration: the main file with every included file in
/// place, as i3 reports them with variables expanded. The daemon's own
/// include is left out.
fn running_config(own: Option<PathBuf>) -> swayipc::Fallible<Vec<Binding>> {
    let mut stream: UnixStream = Connection::new()?.into();
    stream.write_all(&CommandType::GetConfig.encode())?;
    let reply: Value = serde_json::from_slice(&utils::receive_message(&mut stream)?.1)?;
    let mut files = HashMap::new();
    let mut main = None;
    for file in reply["included_configs"].as_array().into_iter().flatten() {
        if let (Some(path), Some(text)) = (
            file["path"].as_str(),
            file["variable_replaced_contents"].as_str(),
        ) {
            main.get_or_insert_with(|| (PathBuf::from(path), text.to_string()));
            files.insert(PathBuf::from(path), text.to_string());
        }
    }
    Ok(match main {
        Some((path, text)) => {
            let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
            Reader::new(Wm::I3, &files, false)
                .skip(own)
                .read(&text, Some(&path), &dir)
        }
        // An i3 that reports the main file only.
        None => {
            let text = reply["config"].as_str().unwrap_or_default();
            Reader::new(Wm::I3, &super::config::Disk, true)
                .skip(own)
                .read(text, None, Path::new("/"))
        }
    })
}

/// Reload i3, keeping the binding mode the user is in.
fn reload() -> swayipc::Fallible<()> {
    let mut conn = Connection::new()?;
    let mode = conn.get_binding_state().unwrap_or_default();
    let cmd = if mode.is_empty() || mode == "default" {
        String::from("reload")
    } else {
        format!("reload; mode {}", quote(&mode))
    };
    for outcome in conn.run_command(&cmd)? {
        if let Err(e) = outcome {
            warn!("{cmd}: {e}");
        }
    }
    Ok(())
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Replace `path` with `text` in one step, so that i3 never reads a partly
/// written file.
fn replace(path: &Path, text: &str) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temporary = dir.join(format!(".{name}.{}", std::process::id()));
    // A leftover of an earlier daemon with the same process id.
    let _ = fs::remove_file(&temporary);
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// The text of the include: for every mode and group, a `GroupN` binding for
/// each key whose binding differs from i3's own resolution. Empty when no
/// key differs.
pub fn include(bindings: &[Binding], layouts: &Layouts, us: &Layouts) -> String {
    let groups = layouts.groups();
    if groups == 0 || (groups == 1 && layouts.latin(0)) {
        return String::new();
    }
    let mut modes: Vec<(&str, bool)> = Vec::new();
    for b in bindings {
        if !modes.iter().any(|(m, _)| *m == b.mode) {
            modes.push((&b.mode, b.pango));
        }
    }
    let mut text = String::new();
    for (mode, pango) in modes {
        let in_mode: Vec<&Binding> = bindings.iter().filter(|b| b.mode == mode).collect();
        let lines: Vec<String> = (0..groups)
            .flat_map(|group| mode_lines(&in_mode, layouts, us, group))
            .collect();
        if lines.is_empty() {
            continue;
        }
        if mode == "default" {
            text.extend(lines.iter().map(|l| format!("{l}\n")));
        } else {
            let pango = if pango { "--pango_markup " } else { "" };
            text.push_str(&format!("mode {pango}{} {{\n", quote(mode)));
            text.extend(lines.iter().map(|l| format!("    {l}\n")));
            text.push_str("}\n");
        }
    }
    if text.is_empty() {
        return text;
    }
    format!(
        "# Generated by regolith-inputd: shortcuts that follow the keyboard layout\n\
         # in use. Changes are overwritten.\n{text}"
    )
}

/// The bindings of one mode for one group.
fn mode_lines(bindings: &[&Binding], layouts: &Layouts, us: &Layouts, group: u32) -> Vec<String> {
    // Keys the user bound explicitly (by code, or for this group) stay theirs.
    let mut fixed = BTreeSet::new();
    for b in bindings.iter().filter(|b| b.follows_layout().is_none()) {
        let keys = match (b.key, b.group) {
            (_, Some(g)) if g != group => continue,
            (Key::Code(code), _) => vec![(code, b.mods)],
            (Key::Sym(sym), _) => layouts.i3_keys(sym, b.mods, group),
        };
        fixed.extend(keys.into_iter().map(|(c, m)| (c, m, b.release())));
    }
    // What i3 runs on its own, from the first group, and what should run.
    let mut native: BTreeMap<KeyPress, usize> = BTreeMap::new();
    let mut wanted: BTreeMap<KeyPress, usize> = BTreeMap::new();
    for (i, b) in bindings.iter().enumerate() {
        let Some(sym) = b.follows_layout() else {
            continue;
        };
        for (code, mods) in layouts.i3_keys(sym, b.mods, 0) {
            native.entry((code, mods, b.release())).or_insert(i);
        }
        for (code, mods) in following_keys(layouts, us, sym, b.mods, group) {
            wanted.entry((code, mods, b.release())).or_insert(i);
        }
    }
    let keys: BTreeSet<KeyPress> = native.keys().chain(wanted.keys()).copied().collect();
    keys.into_iter()
        .filter(|k| !fixed.contains(k) && native.get(k) != wanted.get(k))
        .map(|(code, mods, release)| {
            let modifiers: String = (0..8)
                .filter(|i| mods & (1 << i) != 0)
                .map(|i| format!("{}+", MOD_NAMES[i]))
                .collect();
            let combo = format!("{modifiers}Group{}+{code}", group + 1);
            match wanted.get(&(code, mods, release)) {
                Some(&i) => {
                    let b = bindings[i];
                    let flags: String = b.flags.iter().map(|f| format!("{f} ")).collect();
                    format!("bindcode {flags}{combo} {}", b.command)
                }
                None if release => format!("bindcode --release {combo} nop"),
                None => format!("bindcode {combo} nop"),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout_bindings::config::Disk;

    fn bindings(text: &str) -> Vec<Binding> {
        Reader::new(Wm::I3, &Disk, true).read(text, None, Path::new("/"))
    }

    fn letters() -> String {
        ('a'..='z')
            .map(|c| format!("bindsym $mod+{c} workspace {c}\n"))
            .collect::<String>()
    }

    #[test]
    fn single_latin_layouts_and_shared_positions_need_no_bindings() {
        let us = Layouts::us().unwrap();
        let config = format!("set $mod Mod4\n{}", letters());
        for layouts in ["us", "de", "epo"] {
            let l = Layouts::from_names(layouts, "", "").unwrap();
            assert_eq!(include(&bindings(&config), &l, &us), "", "{layouts}");
        }
        let l = Layouts::from_names("us,de", "", "").unwrap();
        let shared = bindings("bindsym Mod4+a workspace a\n");
        assert_eq!(include(&shared, &l, &us), "");
    }

    #[test]
    fn keys_that_moved_get_the_binding_of_their_letter() {
        let us = Layouts::us().unwrap();
        let l = Layouts::from_names("us,de", "", "").unwrap();
        let text = include(&bindings("bindsym Mod4+z workspace z\n"), &l, &us);
        let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        // German z is on AD06 (code 29); AB01 (52) types "y" there.
        assert_eq!(
            lines,
            [
                "bindcode Mod4+Group2+29 workspace z",
                "bindcode Mod4+Group2+52 nop"
            ]
        );
    }

    #[test]
    fn non_latin_groups_get_us_positions_inside_their_modes() {
        let us = Layouts::us().unwrap();
        let l = Layouts::from_names("ru", "", "").unwrap();
        let config = "mode --pango_markup \"edit mode\" {\n\
            bindsym --release Mod4+Shift+z exec \"echo z\"\n}\n\
            bindsym Mod4+Group1+x nop\nbindcode Mod4+53 nop\nbindsym Mod4+x workspace x\n";
        let text = include(&bindings(config), &l, &us);
        let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            lines,
            [
                "mode --pango_markup \"edit mode\" {",
                "    bindcode --release Shift+Mod4+Group1+52 exec \"echo z\"",
                "}",
            ]
        );
    }
}
