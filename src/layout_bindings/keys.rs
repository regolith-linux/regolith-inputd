//! Which keys produce a binding's keysym, in a layout group of an XKB keymap.

use super::config::{MOD_NAMES, SHIFT};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use xkbcommon::xkb;

/// i3 and sway handle at most four layout groups.
pub const MAX_GROUPS: u32 = 4;

/// A key of a binding: its code, the modifiers it is pressed with, and
/// whether the binding runs on release.
pub type KeyPress = (u32, u32, bool);

/// An XKB keymap and the layout groups it holds.
pub struct Layouts {
    keymap: xkb::Keymap,
    codes: Vec<u32>,
    /// The keymap's index of each core modifier.
    mod_indices: [u32; 8],
    states: RefCell<HashMap<(u32, u32), xkb::State>>,
    symbols: RefCell<HashMap<u32, HashSet<u32>>>,
}

/// The keypad keysyms, which i3 never resolves through Shift.
fn keypad(sym: u32) -> bool {
    (0xff80..=0xffbd).contains(&sym)
}

impl Layouts {
    pub fn new(keymap: xkb::Keymap) -> Layouts {
        let codes = (keymap.min_keycode().raw()..=keymap.max_keycode().raw())
            .filter(|&c| keymap.key_get_name(xkb::Keycode::new(c)).is_some())
            .collect();
        let mod_indices = MOD_NAMES.map(|name| keymap.mod_get_index(name));
        Layouts {
            keymap,
            codes,
            mod_indices,
            states: RefCell::new(HashMap::new()),
            symbols: RefCell::new(HashMap::new()),
        }
    }

    /// The keymap of `layouts` and `variants` (comma-separated, as in XKB)
    /// with `options`, from the evdev rules.
    pub fn from_names(layouts: &str, variants: &str, options: &str) -> Option<Layouts> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let options = (!options.is_empty()).then(|| options.to_string());
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            layouts,
            variants,
            options,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )?;
        Some(Layouts::new(keymap))
    }

    /// The plain US layout, whose key positions the fallback uses.
    pub fn us() -> Option<Layouts> {
        Layouts::from_names("us", "", "")
    }

    pub fn groups(&self) -> u32 {
        self.keymap.num_layouts().min(MAX_GROUPS)
    }

    pub fn code(&self, name: &str) -> Option<u32> {
        self.keymap.key_by_name(name).map(|c| c.raw())
    }

    pub fn name(&self, code: u32) -> Option<&str> {
        self.keymap.key_get_name(xkb::Keycode::new(code))
    }

    /// Run `f` on a state with the core modifiers `mods` held and `group`
    /// locked.
    fn with_state<T>(&self, mods: u32, group: u32, f: impl FnOnce(&xkb::State) -> T) -> T {
        let mut states = self.states.borrow_mut();
        let state = states.entry((mods, group)).or_insert_with(|| {
            let mut state = xkb::State::new(&self.keymap);
            let mask = (0..8)
                .filter(|i| mods & (1 << i) != 0 && self.mod_indices[*i] < 32)
                .fold(0, |m, i| m | (1 << self.mod_indices[i]));
            state.update_mask(mask, 0, 0, 0, 0, group);
            state
        });
        f(state)
    }

    fn sym(&self, code: u32, mods: u32, group: u32) -> u32 {
        self.with_state(mods, group, |s| {
            s.key_get_one_sym(xkb::Keycode::new(code)).raw()
        })
    }

    /// The shift level `code` is at with `mods` held in `group`.
    fn level(&self, code: u32, mods: u32, group: u32) -> Option<u32> {
        self.with_state(mods, group, |s| {
            let key = xkb::Keycode::new(code);
            let layout = s.key_get_layout(key);
            (layout != xkb::LAYOUT_INVALID).then(|| s.key_get_level(key, layout))
        })
    }

    /// Every keysym `group` can produce, at any shift level.
    fn symbols(&self, group: u32) -> HashSet<u32> {
        if let Some(symbols) = self.symbols.borrow().get(&group) {
            return symbols.clone();
        }
        let mut symbols = HashSet::new();
        for &code in &self.codes {
            let key = xkb::Keycode::new(code);
            let layout = self.with_state(0, group, |s| s.key_get_layout(key));
            if layout == xkb::LAYOUT_INVALID {
                continue;
            }
            for level in 0..self.keymap.num_levels_for_key(key, layout) {
                for sym in self.keymap.key_get_syms_by_level(key, layout, level) {
                    symbols.insert(sym.raw());
                }
            }
        }
        self.symbols.borrow_mut().insert(group, symbols.clone());
        symbols
    }

    pub fn produces(&self, group: u32, sym: u32) -> bool {
        self.symbols(group).contains(&sym)
    }

    /// Whether every lowercase letter `a` to `z` can be typed in `group`.
    pub fn latin(&self, group: u32) -> bool {
        let symbols = self.symbols(group);
        (u32::from(b'a')..=u32::from(b'z')).all(|letter| symbols.contains(&letter))
    }

    /// The keys i3 binds a keysym binding to when it resolves it in `group`:
    /// the keys that produce `sym` with `mods`, or without Shift when `sym`
    /// is a key's first or second level ("bindsym $mod+Shift+a" and
    /// "bindsym $mod+plus" alike).
    pub fn i3_keys(&self, sym: u32, mods: u32, group: u32) -> Vec<(u32, u32)> {
        self.codes
            .iter()
            .filter(|&&code| {
                let found = self.sym(code, mods, group);
                found == sym
                    || (self.level(code, mods, group).is_some_and(|l| l <= 1)
                        && !keypad(found)
                        && self.sym(code, mods ^ SHIFT, group) == sym)
            })
            .map(|&code| (code, mods))
            .collect()
    }

    /// The keys that, with a binding's modifiers, yield `sym` in `group`, and
    /// the modifiers they are pressed with. Shift selects the second shift
    /// level as usual: "$mod+plus" is Super+Shift+= in the US layout, and
    /// "$mod+Shift+z" is Super+Shift on the key that types "z".
    pub fn keys(&self, sym: u32, mods: u32, group: u32) -> Vec<(u32, u32)> {
        let mut keys = Vec::new();
        for &code in &self.codes {
            let found = self.sym(code, mods, group);
            if found == sym {
                keys.push((code, mods));
            } else if keypad(found) {
                continue;
            } else if mods & SHIFT != 0 {
                if self.level(code, mods, group).is_some_and(|l| l <= 1)
                    && self.sym(code, mods & !SHIFT, group) == sym
                {
                    keys.push((code, mods));
                }
            } else if self.sym(code, mods | SHIFT, group) == sym {
                keys.push((code, mods | SHIFT));
            }
        }
        keys
    }
}

