//! Mirrors the keyboard shortcuts configured in GNOME Settings into sway.
//!
//! In the X11 session gnome-settings-daemon's media-keys component grabs these
//! shortcuts and runs their commands. The Wayland session does not start that
//! component, so this handler installs an equivalent `bindsym ... exec` for
//! every custom shortcut and for the built-in launcher and system keys that
//! have a sensible command under sway.

use crate::traits::InputHandler;
use gio::{prelude::*, Settings, SettingsSchemaSource};
use glib::SignalHandlerId;
use log::{debug, info, warn};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};
use swayipc::Connection as SwayConnection;

const MEDIA_KEYS_SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
const CUSTOM_KEYBINDING_SCHEMA: &str =
    "org.gnome.settings-daemon.plugins.media-keys.custom-keybinding";
const CUSTOM_KEYBINDINGS_KEY: &str = "custom-keybindings";

/// Built-in media-keys entries mirrored to sway, with the `exec` argument each
/// runs. These are passed to sway as written, so they are already in sway's
/// syntax.
///
/// - `www` starts the default web browser, as GNOME does; `x-www-browser` does
///   not exist where the browser is a snap.
/// - `logout` asks for confirmation through the session's own logout prompt
///   (`$sway.prompt.logout`, the same as the logout keybinding and launcher
///   entry), which ends gnome-session as well. The `$` is escaped so that
///   sway expands the variable when the key is pressed, not when it is bound.
///
/// The other media-keys entries are not mirrored:
///
/// - Volume, microphone and playback can be rebound in Settings. Their XF86
///   keys keep working through the session's static bindings, but a
///   different key chosen in Settings is not bound.
/// - Stop, eject, the media player, restart, power off, help, email, search
///   and the accessibility keys can be rebound in Settings too, and have no
///   binding here yet.
/// - Brightness, the power keys (handled by logind), the touchpad and
///   rotation keys and the `*-static` variants are not in Settings.
const BUILTIN_SHORTCUTS: [(&str, &str); 7] = [
    ("terminal", "x-terminal-emulator"),
    ("home", "xdg-open $HOME"),
    ("www", "gtk-launch $(xdg-settings get default-web-browser)"),
    ("calculator", "gnome-calculator"),
    ("control-center", "gnome-control-center"),
    ("screensaver", "loginctl lock-session"),
    ("logout", r"swaymsg exec \$sway.prompt.logout"),
];

/// A sway key binding: a `+`-separated combo, optionally on key release.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Binding {
    combo: String,
    release: bool,
}

impl fmt::Display for Binding {
    /// Formats the binding as `bindsym`/`unbindsym` arguments.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.release {
            write!(f, "--release {}", self.combo)
        } else {
            f.write_str(&self.combo)
        }
    }
}

/// Translates a GTK accelerator such as `<Super><Alt>y` into a sway binding.
///
/// Returns `None` for an empty or malformed accelerator: no key, an unknown
/// modifier, or whitespace anywhere.
fn parse_accelerator(accelerator: &str) -> Option<Binding> {
    if accelerator.chars().any(char::is_whitespace) {
        return None;
    }
    let mut release = false;
    let mut combo: Vec<&str> = Vec::new();
    let mut rest = accelerator;
    while let Some(tail) = rest.strip_prefix('<') {
        let (name, tail) = tail.split_once('>')?;
        // GTK matches modifier names case-insensitively.
        let modifier = match name.to_ascii_lowercase().as_str() {
            "release" => {
                release = true;
                None
            }
            "primary" | "control" => Some("Control"),
            "shift" => Some("Shift"),
            "alt" | "meta" => Some("Mod1"),
            "super" => Some("Mod4"),
            "hyper" => Some("Mod3"),
            _ => return None,
        };
        if let Some(modifier) = modifier.filter(|m| !combo.contains(m)) {
            combo.push(modifier);
        }
        rest = tail;
    }
    if rest.is_empty() || rest.contains(['<', '>', '+']) {
        return None;
    }
    combo.push(rest);
    Some(Binding {
        combo: combo.join("+"),
        release,
    })
}

/// Characters that lose their literal meaning once shell quotes are removed.
fn is_special_unquoted(c: char) -> bool {
    c.is_whitespace() || "\\'\"`$,;[]|&<>()*?#~{}".contains(c)
}

