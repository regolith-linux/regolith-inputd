//! The keyboard bindings of a window manager's running configuration.
//!
//! The configuration is read the way i3 and sway read it: line
//! continuations are joined, blocks (`mode "name" { ... }`, `bar { ... }`)
//! prefix the lines inside them, `include` pulls in other files at the place
//! it appears, and `$variables` are expanded, including those that
//! sway-regolith's `set_from_resource` takes from the resource database. Only
//! keyboard bindings of the top level and of `mode` blocks are kept.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use xkbcommon::xkb;

/// The core modifier masks, in the order of the X11 core protocol (and of
/// the real modifiers of every XKB keymap).
pub const SHIFT: u32 = 1 << 0;
pub const MOD_NAMES: [&str; 8] = [
    "Shift", "Lock", "Control", "Mod1", "Mod2", "Mod3", "Mod4", "Mod5",
];

/// The window manager whose configuration syntax is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wm {
    I3,
    Sway,
}

/// What a binding is bound to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// `bindsym`: a keysym.
    Sym(u32),
    /// `bindcode`: a key code.
    Code(u32),
}

/// A keyboard binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// The binding mode it belongs to; top-level bindings are in "default".
    pub mode: String,
    /// Whether the mode is declared with `--pango_markup`.
    pub pango: bool,
    /// The options before the key combination (`--release` and the like).
    pub flags: Vec<String>,
    /// The modifiers, as a core modifier mask.
    pub mods: u32,
    /// The layout group named with a `GroupN` (or `Mode_switch`) modifier.
    pub group: Option<u32>,
    pub key: Key,
    /// The command, as written (variables expanded).
    pub command: String,
}

impl Binding {
    /// A `bindsym` binding the daemon keeps on the layout in use: one that
    /// names no layout group. `bindcode` bindings and group-specific ones are
    /// left as the user wrote them, and so are sway's `bindsym --to-code`
    /// ones, which sway itself binds to key codes.
    pub fn follows_layout(&self) -> Option<u32> {
        match self.key {
            Key::Sym(sym) if self.group.is_none() && !self.has_flag("--to-code") => Some(sym),
            _ => None,
        }
    }

    fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }

    pub fn release(&self) -> bool {
        self.has_flag("--release")
    }
}

/// The text of the configuration files, by path.
pub trait Files {
    fn read(&self, path: &Path) -> Option<String>;
}

/// Files read from disk.
pub struct Disk;

impl Files for Disk {
    fn read(&self, path: &Path) -> Option<String> {
        fs::read_to_string(path).ok()
    }
}

/// Files whose (variable-expanded) text the window manager reported.
impl Files for HashMap<PathBuf, String> {
    fn read(&self, path: &Path) -> Option<String> {
        self.get(path).cloned()
    }
}

/// Reads the bindings of a configuration.
pub struct Reader<'a> {
    wm: Wm,
    files: &'a dyn Files,
    /// Whether `$variables` are expanded here (they are not in i3's reported
    /// text, where i3 did it already).
    expand: bool,
    /// Files whose bindings are not read: the daemon's own include.
    skip: Vec<PathBuf>,
    /// The resources `set_from_resource` reads.
    resources: HashMap<String, String>,
    variables: Vec<(String, String)>,
    seen: Vec<PathBuf>,
    bindings: Vec<Binding>,
}

