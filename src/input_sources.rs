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
        let delay: Vec<(String, String)> = self.settings().get("sources");
        // Layout is of form code+variant
        let (layouts, variants) = delay
            .into_iter()
            .map(|(_, layout)| {
                if layout.contains("+") {
                    let (layout, variant) = layout.split_once("+").unwrap();
                    (String::from(layout), Some(String::from(variant)))
                } else {
                    (layout, None)
                }
            })
            .reduce(|(layout, variant), (curr_layout, curr_variant)| {
                let updated_variant = if variant.is_none() && curr_variant.is_some() {
                    curr_variant
                } else if variant.is_some() && curr_variant.is_some() {
                    let curr_variant_val = curr_variant.as_deref().unwrap();
                    let variant_val = variant.unwrap();
                    Some(variant_val + "," + curr_variant_val)
                } else {
                    variant
                };
                (layout + "," + &curr_layout, updated_variant)
            })
            .ok_or("Invalid keyboard layout or variant")?;
        let variants_str = if let Some(variants) = variants {
            variants
        } else {
            String::new()
        };
        let layout_cmd = format!("input type:keyboard xkb_layout '{layouts}'");
        let vairants_cmd = format!("input type:keyboard xkb_variant '{variants_str}'");
        info!("{vairants_cmd}");
        info!("{layout_cmd}");
        self.sway_connection().run_command(vairants_cmd)?;
        self.sway_connection().run_command(layout_cmd)?;
        Ok(())
    }
}

impl InputHandler for InputSourcesHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        info!("org.gnome.desktop.input-sources -> Key: {key} chaged");
        match key {
            "sources" => self.apply_input_sources()?,
            _ => (),
        };
        Ok(())
    }
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>> {
        self.apply_input_sources()
    }
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn sync_gsettings(&mut self, input: swayipc::Input) -> Result<(), Box<dyn Error>> {
        info!("xkb_layout: {}", input.xkb_layout_names[0]);
        Ok(())
    }
    fn sway_connection(&mut self) -> &mut swayipc::Connection {
        &mut self.sway_connection
    }
}
unsafe impl Send for InputSourcesHandler {}
