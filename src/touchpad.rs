use crate::traits::{InputHandler, PointerMethods, PrimitiveToSwayType, SwayTypeToPrimitive};
use gio::{prelude::SettingsExtManual, traits::SettingsExt, Settings};
use log::info;

use std::error::Error;
use swayipc::{Connection as SwayConnection, Input};

pub struct TouchpadHandler {
    settings: Settings,
    sway_connection: SwayConnection,
}
impl TouchpadHandler {
    pub fn new() -> TouchpadHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.touchpad");
        let connection = SwayConnection::new().unwrap();
        TouchpadHandler {
            settings,
            sway_connection: connection,
        }
    }
    fn apply_tap(&mut self) -> Result<(), Box<dyn Error>> {
        let tap_enabled: &str = self
            .settings
            .get::<bool>("tap-to-click")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad tap {tap_enabled}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn apply_scroll_method(&mut self) -> Result<(), Box<dyn Error>> {
        let two_finger: bool = self.settings().get("two-finger-scrolling-enabled");
        let edge_scroll: bool = self.settings().get("edge-scrolling-enabled");
        let scroll_method = if two_finger {
            "two_finger"
        } else if edge_scroll {
            "edge"
        } else {
            "none"
        };
        let cmd = format!("input type:touchpad scroll_method {scroll_method}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn use_dwt(&mut self) -> Result<(), Box<dyn Error>> {
        let dwt_enabled: &str = self
            .settings
            .get::<bool>("disable-while-typing")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad dwt {dwt_enabled}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn send_events(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: String = self.settings().get("send-events");
        let cmd = format!("input type:touchpad events {new_val}");
        info!("{cmd}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn use_drag(&mut self) -> Result<(), Box<dyn Error>> {
        let use_drag_enabled: &str = self
            .settings
            .get::<bool>("tap-and-drag")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad drag {use_drag_enabled}");
        info!("{cmd}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn use_drag_lock(&mut self) -> Result<(), Box<dyn Error>> {
        let drag_lock_enabled: &str = self
            .settings
            .get::<bool>("tap-and-drag-lock")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad drag_lock {drag_lock_enabled}");
        info!("{cmd}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }

    fn emulate_middle_click(&mut self) -> Result<(), Box<dyn Error>> {
        let emulate_middle_enabled: &str = self
            .settings
            .get::<bool>("middle-click-emulation")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad middle_emulation {emulate_middle_enabled}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
}

impl PointerMethods for TouchpadHandler {
    fn pointer_type(&self) -> &str {
        "touchpad"
    }
    fn apply_left_handed(&mut self) -> Result<(), Box<dyn Error>> {
        let left_handed: String = self.settings().get("left-handed");

        let left_handed_enabled: &str = match left_handed.as_ref() {
            "left" => true,
            "right" => false,
            "mouse" => {
                let mouse_settings = Settings::new("org.gnome.desktop.peripherals.mouse");
                mouse_settings.get("left-handed")
            }
            _ => false,
        }
        .to_sway_type()
        .to_primitive();
        let pointer_type = self.pointer_type();
        let cmd = format!("input type:{pointer_type} left_handed {left_handed_enabled}");
        info!("{cmd}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }

    /**
     * See https://manpages.debian.org/experimental/sway/sway-input.5.en.html
     * 
     * How to generate software-emulated buttons, either disabled (“none”), 
     * through specific areas (“areas”), number of fingers (“fingers”) or 
     * left as hardware default (“default”).
     */
    fn apply_click_method(&mut self) -> Result<(), Box<dyn Error>> {
        let click_method: String = self.settings().get("click-method");

        let click_method: &str = match click_method.as_ref() {
            "areas" => "button_areas",
            "fingers" => "clickfinger",
            _ => "none",
        };
        
        let pointer_type = self.pointer_type();
        let cmd = format!("input type:{pointer_type} click_method {click_method}");
        info!("{cmd}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }
}

impl InputHandler for TouchpadHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        info!("org.gnome.desktop.peripherals.touchpad -> Key: {key} chaged");
        match key {
            "speed" => self.apply_speed()?,
            "natural-scroll" => self.apply_natural_scroll()?,
            "tap-to-click" => self.apply_tap()?,
            "two-finger-scrolling-enabled" | "edge-scrolling-enabled" => {
                self.apply_scroll_method()?
            }
            "left-handed" => self.apply_left_handed()?,
            "send-events" => self.send_events()?,
            "disable-while-typing" => self.use_dwt()?,
            "tap-and-drag" => self.use_drag()?,
            "tap-and-drag-lock" => self.use_drag_lock()?,
            "middle-click-emulation" => self.emulate_middle_click()?,
            "click_method" => self.apply_click_method()?,
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
        self.apply_tap()?;
        self.apply_scroll_method()?;
        self.send_events()?;
        self.use_dwt()?;
        self.use_drag()?;
        self.use_drag_lock()?;
        self.emulate_middle_click()?;
        Ok(())
    }
    fn sync_gsettings(&mut self, input: &Input) -> Result<(), Box<dyn Error>> {
        self.sync_pointer_gsettings(input)?;
        if input.libinput.is_none() {
            return Ok(());
        }
        let libinput = input.libinput.as_ref().unwrap();
        if let Some(enabled) = libinput.send_events.as_ref() {
            self.settings()
                .set_string("send-events", enabled.to_primitive())?;
        }
        if let Some(tap) = &libinput.tap {
            self.settings()
                .set_boolean("tap-to-click", tap.to_primitive())?;
        }
        if let Some(drag) = &libinput.tap_drag {
            self.settings()
                .set_boolean("tap-and-drag", drag.to_primitive())?;
        }
        if let Some(drag_lock) = &libinput.tap_drag_lock {
            self.settings()
                .set_boolean("tap-and-drag-lock", drag_lock.to_primitive())?;
        }
        Ok(())
    }
}
unsafe impl Send for TouchpadHandler {}
unsafe impl Sync for TouchpadHandler {}