/// Rewrites a shell command so that `bindsym ... exec` hands it to the shell
/// with its meaning intact.
///
/// Sway splits a command line on `;` and `,`, and strips unescaped quote
/// characters from every argument, both when the binding is installed and
/// each time it fires; backslash escapes survive both passes. The shell
/// therefore never sees the original quotes, so everything they protected is
/// backslash-escaped instead, and sway's own separators and criteria brackets
/// are escaped everywhere.
///
/// Returns `None` for a command that cannot be expressed: an empty one, one
/// with unbalanced quotes or a trailing backslash, or one that separates
/// commands with a `;` or a newline outside quotes, which sway would split
/// or join.
fn quote_command(command: &str) -> Option<String> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    if command.trim().is_empty() {
        return None;
    }
    let mut quoted = String::with_capacity(command.len());
    let mut quote = Quote::None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Quote::None, '\'') => quote = Quote::Single,
            (Quote::None, '"') => quote = Quote::Double,
            (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
            (Quote::None, ';' | '\n') => return None,
            // An existing escape already means the same thing to sway and to
            // the shell.
            (Quote::None, '\\') => {
                quoted.push(c);
                quoted.push(chars.next()?);
                continue;
            }
            (Quote::Double, '\\')
                if matches!(chars.peek(), Some('$' | '`' | '"' | '\\' | '\n')) =>
            {
                quoted.push(c);
                quoted.push(chars.next()?);
                continue;
            }
            (Quote::None, ',' | '[' | ']') => quoted.push('\\'),
            // Expansions stay active inside double quotes.
            (Quote::Double, '$' | '`') => (),
            (Quote::Single | Quote::Double, _) if is_special_unquoted(c) => quoted.push('\\'),
            _ => (),
        }
        quoted.push(c);
    }
    (quote == Quote::None).then_some(quoted)
}

/// A custom shortcut entry being watched for changes.
struct CustomEntry {
    settings: Settings,
    changed_handler: Option<SignalHandlerId>,
}

impl Drop for CustomEntry {
    fn drop(&mut self) {
        if let Some(handler) = self.changed_handler.take() {
            self.settings.disconnect(handler);
        }
    }
}

struct ShortcutsState {
    sway_connection: SwayConnection,
    /// Watched custom shortcut entries, keyed by settings path.
    custom_entries: BTreeMap<String, CustomEntry>,
    /// Bindings this daemon has installed in sway, with their exec argument.
    installed: BTreeMap<Binding, String>,
}

impl ShortcutsState {
    fn run_command(&mut self, command: String) -> Result<(), Box<dyn Error>> {
        debug!("Executing command: {command}");
        for outcome in self.sway_connection.run_command(&command)? {
            if let Err(e) = outcome {
                warn!("sway rejected `{command}`: {e}");
            }
        }
        Ok(())
    }

    /// Brings sway's bindings in line with `wanted`. With `reinstall`, every
    /// wanted binding is sent again, as after a sway reload, which drops all
    /// bindings added at runtime.
    fn install(
        &mut self,
        wanted: BTreeMap<Binding, String>,
        reinstall: bool,
    ) -> Result<(), Box<dyn Error>> {
        let stale: Vec<Binding> = self
            .installed
            .keys()
            .filter(|binding| !wanted.contains_key(*binding))
            .cloned()
            .collect();
        for binding in stale {
            if !reinstall {
                self.run_command(format!("unbindsym {binding}"))?;
            }
            self.installed.remove(&binding);
        }
        for (binding, command) in wanted {
            if reinstall || self.installed.get(&binding) != Some(&command) {
                self.run_command(format!("bindsym {binding} exec {command}"))?;
                self.installed.insert(binding, command);
            }
        }
        Ok(())
    }
}

pub struct ShortcutsHandler {
    settings: Settings,
    /// The entries of `BUILTIN_SHORTCUTS` that the installed schema has.
    builtin_shortcuts: Vec<(&'static str, &'static str)>,
    // Settings callbacks run on the GLib main loop while a sway reload
    // re-applies everything from the event thread.
    state: Mutex<ShortcutsState>,
}

