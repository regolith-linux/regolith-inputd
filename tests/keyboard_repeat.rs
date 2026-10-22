//! Keyboard repeat from GNOME's keyboard settings.

mod common;

use common::{change, Daemon};

const KEYBOARD: &str = "org.gnome.desktop.peripherals.keyboard";

/// At login sway must get GNOME's repeat delay and rate, not keep its own
/// defaults (600 ms, 25 per second) until the user changes one of them.
#[test]
fn repeat_settings_are_applied_at_startup() {
    let (sway, _daemon) = Daemon::start(
        "repeat-startup",
        "[org/gnome/desktop/peripherals/keyboard]\n\
         delay=uint32 250\n\
         repeat-interval=uint32 20\n",
    );
    sway.wait_for_startup();
    let commands = sway.commands();
    for expected in [
        "input type:keyboard repeat_delay 250",
        "input type:keyboard repeat_rate 50",
    ] {
        assert!(
            commands.contains(&expected.to_string()),
            "sway got {commands:?}"
        );
    }
}

#[test]
fn gnome_defaults_are_applied_at_startup() {
    let (sway, _daemon) = Daemon::start("repeat-defaults", "");
    sway.wait_for_startup();
    let commands = sway.commands();
    for expected in [
        "input type:keyboard repeat_delay 500",
        "input type:keyboard repeat_rate 33",
    ] {
        assert!(
            commands.contains(&expected.to_string()),
            "sway got {commands:?}"
        );
    }
}

/// Turning "Repeat Keys" off in Settings must stop repeating, and turning
/// it back on must restore the rate.
#[test]
fn repeat_off_and_on() {
    let (sway, daemon) = Daemon::start(
        "repeat-toggle",
        "[org/gnome/desktop/peripherals/keyboard]\nrepeat-interval=uint32 40\n",
    );
    sway.wait_for_startup();
    assert_eq!(
        change(
            &sway,
            &daemon,
            (KEYBOARD, "repeat", "false"),
            "input type:keyboard repeat_rate"
        )
        .as_deref(),
        Some("input type:keyboard repeat_rate 0")
    );
    assert_eq!(
        change(
            &sway,
            &daemon,
            (KEYBOARD, "repeat-interval", "20"),
            "input type:keyboard repeat_rate"
        )
        .as_deref(),
        Some("input type:keyboard repeat_rate 0"),
        "changing the interval must not turn repeat back on"
    );
    assert_eq!(
        change(
            &sway,
            &daemon,
            (KEYBOARD, "repeat", "true"),
            "input type:keyboard repeat_rate"
        )
        .as_deref(),
        Some("input type:keyboard repeat_rate 50")
    );
}
