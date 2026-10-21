use crate::InputHandler;
use gio::{prelude::SettingsExtManual, Settings};
use log::info;
use std::error::Error;
use swayipc::{Connection as SwayConnection, Input};

/// Mirrors GNOME's cursor theme and size to sway's seats.
pub struct CursorHandler {
    settings: Settings,
    sway_connection: SwayConnection,
}

impl CursorHandler {
    pub fn new() -> CursorHandler {
        let settings = Settings::new("org.gnome.desktop.interface");
        let sway_connection = SwayConnection::new().unwrap();
        CursorHandler {
            settings,
            sway_connection,
        }
    }
    fn apply_cursor_theme(&mut self) -> Result<(), Box<dyn Error>> {
        let theme: String = self.settings().get("cursor-theme");
        let size: i32 = self.settings().get("cursor-size");
        let cmd = xcursor_theme_command(&theme, size);
        info!("{cmd}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }
}

/// sway's command for a cursor theme and size. sway also hands both to
/// XWayland and, as XCURSOR_THEME and XCURSOR_SIZE, to the programs it
/// starts afterwards.
fn xcursor_theme_command(theme: &str, size: i32) -> String {
    let theme = theme.replace('\\', "\\\\").replace('"', "\\\"");
    format!("seat * xcursor_theme \"{theme}\" {}", size.max(1))
}

impl InputHandler for CursorHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        match key {
            "cursor-theme" | "cursor-size" => {
                info!("org.gnome.desktop.interface -> Key: {key} changed");
                self.apply_cursor_theme()?
            }
            _ => (),
        };
        Ok(())
    }
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn sway_connection(&mut self) -> &mut SwayConnection {
        &mut self.sway_connection
    }
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>> {
        self.apply_cursor_theme()
    }
    fn sync_gsettings(&mut self, _: &Input) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
}

unsafe impl Send for CursorHandler {}

#[cfg(test)]
mod tests {
    use super::xcursor_theme_command;

    #[test]
    fn theme_and_size_map_to_seat_command() {
        assert_eq!(
            xcursor_theme_command("Adwaita", 24),
            "seat * xcursor_theme \"Adwaita\" 24"
        );
        assert_eq!(
            xcursor_theme_command("My \"odd\" theme", 0),
            "seat * xcursor_theme \"My \\\"odd\\\" theme\" 1"
        );
    }
}