impl ShortcutsHandler {
    /// Returns `None` when gnome-settings-daemon's media-keys schemas are not
    /// installed, leaving nothing to mirror.
    pub fn new() -> Option<ShortcutsHandler> {
        let source = SettingsSchemaSource::default()?;
        let schema = source.lookup(MEDIA_KEYS_SCHEMA, true)?;
        source.lookup(CUSTOM_KEYBINDING_SCHEMA, true)?;
        // Not every build of the schema has every key (`terminal` is an
        // Ubuntu addition), and GSettings aborts the process when asked for
        // a key its schema lacks.
        let (builtin_shortcuts, missing): (Vec<_>, Vec<_>) = BUILTIN_SHORTCUTS
            .into_iter()
            .partition(|(key, _)| schema.has_key(key));
        for (key, _) in missing {
            info!("{MEDIA_KEYS_SCHEMA} has no key {key}, not mirroring it");
        }
        Some(ShortcutsHandler {
            settings: Settings::new(MEDIA_KEYS_SCHEMA),
            builtin_shortcuts,
            state: Mutex::new(ShortcutsState {
                sway_connection: SwayConnection::new().unwrap(),
                custom_entries: BTreeMap::new(),
                installed: BTreeMap::new(),
            }),
        })
    }

    fn lock_state(&self) -> MutexGuard<'_, ShortcutsState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Re-reads every shortcut and updates sway's bindings to match.
    fn refresh(&self, reinstall: bool) -> Result<(), Box<dyn Error>> {
        let mut state = self.lock_state();
        self.watch_custom_entries(&mut state);
        let wanted = self.wanted_bindings(&state);
        state.install(wanted, reinstall)
    }

    /// Starts watching entries newly added to `custom-keybindings` and stops
    /// watching removed ones.
    fn watch_custom_entries(&self, state: &mut ShortcutsState) {
        let paths: Vec<String> = self.settings.get(CUSTOM_KEYBINDINGS_KEY);
        state.custom_entries.retain(|path, _| paths.contains(path));
        for path in paths {
            if state.custom_entries.contains_key(&path) {
                continue;
            }
            // GSettings aborts the process on a malformed path.
            if !is_valid_settings_path(&path) {
                warn!("Ignoring custom shortcut with invalid path {path:?}");
                continue;
            }
            let settings = Settings::with_path(CUSTOM_KEYBINDING_SCHEMA, &path);
            let handler: *const Self = self;
            let changed_handler = settings.connect_changed(None, move |_, key| {
                if matches!(key, "binding" | "command") {
                    // SAFETY: the handler is boxed for the lifetime of the
                    // daemon, and dropping it drops this entry, which
                    // disconnects the callback.
                    if let Err(e) = unsafe { &*handler }.refresh(false) {
                        warn!("Failed to update custom shortcut: {e}");
                    }
                }
            });
            state.custom_entries.insert(
                path,
                CustomEntry {
                    settings,
                    changed_handler: Some(changed_handler),
                },
            );
        }
    }

    /// Every binding that should currently be installed, with the exec
    /// argument it runs.
    fn wanted_bindings(&self, state: &ShortcutsState) -> BTreeMap<Binding, String> {
        let mut wanted = BTreeMap::new();
        for &(key, command) in &self.builtin_shortcuts {
            let accelerators: Vec<String> = self.settings.get(key);
            for accelerator in accelerators.iter().filter(|a| !a.is_empty()) {
                match parse_accelerator(accelerator) {
                    Some(binding) => {
                        wanted.insert(binding, command.to_string());
                    }
                    None => warn!("Ignoring invalid accelerator {accelerator:?} for {key}"),
                }
            }
        }
        for (path, entry) in &state.custom_entries {
            let accelerator: String = entry.settings.get("binding");
            let command: String = entry.settings.get("command");
            if accelerator.is_empty() || command.is_empty() {
                continue;
            }
            match (parse_accelerator(&accelerator), quote_command(&command)) {
                (Some(binding), Some(command)) => {
                    wanted.insert(binding, command);
                }
                (None, _) => warn!("Ignoring invalid accelerator {accelerator:?} for {path}"),
                (_, None) => warn!("Ignoring command {command:?} for {path}: sway cannot run it"),
            }
        }
        wanted
    }
}

