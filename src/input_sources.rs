use crate::InputHandler;
use gio::{prelude::SettingsExtManual, Settings};
use log::{info, warn};
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use swayipc::Connection as SwayConnection;

/// The layout GNOME applies when no source is usable, and the one IBus
/// gives an engine whose description has none.
const DEFAULT_LAYOUT: &str = "us";

/// A source of the effective source list: its identity in the `sources`
/// setting and the XKB layout and variant it applies.
pub struct EffectiveSource {
    pub source: (String, String),
    pub layout: String,
    pub variant: String,
}

/// The sources in effect, in list order, as in GNOME Shell: an xkb source
/// applies its own layout (`layout+variant`), an IBus engine the layout and
/// variant of its installed description. IBus engines with no installed
/// description, and sources of any other type, are left out.
pub fn effective_sources(sources: Vec<(String, String)>) -> Vec<EffectiveSource> {
    let mut engines: Option<HashMap<String, (String, String)>> = None;
    let mut effective = Vec::new();
    for (kind, id) in sources {
        let (layout, variant) = match kind.as_str() {
            "xkb" => match id.split_once('+') {
                Some((layout, variant)) => (layout.to_string(), variant.to_string()),
                None => (id.clone(), String::new()),
            },
            "ibus" => match engines.get_or_insert_with(ibus_engines).get(&id) {
                Some(desc) => desc.clone(),
                None => continue,
            },
            _ => continue,
        };
        effective.push(EffectiveSource {
            source: (kind, id),
            layout,
            variant,
        });
    }
    effective
}

/// The installed IBus engine descriptions, by engine name: the layout and
/// variant of each. They are read from IBus's component directories
/// (`$IBUS_COMPONENT_PATH`, else `/usr/share/ibus/component`).
fn ibus_engines() -> HashMap<String, (String, String)> {
    let dirs: Vec<PathBuf> = match env::var("IBUS_COMPONENT_PATH") {
        Ok(path) => env::split_paths(&path).collect(),
        Err(_) => vec![PathBuf::from("/usr/share/ibus/component")],
    };
    let mut engines = HashMap::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for path in entries.flatten().map(|e| e.path()) {
            if path.extension().map_or(true, |e| e != "xml") {
                continue;
            }
            if let Ok(xml) = fs::read_to_string(&path) {
                engines.extend(parse_engines(&xml));
            }
        }
    }
    engines
}

/// The `<engine>` elements of an IBus component description.
fn parse_engines(xml: &str) -> Vec<(String, (String, String))> {
    let mut text = String::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<!--") {
        text.push_str(&rest[..start]);
        rest = rest[start..]
            .find("-->")
            .map_or("", |end| &rest[start + end + 3..]);
    }
    text.push_str(rest);
    let mut engines = Vec::new();
    for block in text.split("<engine>").skip(1) {
        let block = block.split("</engine>").next().unwrap_or_default();
        let Some(name) = element(block, "name") else {
            continue;
        };
        let layout = element(block, "layout")
            .filter(|l| !l.is_empty() && l != "default")
            .unwrap_or_else(|| String::from(DEFAULT_LAYOUT));
        let variant = element(block, "layout_variant").unwrap_or_default();
        engines.push((name, (layout, variant)));
    }
    engines
}

