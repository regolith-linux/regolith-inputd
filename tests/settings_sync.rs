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

/// A user who never chose an input source in GNOME (for example one whose
/// first login is into the Wayland session) has an empty `sources` list.
/// The daemon must keep running and keep mirroring the other settings.
#[test]
fn no_input_sources_configured() {
    let (sway, mut daemon) = Daemon::start(
        "no-sources",
        "[org/gnome/desktop/peripherals/touchpad]\ntap-to-click=false\n",
    );
    sway.wait_for_startup();
    assert!(
        daemon.is_running(),
        "daemon exited; sway got {:?}",
        sway.commands()
    );
    assert!(!sway
        .commands()
        .iter()
        .any(|c| c.starts_with("input type:keyboard xkb_layout")));

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

/// GNOME's send-events values must reach sway in sway's spelling, which
/// uses underscores ("disabled_on_external_mouse").
#[test]
fn touchpad_send_events_uses_sway_names() {
    let (sway, daemon) = Daemon::start(
        "send-events",
        "[org/gnome/desktop/input-sources]\nsources=[('xkb', 'us')]\n",
    );
    sway.wait_for_startup();
    for (value, expected) in [
        (
            "disabled-on-external-mouse",
            "input type:touchpad events disabled_on_external_mouse",
        ),
        ("disabled", "input type:touchpad events disabled"),
        ("enabled", "input type:touchpad events enabled"),
    ] {
        assert_eq!(
            change(
                &sway,
                &daemon,
                (TOUCHPAD, "send-events", value),
                "input type:touchpad events"
            )
            .as_deref(),
            Some(expected),
            "send-events {value}"
        );
    }
}
