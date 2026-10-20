//! Settings that the daemon already mirrored to sway before these tests
//! were added; they also guard the harness itself.

mod common;

use common::{change, Daemon};

const TOUCHPAD: &str = "org.gnome.desktop.peripherals.touchpad";

#[test]
fn touchpad_settings_are_applied_at_startup_and_on_change() {
    let (sway, daemon) = Daemon::start(
        "settings-sync",
        "[org/gnome/desktop/input-sources]\n\
         sources=[('xkb', 'us')]\n\
         [org/gnome/desktop/peripherals/touchpad]\n\
         tap-to-click=false\n",
    );
    sway.wait_for_startup();
    let commands = sway.commands();
    assert!(
        commands.contains(&"input type:touchpad tap disabled".to_string()),
        "sway got {commands:?}"
    );
    assert!(
        commands.contains(&"input type:keyboard xkb_layout 'us'".to_string()),
        "sway got {commands:?}"
    );

    assert_eq!(
        change(
            &sway,
            &daemon,
            (TOUCHPAD, "tap-to-click", "true"),
            "input type:touchpad tap"
        )
        .as_deref(),
        Some("input type:touchpad tap enabled")
    );
}