/// The keys that run a keysym binding while `group` of `layouts` is in use:
/// those that yield the keysym in that group, or, when the group is
/// non-Latin and cannot produce the keysym at all, those that yield it in
/// the plain US layout, found by key name.
pub fn following_keys(
    layouts: &Layouts,
    us: &Layouts,
    sym: u32,
    mods: u32,
    group: u32,
) -> Vec<(u32, u32)> {
    if layouts.latin(group) || layouts.produces(group, sym) {
        return layouts.keys(sym, mods, group);
    }
    us.keys(sym, mods, 0)
        .into_iter()
        .filter_map(|(code, mods)| Some((layouts.code(us.name(code)?)?, mods)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOD4: u32 = 1 << 6;

    fn sym(name: &str) -> u32 {
        xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS).raw()
    }

    fn names(layouts: &Layouts, keys: Vec<(u32, u32)>) -> Vec<(String, u32)> {
        keys.into_iter()
            .map(|(c, m)| (layouts.name(c).unwrap().to_string(), m))
            .collect()
    }

    #[test]
    fn latin_means_all_lowercase_letters_at_any_level() {
        let layouts = Layouts::from_names("us,ru,epo,gr", "", "").unwrap();
        assert_eq!(layouts.groups(), 4);
        assert!(layouts.latin(0));
        assert!(!layouts.latin(1));
        assert!(layouts.latin(2));
        assert!(!layouts.latin(3));
    }

    #[test]
    fn keys_follow_the_group_and_its_shift_levels() {
        let layouts = Layouts::from_names("us,de", "", "").unwrap();
        let k = |s: &str, mods, group| names(&layouts, layouts.keys(sym(s), mods, group));
        assert_eq!(k("z", MOD4, 0), [("AB01".to_string(), MOD4)]);
        assert_eq!(k("z", MOD4, 1), [("AD06".to_string(), MOD4)]);
        assert_eq!(
            k("z", MOD4 | SHIFT, 1),
            [("AD06".to_string(), MOD4 | SHIFT)]
        );
        assert_eq!(k("plus", MOD4, 0), [("AE12".to_string(), MOD4 | SHIFT)]);
        assert_eq!(k("plus", MOD4, 1), [("AD12".to_string(), MOD4)]);
        // i3 itself binds "$mod+plus" without Shift.
        let i3 = names(&layouts, layouts.i3_keys(sym("plus"), MOD4, 0));
        assert_eq!(i3, [("AE12".to_string(), MOD4)]);
    }

    #[test]
    fn missing_keysyms_of_non_latin_groups_use_us_key_names() {
        let layouts = Layouts::from_names("ru,us", "", "").unwrap();
        let us = Layouts::us().unwrap();
        let k =
            |s: &str, group| names(&layouts, following_keys(&layouts, &us, sym(s), MOD4, group));
        assert_eq!(k("z", 0), [("AB01".to_string(), MOD4)]);
        assert_eq!(k("period", 0), [("AB10".to_string(), MOD4)]);
        assert_eq!(k("Greek_alpha", 0), []);
    }
}
