//! GNOME's XKB options (Settings > Keyboard: Compose Key, Alternate
//! Characters Key, and any option set with gsettings) must reach sway.

mod common;

use common::{change, Daemon};

const SOURCES: &str = "org.gnome.desktop.input-sources";
const XKB_OPTIONS: &str = "input type:keyboard xkb_options";

#[test]
fn xkb_options_are_applied_at_startup() {
    let (sway, _daemon) = Daemon::start(
        "xkb-startup",
        "[org/gnome/desktop/input-sources]\n\
         sources=[('xkb', 'us')]\n\
         xkb-options=['compose:ralt', 'lv3:menu_switch']\n",
    );
    sway.wait_for_startup();
    // Quoted: sway splits an unquoted command at the comma and runs
    // "lv3:menu_switch" as a separate, invalid command.
    assert!(
        sway.commands().contains(
            &"input type:keyboard xkb_options 'compose:ralt,lv3:menu_switch'".to_string()
        ),
        "sway got {:?}",
        sway.commands()
    );
}

#[test]
fn xkb_options_are_applied_without_input_sources() {
    let (sway, _daemon) = Daemon::start(
        "xkb-no-sources",
        "[org/gnome/desktop/input-sources]\nxkb-options=['compose:ralt']\n",
    );
    sway.wait_for_startup();
    assert!(
        sway.commands()
            .contains(&"input type:keyboard xkb_options 'compose:ralt'".to_string()),
        "sway got {:?}",
        sway.commands()
    );
}

#[test]
fn xkb_options_follow_changes() {
    let (sway, daemon) = Daemon::start(
        "xkb-change",
        "[org/gnome/desktop/input-sources]\nsources=[('xkb', 'us')]\n",
    );
    sway.wait_for_startup();
    for (value, expected) in [
        (
            "['compose:menu']",
            "input type:keyboard xkb_options 'compose:menu'",
        ),
        (
            "['compose:menu', 'lv3:ralt_switch']",
            "input type:keyboard xkb_options 'compose:menu,lv3:ralt_switch'",
        ),
        // Removing the options in Settings removes them in sway.
        ("[]", "input type:keyboard xkb_options ''"),
    ] {
        assert_eq!(
            change(&sway, &daemon, (SOURCES, "xkb-options", value), XKB_OPTIONS).as_deref(),
            Some(expected),
            "xkb-options {value}"
        );
    }
}

/// With no XKB options in GNOME (the default) the daemon does not touch
/// sway's, so options set in the user's sway config keep working.
#[test]
fn no_xkb_options_leave_sway_config_alone() {
    let (sway, _daemon) = Daemon::start(
        "xkb-default",
        "[org/gnome/desktop/input-sources]\nsources=[('xkb', 'us')]\n",
    );
    sway.wait_for_startup();
    let commands = sway.commands();
    assert!(
        !commands.iter().any(|c| c.contains("xkb_options")),
        "sway got {commands:?}"
    );
}