impl<'a> Reader<'a> {
    pub fn new(wm: Wm, files: &'a dyn Files, expand: bool) -> Reader<'a> {
        Reader {
            wm,
            files,
            expand,
            skip: Vec::new(),
            resources: HashMap::new(),
            variables: Vec::new(),
            seen: Vec::new(),
            bindings: Vec::new(),
        }
    }

    pub fn skip(mut self, path: Option<PathBuf>) -> Self {
        self.skip
            .extend(path.map(|p| fs::canonicalize(&p).unwrap_or(p)));
        self
    }

    pub fn resources(mut self, resources: HashMap<String, String>) -> Self {
        self.resources = resources;
        self
    }

    /// Read the main configuration, whose text is `text`; relative includes
    /// are resolved against `dir`.
    pub fn read(mut self, text: &str, path: Option<&Path>, dir: &Path) -> Vec<Binding> {
        if let Some(path) = path {
            self.seen.push(path.to_path_buf());
        }
        self.read_text(text, dir);
        self.bindings
    }

    fn read_file(&mut self, path: PathBuf) {
        if self.seen.contains(&path) || self.skip.contains(&path) {
            return;
        }
        self.seen.push(path.clone());
        if let Some(text) = self.files.read(&path) {
            let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
            self.read_text(&text, &dir);
        }
    }

    fn read_text(&mut self, text: &str, dir: &Path) {
        let mut blocks: Vec<String> = Vec::new();
        for line in logical_lines(text) {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if self.expand && blocks.is_empty() && self.set_variable(line) {
                continue;
            }
            let line = if self.expand {
                self.expand_variables(line)
            } else {
                line.to_string()
            };
            if line == "}" {
                blocks.pop();
                continue;
            }
            if let Some(header) = line.strip_suffix('{') {
                blocks.push(header.trim().to_string());
                continue;
            }
            if blocks.is_empty() {
                let words = words(&line);
                if words.first().map(|&(s, e)| &line[s..e]) == Some("include") {
                    if let Some(&(start, _)) = words.get(1) {
                        for path in include_paths(&line[start..], dir) {
                            self.read_file(path);
                        }
                    }
                    continue;
                }
                self.statement(&line);
            } else {
                self.statement(&format!("{} {line}", blocks.join(" ")));
            }
        }
    }

    /// `set $name value`, sway's variables, and sway-regolith's
    /// `set_from_resource $name resource [default]`: the resource's value, or
    /// the default when the resource is empty or not set.
    fn set_variable(&mut self, line: &str) -> bool {
        let w = words(line);
        let command = w.first().map(|&(s, e)| &line[s..e]);
        let from_resource = match command {
            Some("set") => false,
            Some("set_from_resource") => true,
            _ => return false,
        };
        if w.len() < 3 || !line[w[1].0..].starts_with('$') {
            return false;
        }
        let name = line[w[1].0..w[1].1].to_string();
        let value = if from_resource {
            let resource = self.expand_variables(&line[w[2].0..w[2].1]);
            match self.resources.get(&resource).filter(|v| !v.is_empty()) {
                Some(value) => value.clone(),
                None if w.len() > 3 => {
                    let default: Vec<&str> = w[3..].iter().map(|&(s, e)| &line[s..e]).collect();
                    self.expand_variables(&default.join(" "))
                }
                // sway leaves the variable unset.
                None => return true,
            }
        } else {
            self.expand_variables(line[w[2].0..].trim_end())
        };
        self.variables.retain(|(n, _)| *n != name);
        self.variables.push((name, value));
        // The longest name that matches wins, as in sway.
        self.variables.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        true
    }

    fn expand_variables(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(i) = rest.find('$') {
            out.push_str(&rest[..i]);
            let tail = &rest[i..];
            let escaped = out.ends_with('\\') && !out.ends_with("\\\\");
            if escaped {
                out.push('$');
                rest = &tail[1..];
            } else if let Some(after) = tail.strip_prefix("$$") {
                out.push('$');
                rest = after;
            } else if let Some((name, value)) = self
                .variables
                .iter()
                .find(|(n, _)| tail.starts_with(n.as_str()))
            {
                out.push_str(value);
                rest = &tail[name.len()..];
            } else {
                out.push('$');
                rest = &tail[1..];
            }
        }
        out.push_str(rest);
        out
    }

    /// A statement with its block headers in front: `bindsym ...`,
    /// `mode "name" bindsym ...`; anything else is not a key binding.
    fn statement(&mut self, line: &str) {
        let w = words(line);
        let word = |i: usize| w.get(i).map(|&(s, e)| &line[s..e]);
        let (mode, pango, at) = match word(0) {
            Some("bindsym" | "bindcode") => ("default".to_string(), false, 0),
            Some("mode") => {
                let pango = word(1) == Some("--pango_markup");
                let at = if pango { 2 } else { 1 };
                match word(at) {
                    Some(name) => (unquote(name), pango, at + 1),
                    None => return,
                }
            }
            _ => return,
        };
        let code = match word(at) {
            Some("bindsym") => false,
            Some("bindcode") => true,
            _ => return,
        };
        let mut i = at + 1;
        let mut flags = Vec::new();
        while let Some(flag) = word(i).filter(|f| f.starts_with("--")) {
            flags.push(flag.to_string());
            i += 1;
        }
        let (Some(combo), Some(&(command, _))) = (word(i), w.get(i + 1)) else {
            return;
        };
        if let Some((mods, group, key)) = self.combo(combo, code) {
            self.bindings.push(Binding {
                mode,
                pango,
                flags,
                mods,
                group,
                key,
                command: line[command..].trim().to_string(),
            });
        }
    }

    /// The modifiers, group and key of a key combination such as
    /// `Mod4+Shift+z`. Pointer buttons and combinations of several keys are
    /// not keyboard bindings the daemon handles.
    fn combo(&self, combo: &str, code: bool) -> Option<(u32, Option<u32>, Key)> {
        let parts: Vec<&str> = combo.split('+').collect();
        let (key, modifiers) = parts.split_last()?;
        let mut mods = 0;
        let mut group = None;
        for m in modifiers {
            match modifier(m, self.wm)? {
                Modifier::Mask(mask) => mods |= mask,
                Modifier::Group(g) => group = Some(g),
            }
        }
        let lower = key.to_ascii_lowercase();
        if let Some(n) = lower.strip_prefix("button") {
            if n.parse::<u32>().is_ok() {
                return None;
            }
        }
        if code {
            return key.parse().ok().map(|c| (mods, group, Key::Code(c)));
        }
        let flags = match self.wm {
            Wm::I3 => xkb::KEYSYM_NO_FLAGS,
            Wm::Sway => xkb::KEYSYM_CASE_INSENSITIVE,
        };
        match xkb::keysym_from_name(key, flags).raw() {
            0 => None,
            sym => Some((mods, group, Key::Sym(sym))),
        }
    }
}

enum Modifier {
    Mask(u32),
    Group(u32),
}

fn modifier(name: &str, wm: Wm) -> Option<Modifier> {
    let lower = name.to_ascii_lowercase();
    let index = match lower.as_str() {
        "shift" => 0,
        "lock" | "caps" => 1,
        "control" | "ctrl" => 2,
        "mod1" | "alt" => 3,
        "mod2" => 4,
        "mod3" => 5,
        "mod4" | "super" | "logo" => 6,
        "mod5" => 7,
        "mode_switch" if wm == Wm::I3 => return Some(Modifier::Group(1)),
        _ => {
            let n = lower.strip_prefix("group")?.parse::<u32>().ok()?;
            return (1..=4).contains(&n).then(|| Modifier::Group(n - 1));
        }
    };
    Some(Modifier::Mask(1 << index))
}

/// The lines of a configuration, with `\` continuations joined.
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        match line.strip_suffix('\\') {
            Some(start) => current.push_str(start),
            None => {
                current.push_str(line);
                lines.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// The words of a line, as byte ranges: separated by white space, with
/// quoted strings and escaped characters kept in their word.
pub fn words(line: &str) -> Vec<(usize, usize)> {
    let mut words = Vec::new();
    let mut start = None;
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if start.is_none() {
            if c.is_whitespace() {
                continue;
            }
            start = Some(i);
        }
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' | '\'' if quote == Some(c) => quote = None,
            '"' | '\'' if quote.is_none() => quote = Some(c),
            c if c.is_whitespace() && quote.is_none() => {
                words.push((start.take().unwrap(), i));
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        words.push((s, line.len()));
    }
    words
}

/// A word without its surrounding quotes.
fn unquote(word: &str) -> String {
    for q in ['"', '\''] {
        if word.len() >= 2 && word.starts_with(q) && word.ends_with(q) {
            return word[1..word.len() - 1].replace(&format!("\\{q}"), &q.to_string());
        }
    }
    word.to_string()
}

/// The files an `include` names, in order: words are expanded as by the
/// shell (`~`, environment variables, wildcards), relative to `dir`.
fn include_paths(arguments: &str, dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for (s, e) in words(arguments) {
        let word = expand_environment(&unquote(&arguments[s..e]));
        let path = match word.strip_prefix("~/") {
            Some(rest) => PathBuf::from(env::var("HOME").unwrap_or_default()).join(rest),
            None => dir.join(&word),
        };
        for path in glob(&path) {
            if let Ok(real) = fs::canonicalize(&path) {
                if real.is_file() && !paths.contains(&real) {
                    paths.push(real);
                }
            }
        }
    }
    paths
}

fn expand_environment(word: &str) -> String {
    let mut out = String::new();
    let mut rest = word;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        let (name, after) = match tail.strip_prefix('{') {
            Some(braced) => match braced.find('}') {
                Some(end) => (&braced[..end], &braced[end + 1..]),
                None => ("", tail),
            },
            None => {
                let end = tail
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(tail.len());
                (&tail[..end], &tail[end..])
            }
        };
        if name.is_empty() {
            out.push('$');
        } else {
            out.push_str(&env::var(name).unwrap_or_default());
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// The existing paths that match `pattern`, whose components may hold the
/// wildcards `*`, `?` and `[...]`; sorted, as the shell sorts them.
fn glob(pattern: &Path) -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::new()];
    for component in pattern.components() {
        let part = component.as_os_str().to_string_lossy();
        let wild = matches!(component, Component::Normal(_)) && part.contains(['*', '?', '[']);
        if !wild {
            paths.iter_mut().for_each(|p| p.push(component));
            continue;
        }
        let mut matched = Vec::new();
        for dir in &paths {
            let Ok(entries) = fs::read_dir(if dir.as_os_str().is_empty() {
                Path::new(".")
            } else {
                dir
            }) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| (!n.starts_with('.') || part.starts_with('.')) && wildcard(&part, n))
                .collect();
            names.sort();
            matched.extend(names.into_iter().map(|n| dir.join(n)));
        }
        paths = matched;
    }
    paths.into_iter().filter(|p| p.exists()).collect()
}

/// Shell wildcard matching of one path component.
fn wildcard(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    fn class(p: &[char], c: char) -> Option<(bool, usize)> {
        // p starts after '['; returns whether c matches and the length used.
        let mut i = 0;
        let negate = matches!(p.first(), Some('!' | '^'));
        if negate {
            i += 1;
        }
        let mut found = false;
        let mut first = true;
        while i < p.len() && (first || p[i] != ']') {
            first = false;
            if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
                found |= p[i] <= c && c <= p[i + 2];
                i += 3;
            } else {
                found |= p[i] == c;
                i += 1;
            }
        }
        (i < p.len()).then(|| (found != negate, i + 1))
    }
    fn matches(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).any(|k| matches(&p[1..], &n[k..])),
            Some('?') => !n.is_empty() && matches(&p[1..], &n[1..]),
            Some('[') if !n.is_empty() => match class(&p[1..], n[0]) {
                Some((true, used)) => matches(&p[1 + used..], &n[1..]),
                Some((false, _)) => false,
                None => n[0] == '[' && matches(&p[1..], &n[1..]),
            },
            Some(c) => n.first() == Some(c) && matches(&p[1..], &n[1..]),
        }
    }
    matches(&p, &n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(wm: Wm, text: &str) -> Vec<Binding> {
        let expand = wm == Wm::Sway;
        Reader::new(wm, &Disk, expand).read(text, None, Path::new("/nonexistent"))
    }

    fn sym(name: &str) -> Key {
        Key::Sym(xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS).raw())
    }

    #[test]
    fn keysym_bindings_are_read_with_their_mode_flags_and_command() {
        let config = "set $mod Mod4\n\
            # bindsym $mod+q kill\n\
            bindsym $mod+Shift+z workspace \"layout room\"\n\
            bindsym --release $mod+y exec \\\n  notify-send y\n\
            mode --pango_markup \"edit mode\" {\n  bindsym $mod+Control+a nop; mode default\n}\n\
            bar {\n  bindsym button1 nop\n}\n";
        let bindings = read(Wm::Sway, config);
        assert_eq!(bindings.len(), 3);
        assert_eq!(bindings[0].mode, "default");
        assert_eq!(bindings[0].mods, (1 << 6) | SHIFT);
        assert_eq!(bindings[0].key, sym("z"));
        assert_eq!(bindings[0].command, "workspace \"layout room\"");
        assert_eq!(bindings[1].flags, vec!["--release"]);
        assert!(bindings[1].release());
        assert_eq!(bindings[1].command, "exec   notify-send y");
        assert_eq!(bindings[2].mode, "edit mode");
        assert!(bindings[2].pango);
        assert_eq!(bindings[2].mods, (1 << 6) | (1 << 2));
        assert_eq!(bindings[2].command, "nop; mode default");
    }

    #[test]
    fn group_bindings_bindcodes_and_buttons_are_told_apart() {
        let config = "bindsym Mod4+Group2+z nop\n\
            bindsym Mod4+Mode_switch+z nop\n\
            bindcode Mod4+38 nop\n\
            bindsym Mod4+button1 nop\n\
            bindsym Mod4+a+b nop\n";
        let bindings = read(Wm::I3, config);
        assert_eq!(bindings.len(), 3);
        let to_code = read(Wm::Sway, "bindsym --to-code Mod4+z nop\n");
        assert_eq!(to_code[0].follows_layout(), None);
        assert_eq!(bindings[0].group, Some(1));
        assert_eq!(bindings[0].follows_layout(), None);
        assert_eq!(bindings[1].group, Some(1));
        assert_eq!(bindings[2].key, Key::Code(38));
        assert_eq!(bindings[2].follows_layout(), None);
    }

    #[test]
    fn sway_variables_expand_longest_name_first() {
        let config = "set $m Mod1\nset $mod Mod4\nset $ws $mod-ws\n\
            bindsym $mod+a workspace $ws $$HOME\n";
        let bindings = read(Wm::Sway, config);
        assert_eq!(bindings[0].mods, 1 << 6);
        assert_eq!(bindings[0].command, "workspace Mod4-ws $HOME");
    }

    #[test]
    fn resources_set_variables_or_leave_their_defaults() {
        let config = "set_from_resource $mod wm.mod Mod4\n\
            set_from_resource $wm.binding.focus_child wm.binding.focus_child a\n\
            set_from_resource $ws1 wm.workspace.01.name \"1\"\n\
            set_from_resource $empty wm.empty\n\
            bindsym $mod+$wm.binding.focus_child workspace $ws1\n\
            bindsym $mod+$empty nop\n";
        let resources = HashMap::from([
            ("wm.binding.focus_child".to_string(), "c".to_string()),
            ("wm.workspace.01.name".to_string(), String::new()),
        ]);
        let bindings = Reader::new(Wm::Sway, &Disk, true)
            .resources(resources)
            .read(config, None, Path::new("/nonexistent"));
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].mods, 1 << 6);
        assert_eq!(bindings[0].key, sym("c"));
        assert_eq!(bindings[0].command, "workspace \"1\"");
    }

    #[test]
    fn includes_are_read_in_place_except_skipped_files() {
        let dir = env::temp_dir().join(format!("inputd-config-{}", std::process::id()));
        fs::create_dir_all(dir.join("config.d")).unwrap();
        fs::write(dir.join("config.d/10-a"), "bindsym Mod4+a nop a\n").unwrap();
        fs::write(dir.join("config.d/20-b"), "bindsym Mod4+b nop b\n").unwrap();
        fs::write(dir.join("generated"), "bindsym Mod4+g nop g\n").unwrap();
        let config = "include generated\nbindsym Mod4+z nop z\ninclude config.d/*\n";
        let bindings = Reader::new(Wm::Sway, &Disk, true)
            .skip(Some(dir.join("generated")))
            .read(config, None, &dir);
        let commands: Vec<&str> = bindings.iter().map(|b| b.command.as_str()).collect();
        assert_eq!(commands, ["nop z", "nop a", "nop b"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn wildcards_match_like_the_shell() {
        assert!(wildcard("*.conf", "a.conf"));
        assert!(!wildcard("*.conf", "a.confx"));
        assert!(wildcard("[0-9]?-*", "10-keys"));
        assert!(!wildcard("[!0-9]*", "10-keys"));
        assert!(wildcard("a[]]b", "a]b"));
    }
}
