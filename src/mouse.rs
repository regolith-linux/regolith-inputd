use crate::traits::{InputHandler, PointerMethods, PrimitiveToSwayType, SwayTypeToPrimitive};
use gio::{prelude::SettingsExtManual, Settings};
use log::info;
use std::error::Error;
use swayipc::{Connection as SwayConnection, Input};
pub struct MouseHandler {
    settings: Settings,
    sway_connection: SwayConnection,
}

impl MouseHandler {
    pub fn new() -> MouseHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.mouse");
        let sway_connection = SwayConnection::new().unwrap();
        MouseHandler {
            settings,
            sway_connection,
        }
    }
}

impl PointerMethods for MouseHandler {
    fn pointer_type(&self) -> &str {
        "pointer"
    }
    fn apply_left_handed(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: &str = self
            .settings()
            .get::<bool>("left-handed")
            .to_sway_type()
            .to_primitive();
        let pointer_type = self.pointer_type();
        let cmd = format!("input type:{pointer_type} left_handed {new_val}");
        info!("Executing command: {cmd}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }
}

impl InputHandler for MouseHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        info!("org.gnome.desktop.peripherals.mouse -> Key: {key} chaged");
        match key {
            "speed" => self.apply_speed()?,
            "natural-scroll" => self.apply_natural_scroll()?,
            "left-handed" => self.apply_left_handed()?,
            _ => (),
        };
        Ok(())
    }
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn sway_connection(&mut self) -> &mut swayipc::Connection {
        &mut self.sway_connection
    }
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>> {
        self.apply_speed()?;
        self.apply_left_handed()?;
        self.apply_natural_scroll()?;
        Ok(())
    }
    fn sync_gsettings(&mut self, input: Input) -> Result<(), Box<dyn Error>> {
        info!("Syncronizing mouse input state of sway with gsettings...");
        self.sync_pointer_gsettings(&input)?;
        Ok(())
    }
}

unsafe impl Send for MouseHandler {}
