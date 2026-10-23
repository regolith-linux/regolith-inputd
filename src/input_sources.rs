use crate::InputHandler;
use gio::{prelude::SettingsExtManual, Settings};
use log::info;
use std::error::Error;
use swayipc::Connection as SwayConnection;

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
        // Layout is of form code+variant
        let (layouts, variants) = sources
            .into_iter()
            .map(|(_, layout)| {
                if layout.contains('+') {
                    let (layout, variant) = layout.split_once('+').unwrap();
                    (String::from(layout), String::from(variant))
                } else {
                    (layout, String::from(""))
                }
            })
            .reduce(|(layout, variant), (curr_layout, curr_variant)| {
                (layout + "," + &curr_layout, variant + "," + &curr_variant)
            })
            .ok_or("Invalid keyboard layout or variant")?;
        let layout_cmd = format!("input type:keyboard xkb_layout '{layouts}'");
        let vairants_cmd = format!("input type:keyboard xkb_variant '{variants}'");
        info!("{vairants_cmd}");
        info!("{layout_cmd}");
        self.sway_connection().run_command(vairants_cmd)?;
        self.sway_connection().run_command(layout_cmd)?;
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
    use super::xkb_options_command;

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
