//! GNOME's cursor theme and size for sway's own cursor.

mod common;

use common::{change, Daemon};

const INTERFACE: &str = "org.gnome.desktop.interface";

#[test]
fn cursor_theme_and_size_are_applied_at_startup_and_on_change() {
    let (sway, daemon) = Daemon::start(
        "cursor",
        "[org/gnome/desktop/interface]\n\
         cursor-theme='DMZ-White'\n\
         cursor-size=32\n",
    );
    sway.wait_for_startup();
    let commands = sway.commands();
    assert!(
        commands.contains(&"seat * xcursor_theme \"DMZ-White\" 32".to_string()),
        "sway got {commands:?}"
    );

    assert_eq!(
        change(
            &sway,
            &daemon,
            (INTERFACE, "cursor-size", "48"),
            "seat * xcursor_theme"
        )
        .as_deref(),
        Some("seat * xcursor_theme \"DMZ-White\" 48")
    );
    assert_eq!(
        change(
            &sway,
            &daemon,
            (INTERFACE, "cursor-theme", "'Yaru'"),
            "seat * xcursor_theme"
        )
        .as_deref(),
        Some("seat * xcursor_theme \"Yaru\" 48")
    );
}
