//! The names GNOME gives keyboard layouts, from XKB's rules descriptions.
//!
//! GNOME labels an xkb source with the short description and names it with
//! the description its layout, or its variant, has in the `evdev` rules
//! description, read through libxkbregistry. A variant without a short
//! description of its own takes its layout's. The texts are used as written
//! there, untranslated.
//!
//! The rules descriptions are read from the directories libxkbregistry (and
//! xkbcommon, which compiles sway's keymaps) searches, in its order: the
//! user's `$XDG_CONFIG_HOME/xkb` and `~/.xkb`, then `/etc/xkb`, then the
//! system's `$XKB_CONFIG_ROOT` (`/usr/share/X11/xkb` by default). The first
//! description of a layout or variant wins. `evdev.extras.xml` is read after
//! `evdev.xml`, so a configured source from the extra layouts is named too.

use crate::input_sources::{element, strip_comments};
use log::debug;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

/// The system's XKB data, when `$XKB_CONFIG_ROOT` doesn't name another.
const XKB_CONFIG_ROOT: &str = "/usr/share/X11/xkb";

/// The rules descriptions of each directory, in the order they are read.
const RULES_FILES: [&str; 2] = ["rules/evdev.xml", "rules/evdev.extras.xml"];

/// The short form and the full name of a layout or of a variant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Description {
    pub short: String,
    pub name: String,
}

/// A layout or variant as a rules description gives it.
struct Item {
    short: Option<String>,
    name: String,
}

/// The layouts and variants of the rules descriptions, by `layout` and by
/// `layout+variant`.
#[derive(Default)]
pub struct XkbRules {
    items: HashMap<String, Item>,
}

/// The directories searched for XKB data, in libxkbregistry's order.
fn include_paths() -> Vec<PathBuf> {
    let home = env::var_os("HOME").map(PathBuf::from);
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    let root = env::var_os("XKB_CONFIG_ROOT")
        .map_or_else(|| PathBuf::from(XKB_CONFIG_ROOT), PathBuf::from);
    config
        .map(|c| c.join("xkb"))
        .into_iter()
        .chain(home.map(|h| h.join(".xkb")))
        .chain([PathBuf::from("/etc/xkb"), root])
        .collect()
}

impl XkbRules {
    /// The session's rules descriptions. Without any, every layout is
    /// described by its own name.
    pub fn load() -> XkbRules {
        let mut rules = XkbRules::default();
        for dir in include_paths() {
            for file in RULES_FILES {
                let path = dir.join(file);
                match fs::read_to_string(&path) {
                    Ok(xml) => rules.add(&xml),
                    Err(e) => debug!("{}: {e}", path.display()),
                }
            }
        }
        rules
    }

    /// Add the layouts and variants of a rules description that aren't
    /// described yet. A layout listed only to add variants to it (without a
    /// description of its own) leaves the layout's description as it is.
    fn add(&mut self, xml: &str) {
        let text = strip_comments(xml);
        for block in blocks(&text, "layout") {
            let (head, variants) = block.split_once("<variantList").unwrap_or((block, ""));
            let Some((layout, item)) = config_item(head) else {
                continue;
            };
            for variant in blocks(variants, "variant") {
                if let Some((variant, Some(item))) = config_item(variant) {
                    self.items
                        .entry(format!("{layout}+{variant}"))
                        .or_insert(item);
                }
            }
            if let Some(item) = item {
                self.items.entry(layout).or_insert(item);
            }
        }
    }

    /// The short form and the name of `layout` with `variant` (empty for
    /// none), if the rules describe it. A variant without a short form
    /// takes its layout's; without either, the layout's own name is the
    /// short form.
    pub fn describe(&self, layout: &str, variant: &str) -> Option<Description> {
        let main = self.items.get(layout);
        let item = if variant.is_empty() {
            main?
        } else {
            self.items.get(&format!("{layout}+{variant}"))?
        };
        let short = item
            .short
            .as_ref()
            .or_else(|| main.and_then(|m| m.short.as_ref()))
            .map_or_else(|| layout.to_string(), Clone::clone);
        Some(Description {
            short,
            name: item.name.clone(),
        })
    }
}

