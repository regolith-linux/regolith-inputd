//! GNOME's touchpad `click-method` (Settings > Mouse & Touchpad >
//! Secondary Click) must reach sway's `click_method`.

mod common;

use common::{change, Daemon};

const TOUCHPAD: &str = "org.gnome.desktop.peripherals.touchpad";
const CLICK_METHOD: &str = "input type:touchpad click_method";

fn keyfile(click_method: &str) -> String {
    format!(
        "[org/gnome/desktop/input-sources]\n\
         sources=[('xkb', 'us')]\n\
         [org/gnome/desktop/peripherals/touchpad]\n\
         click-method='{click_method}'\n"
    )
}

#[test]
fn click_method_is_applied_at_startup() {
    // "fingers" is GNOME's default and what Settings writes for "Two
    // Finger Push"; libinput's default for most clickpads is areas.
    let (sway, _daemon) = Daemon::start("click-startup", &keyfile("fingers"));
    sway.wait_for_startup();
    assert!(
        sway.commands()
            .contains(&"input type:touchpad click_method clickfinger".to_string()),
        "sway got {:?}",
        sway.commands()
    );
}

#[test]
fn every_click_method_is_applied_on_change() {
    let (sway, daemon) = Daemon::start("click-change", &keyfile("fingers"));
    sway.wait_for_startup();
    for (value, expected) in [
        ("areas", "input type:touchpad click_method button_areas"),
        ("none", "input type:touchpad click_method none"),
        ("fingers", "input type:touchpad click_method clickfinger"),
    ] {
        assert_eq!(
            change(
                &sway,
                &daemon,
                (TOUCHPAD, "click-method", value),
                CLICK_METHOD
            )
            .as_deref(),
            Some(expected),
            "click-method {value}"
        );
    }
}

/// "default" asks for the touchpad's own default. sway cannot reset a
/// setting, so the daemon restores what sway reported for each touchpad
/// before the daemon first changed it (button_areas in the fake sway).
#[test]
fn default_restores_the_touchpad_default() {
    let (sway, daemon) = Daemon::start("click-default", &keyfile("fingers"));
    sway.wait_for_startup();
    assert_eq!(
        change(
            &sway,
            &daemon,
            (TOUCHPAD, "click-method", "default"),
            "input \""
        )
        .as_deref(),
        Some("input \"1267:12445:ELAN0001:00_04F3:309D_Touchpad\" click_method button_areas")
    );
}

/// With "default" at startup the daemon leaves the touchpad (and any
/// click_method in the user's sway config) alone; it must not turn
/// software buttons off.
#[test]
fn default_at_startup_changes_nothing() {
    let (sway, _daemon) = Daemon::start("click-default-startup", &keyfile("default"));
    sway.wait_for_startup();
    let commands = sway.commands();
    assert!(
        !commands.iter().any(|c| c.contains("click_method")),
        "sway got {commands:?}"
    );
}