/// Mirrors GSettings' own check for a relocatable schema path.
fn is_valid_settings_path(path: &str) -> bool {
    path.starts_with('/') && path.ends_with('/') && !path.contains("//")
}

impl InputHandler for ShortcutsHandler {
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn sway_connection(&mut self) -> &mut SwayConnection {
        &mut self
            .state
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .sway_connection
    }
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        if key == CUSTOM_KEYBINDINGS_KEY || self.builtin_shortcuts.iter().any(|(k, _)| *k == key) {
            info!("{MEDIA_KEYS_SCHEMA} -> Key: {key} changed");
            self.refresh(false)?;
        }
        Ok(())
    }
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>> {
        self.refresh(true)
    }
    fn sync_gsettings(&mut self, _: &swayipc::Input) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
}

unsafe impl Send for ShortcutsHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(accelerator: &str) -> Option<String> {
        parse_accelerator(accelerator).map(|binding| binding.to_string())
    }

    #[test]
    fn translates_modifiers() {
        assert_eq!(binding("<Super><Alt>y").as_deref(), Some("Mod4+Mod1+y"));
        assert_eq!(
            binding("<Primary><Shift>F9").as_deref(),
            Some("Control+Shift+F9")
        );
        assert_eq!(binding("<Control>t").as_deref(), Some("Control+t"));
        assert_eq!(binding("<Hyper>t").as_deref(), Some("Mod3+t"));
        assert_eq!(binding("<Meta><Alt>t").as_deref(), Some("Mod1+t"));
        assert_eq!(binding("<super>Delete").as_deref(), Some("Mod4+Delete"));
        assert_eq!(binding("F12").as_deref(), Some("F12"));
    }

    #[test]
    fn release_becomes_a_flag() {
        assert_eq!(
            binding("<Primary><Release>t").as_deref(),
            Some("--release Control+t")
        );
        assert_eq!(
            binding("<Release><Super>Delete").as_deref(),
            Some("--release Mod4+Delete")
        );
        assert_eq!(binding("<Release>F12").as_deref(), Some("--release F12"));
    }

    #[test]
    fn rejects_malformed_accelerators() {
        for accelerator in [
            "",
            "<Super>",
            "<Foo>t",
            "<Super> y",
            "<Super>y z",
            "<Super>y\n",
            "<Super",
            "<Super>a+b",
        ] {
            assert_eq!(binding(accelerator), None, "{accelerator:?}");
        }
    }

    #[test]
    fn quotes_commands_for_sway() {
        let quote = |command| quote_command(command);
        assert_eq!(
            quote("touch /tmp/notes.txt").as_deref(),
            Some("touch /tmp/notes.txt")
        );
        assert_eq!(quote(r#"echo "a  b""#).as_deref(), Some(r#"echo "a\ \ b""#));
        assert_eq!(quote("echo a,b").as_deref(), Some(r"echo a\,b"));
        assert_eq!(
            quote("sh -c 'echo hi'").as_deref(),
            Some(r"sh -c 'echo\ hi'")
        );
        assert_eq!(quote(r#"echo "a;b""#).as_deref(), Some(r#"echo "a\;b""#));
        assert_eq!(quote("echo [a,b]").as_deref(), Some(r"echo \[a\,b\]"));
        assert_eq!(quote("xdg-open $HOME").as_deref(), Some("xdg-open $HOME"));
        assert_eq!(
            quote(r#"echo "$HOME" '$HOME'"#).as_deref(),
            Some(r#"echo "$HOME" '\$HOME'"#)
        );
        assert_eq!(quote(r#"echo "it's""#).as_deref(), Some(r#"echo "it\'s""#));
        assert_eq!(quote(r#"echo "a\"b""#).as_deref(), Some(r#"echo "a\"b""#));
        assert_eq!(quote(r"echo 'a\b'").as_deref(), Some(r"echo 'a\\b'"));
        assert_eq!(quote(r"echo a\;b").as_deref(), Some(r"echo a\;b"));
    }

    #[test]
    fn rejects_unrepresentable_commands() {
        for command in [
            "",
            "  ",
            "echo a; echo b",
            "echo a\necho b",
            "echo 'a",
            "echo \"a",
            "echo a\\",
        ] {
            assert_eq!(quote_command(command), None, "{command:?}");
        }
    }
}