/// The text of the first `<tag>` element in `xml`, unescaped.
fn element(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&close)?;
    let text = xml[start..end].trim();
    Some(
        text.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

pub struct InputSourcesHandler {
    settings: Settings,
    sway_connection: SwayConnection,
}
impl InputSourcesHandler {
    pub fn new() -> InputSourcesHandler {
        let settings = Settings::new("org.gnome.desktop.input-sources");
        let sway_connection = SwayConnection::new().unwrap();
        InputSourcesHandler {
            settings,
            sway_connection,
        }
    }
    fn apply_input_sources(&mut self) -> Result<(), Box<dyn Error>> {
        let sources: Vec<(String, String)> = self.settings().get("sources");
        if sources.is_empty() {
            info!("No input sources set in GNOME, keeping sway's keyboard layout");
            return Ok(());
        }
        let effective = effective_sources(sources);
        let (layouts, variants) = if effective.is_empty() {
            info!("No usable input source, applying the default layout");
            (String::from(DEFAULT_LAYOUT), String::new())
        } else {
            let layouts: Vec<&str> = effective.iter().map(|e| e.layout.as_str()).collect();
            let variants: Vec<&str> = effective.iter().map(|e| e.variant.as_str()).collect();
            (layouts.join(","), variants.join(","))
        };
        // sway compiles every input command against the other stored field,
        // so a new variant next to the old layouts (or new layouts next to
        // the old variants) can be rejected. Clear the variants, then set the
        // layouts, then their variants: each step is valid on its own.
        let cmds = [
            String::from("input type:keyboard xkb_variant ''"),
            format!("input type:keyboard xkb_layout '{layouts}'"),
            format!("input type:keyboard xkb_variant '{variants}'"),
        ];
        for cmd in cmds {
            info!("{cmd}");
            for outcome in self.sway_connection().run_command(&cmd)? {
                if let Err(e) = outcome {
                    warn!("{cmd}: {e}");
                }
            }
        }
        Ok(())
    }
    /// Apply GNOME's XKB options (compose key, alternate characters key and
    /// the like). An empty list is only sent when the user changes the key:
    /// at startup and after a sway reload it means GNOME has no options, and
    /// any set in the user's sway config are kept.
    fn apply_xkb_options(&mut self, changed: bool) -> Result<(), Box<dyn Error>> {
        let options: Vec<String> = self.settings().get("xkb-options");
        if options.is_empty() && !changed {
            return Ok(());
        }
        let cmd = xkb_options_command(&options);
        info!("{cmd}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }
}

/// The options are quoted because sway splits an unquoted command at
/// commas and would run every option after the first as a command.
fn xkb_options_command(options: &[String]) -> String {
    format!("input type:keyboard xkb_options '{}'", options.join(","))
}

impl InputHandler for InputSourcesHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        info!("org.gnome.desktop.input-sources -> Key: {key} chaged");
        match key {
            "sources" => self.apply_input_sources()?,
            "xkb-options" => self.apply_xkb_options(true)?,
            _ => (),
        };
        Ok(())
    }
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>> {
        self.apply_input_sources()?;
        self.apply_xkb_options(false)
    }
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn sync_gsettings(&mut self, input: &swayipc::Input) -> Result<(), Box<dyn Error>> {
        info!("xkb_layout: {}", input.xkb_layout_names[0]);
        Ok(())
    }
    fn sway_connection(&mut self) -> &mut swayipc::Connection {
        &mut self.sway_connection
    }
}
unsafe impl Send for InputSourcesHandler {}

#[cfg(test)]
mod tests {
    use super::{effective_sources, parse_engines, xkb_options_command};

    #[test]
    fn xkb_sources_are_split_into_layout_and_variant() {
        let sources = vec![
            ("xkb".into(), "us".into()),
            ("other".into(), "de".into()),
            ("xkb".into(), "de+nodeadkeys".into()),
        ];
        let effective: Vec<(String, String)> = effective_sources(sources)
            .into_iter()
            .map(|e| (e.layout, e.variant))
            .collect();
        assert_eq!(
            effective,
            vec![("us".into(), "".into()), ("de".into(), "nodeadkeys".into())]
        );
    }

    #[test]
    fn ibus_engine_descriptions_are_parsed() {
        let xml = "<component><name>c</name><!-- <engine><name>x</name></engine> -->\
            <engines><engine><name>xkb:jp::jpn</name><layout>jp</layout></engine>\
            <engine><name>e</name><layout_variant>dvorak</layout_variant></engine>\
            </engines></component>";
        let engines = parse_engines(xml);
        assert_eq!(
            engines,
            vec![
                ("xkb:jp::jpn".into(), ("jp".into(), "".into())),
                ("e".into(), ("us".into(), "dvorak".into())),
            ]
        );
    }

    #[test]
    fn xkb_options_are_joined_and_quoted() {
        let options = ["compose:ralt".to_string(), "lv3:menu_switch".to_string()];
        assert_eq!(
            xkb_options_command(&options),
            "input type:keyboard xkb_options 'compose:ralt,lv3:menu_switch'"
        );
        assert_eq!(
            xkb_options_command(&[]),
            "input type:keyboard xkb_options ''"
        );
    }
}
