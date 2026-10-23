//! Pointer acceleration profile of mice and touchpads.
//!
//! The touchpad `accel-profile` key only exists from GNOME 46
//! (gsettings-desktop-schemas 46). Where the installed schema lacks it, the
//! daemon must leave the touchpad profile alone.

mod common;

use common::{change, schema_has_key, Daemon};

const MOUSE: &str = "org.gnome.desktop.peripherals.mouse";
const TOUCHPAD: &str = "org.gnome.desktop.peripherals.touchpad";
const TOUCHPAD_PROFILE: &str = "input type:touchpad accel_profile";

fn touchpad_has_accel_profile() -> bool {
    schema_has_key(TOUCHPAD, "accel-profile")
}

fn assert_no_touchpad_profile(commands: &[String]) {
    assert!(
        !commands.iter().any(|c| c.starts_with(TOUCHPAD_PROFILE)),
        "the touchpad schema has no accel-profile key, yet sway got {commands:?}"
    );
}

#[test]
fn flat_profile_is_applied_at_startup() {
    let (sway, mut daemon) = Daemon::start(
        "accel-startup",
        "[org/gnome/desktop/peripherals/mouse]\n\
         accel-profile='flat'\n\
         [org/gnome/desktop/peripherals/touchpad]\n\
         accel-profile='flat'\n",
    );
    sway.wait_for_startup();
    let commands = sway.commands();
    assert!(
        commands.contains(&"input type:pointer accel_profile flat".to_string()),
        "sway got {commands:?}"
    );
    if touchpad_has_accel_profile() {
        assert!(
            commands.contains(&format!("{TOUCHPAD_PROFILE} flat")),
            "sway got {commands:?}"
        );
    } else {
        assert_no_touchpad_profile(&commands);
    }
    assert!(daemon.is_running());
}

/// "default" is the device's own profile. Nothing to do at startup, so an
/// accel_profile from the user's sway config stays in effect.
#[test]
fn default_profile_leaves_sway_alone_at_startup() {
    let (sway, _daemon) = Daemon::start("accel-default", "");
    sway.wait_for_startup();
    let commands = sway.commands();
    assert!(
        !commands.iter().any(|c| c.contains("accel_profile")),
        "sway got {commands:?}"
    );
}

#[test]
fn mouse_profile_changes_are_applied() {
    assert_profile_changes_are_applied(MOUSE, "pointer");
}

#[test]
fn touchpad_profile_changes_are_applied() {
    if touchpad_has_accel_profile() {
        assert_profile_changes_are_applied(TOUCHPAD, "touchpad");
        return;
    }
    // No key to change: changing the mouse profile must not touch the
    // touchpad either, and the daemon keeps running.
    let (sway, mut daemon) = Daemon::start("accel-change-touchpad-nokey", "");
    sway.wait_for_startup();
    let prefix = "input type:pointer accel_profile";
    assert_eq!(
        change(&sway, &daemon, (MOUSE, "accel-profile", "flat"), prefix).as_deref(),
        Some(format!("{prefix} flat").as_str())
    );
    assert_no_touchpad_profile(&sway.commands());
    assert!(daemon.is_running());
}

fn assert_profile_changes_are_applied(schema: &str, pointer_type: &str) {
    let (sway, daemon) = Daemon::start(&format!("accel-change-{pointer_type}"), "");
    sway.wait_for_startup();
    let prefix = format!("input type:{pointer_type} accel_profile");
    for (value, expected) in [
        ("flat", "flat"),
        ("adaptive", "adaptive"),
        ("flat", "flat"),
        // Back to the device default after a change: libinput's
        // default profile.
        ("default", "adaptive"),
    ] {
        assert_eq!(
            change(&sway, &daemon, (schema, "accel-profile", value), &prefix).as_deref(),
            Some(format!("{prefix} {expected}").as_str()),
            "{schema} accel-profile {value}"
        );
    }
}