/// The contents of every `<tag>` element of `xml`, attributes allowed, in
/// order.
fn blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut found = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        // `<layout` must not match `<layoutList>`.
        if !after.starts_with(|c: char| c == '>' || c.is_whitespace()) {
            rest = after;
            continue;
        }
        let Some(gt) = after.find('>') else {
            break;
        };
        let body = &after[gt + 1..];
        let end = body.find(&close).unwrap_or(body.len());
        found.push(&body[..end]);
        rest = &body[end..];
    }
    found
}

/// The name of the first `<configItem>` of `xml` and, if the item has a
/// description, what it describes.
fn config_item(xml: &str) -> Option<(String, Option<Item>)> {
    let item = blocks(xml, "configItem").into_iter().next()?;
    let name = element(item, "name").filter(|n| !n.is_empty())?;
    let description = element(item, "description")
        .filter(|d| !d.is_empty())
        .map(|description| Item {
            short: element(item, "shortDescription").filter(|s| !s.is_empty()),
            name: description,
        });
    Some((name, description))
}

#[cfg(test)]
mod tests {
    use super::{Description, XkbRules};

    const RULES: &str = "<xkbConfigRegistry version=\"1.1\"><layoutList>\
        <layout><configItem><name>ca</name><!-- French -->\
        <shortDescription>fr</shortDescription><description>French (Canada)</description>\
        </configItem><variantList>\
        <variant><configItem><name>eng</name><shortDescription>en</shortDescription>\
        <description>English (Canada)</description></configItem></variant>\
        <variant><configItem popularity=\"exotic\"><name>fr-dvorak</name>\
        <description>French (Canada, Dvorak)</description></configItem></variant>\
        </variantList></layout>\
        <layout><configItem><name>x</name><description>A &amp; B</description>\
        </configItem></layout>\
        </layoutList></xkbConfigRegistry>";

    /// A user's rules description: a variant added to a system layout, and
    /// a layout the system describes too.
    const USER_RULES: &str = "<xkbConfigRegistry version=\"1.1\"><layoutList>\
        <layout><configItem><name>ca</name></configItem><variantList>\
        <variant><configItem><name>mine</name><description>Mine</description>\
        </configItem></variant></variantList></layout>\
        <layout><configItem><name>x</name><shortDescription>y</shortDescription>\
        <description>Y</description></configItem></layout>\
        </layoutList></xkbConfigRegistry>";

    fn d(short: &str, name: &str) -> Option<Description> {
        Some(Description {
            short: short.into(),
            name: name.into(),
        })
    }

    #[test]
    fn layouts_and_variants_are_described() {
        let mut rules = XkbRules::default();
        rules.add(RULES);
        assert_eq!(rules.describe("ca", ""), d("fr", "French (Canada)"));
        assert_eq!(rules.describe("ca", "eng"), d("en", "English (Canada)"));
        // A variant without a short description takes its layout's, also
        // when its item carries attributes.
        assert_eq!(
            rules.describe("ca", "fr-dvorak"),
            d("fr", "French (Canada, Dvorak)")
        );
        // Without either, the layout's name is the short form.
        assert_eq!(rules.describe("x", ""), d("x", "A & B"));
        assert_eq!(rules.describe("ca", "missing"), None);
        assert_eq!(rules.describe("eng", ""), None);
    }

    #[test]
    fn earlier_descriptions_win_and_add_variants() {
        let mut rules = XkbRules::default();
        rules.add(USER_RULES);
        rules.add(RULES);
        // The user's variant takes the short form the system gives ca, and
        // the system's ca keeps its description.
        assert_eq!(rules.describe("ca", "mine"), d("fr", "Mine"));
        assert_eq!(rules.describe("ca", ""), d("fr", "French (Canada)"));
        assert_eq!(rules.describe("ca", "eng"), d("en", "English (Canada)"));
        assert_eq!(rules.describe("x", ""), d("y", "Y"));
    }
}
