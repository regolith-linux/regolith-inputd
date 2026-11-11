use crate::ibus_engines::{self, DEFAULT_LAYOUT};
use crate::input_method::InputMethod;
use crate::InputHandler;
use gio::prelude::{SettingsExt, SettingsExtManual};
use gio::Settings;
use log::{error, info, warn};
use std::error::Error;
use swayipc::Connection as SwayConnection;

/// A source of the effective source list: its identity in the `sources`
/// setting, the XKB layout and variant it applies, and for an IBus source
/// its engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveSource {
    pub source: (String, String),
    pub layout: String,
    pub variant: String,
    pub engine: Option<String>,
}

/// The sources in effect, in list order, as in GNOME Shell: an xkb source
/// applies its own layout (`layout+variant`), an IBus engine the layout and
/// variant of its description (see [`ibus_engines::installed`]). IBus
/// engines with no description, and sources of any other type, are left
/// out.
pub fn effective_sources(sources: Vec<(String, String)>) -> Vec<EffectiveSource> {
    let mut engines = None;
    let mut effective = Vec::new();
    for (kind, id) in sources {
        let ((layout, variant), engine) = match kind.as_str() {
            "xkb" => match id.split_once('+') {
                Some((layout, variant)) => ((layout.to_string(), variant.to_string()), None),
                None => ((id.clone(), String::new()), None),
            },
            "ibus" => match engines.get_or_insert_with(ibus_engines::installed).get(&id) {
                Some(desc) => (desc.clone(), Some(id.clone())),
                None => continue,
            },
            _ => continue,
        };
        effective.push(EffectiveSource {
            source: (kind, id),
            layout,
            variant,
            engine,
        });
    }
    effective
}

pub struct InputSourcesHandler {
    settings: Settings,
    sway_connection: SwayConnection,
    input_method: InputMethod,
    /// The `sources` value last applied, and what it gave.
    applied: (Vec<(String, String)>, Vec<EffectiveSource>),
    /// New engine lists from IBus, until the handler follows them.
    engine_lists: Option<glib::Receiver<()>>,
}
impl InputSourcesHandler {
    pub fn new() -> InputSourcesHandler {
        let settings = Settings::new("org.gnome.desktop.input-sources");
        let sway_connection = SwayConnection::new().unwrap();
        // Watch before the first list is applied: an engine list IBus
        // reports from now on is followed.
        let engine_lists = Some(ibus_engines::watch());
        InputSourcesHandler {
            settings,
            sway_connection,
            input_method: InputMethod::start(),
            applied: Default::default(),
            engine_lists,
        }
    }
    fn apply_input_sources(&mut self) -> Result<(), Box<dyn Error>> {
        let sources: Vec<(String, String)> = self.settings().get("sources");
        if sources.is_empty() {
            info!("No input sources set in GNOME, keeping sway's keyboard layout");
            self.applied = Default::default();
            self.input_method.set_sources(Vec::new());
            return Ok(());
        }
        let effective = effective_sources(sources.clone());
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
        // The layouts are in: the input method follows the new list.
        self.applied = (sources, effective.clone());
        self.input_method.set_sources(effective);
        Ok(())
    }
    /// IBus reported a new engine list: apply the sources again if it
    /// changes which of the applied ones count or what they apply. Otherwise
    /// nothing is sent, and the source in use stays. (A change of the
    /// setting itself is applied when its own notification arrives.)
    fn follow_engine_list(&mut self) -> Result<(), Box<dyn Error>> {
        let (sources, effective) = &self.applied;
        if effective_sources(sources.clone()) == *effective {
            return Ok(());
        }
        info!("IBus's engines change the input sources, applying them again");
        self.apply_input_sources()
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
    fn monitor_gsettings_change(&mut self)
    where
        Self: 'static,
    {
        let ptr: *mut Self = self;
        self.settings().connect_changed(None, move |_, key| unsafe {
            if let Err(e) = (*ptr).apply_changes_sync(key) {
                error!("{e}");
            }
        });
        if let Some(engine_lists) = self.engine_lists.take() {
            engine_lists.attach(None, move |()| {
                if let Err(e) = unsafe { (*ptr).follow_engine_list() } {
                    error!("{e}");
                }
                glib::Continue(true)
            });
        }
    }
}
unsafe impl Send for InputSourcesHandler {}

#[cfg(test)]
mod tests {
    use super::{effective_sources, xkb_options_command};

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
