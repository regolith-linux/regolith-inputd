//! Touchpads follow the mouse's handedness when their left-handed setting
//! is "mouse", GNOME's default.

mod common;

use common::{change, Daemon};
use std::thread;
use std::time::Duration;

const MOUSE: &str = "org.gnome.desktop.peripherals.mouse";
const TOUCHPAD_LEFT_HANDED: &str = "input type:touchpad left_handed";

fn keyfile(touchpad_left_handed: &str) -> String {
    format!(
        "[org/gnome/desktop/input-sources]\n\
         sources=[('xkb', 'us')]\n\
         [org/gnome/desktop/peripherals/touchpad]\n\
         left-handed='{touchpad_left_handed}'\n"
    )
}

#[test]
fn touchpad_following_the_mouse_takes_its_changes() {
    let (sway, daemon) = Daemon::start("left-handed-mouse", &keyfile("mouse"));
    sway.wait_for_startup();
    for (value, expected) in [
        ("true", "input type:touchpad left_handed enabled"),
        ("false", "input type:touchpad left_handed disabled"),
    ] {
        assert_eq!(
            change(
                &sway,
                &daemon,
                (MOUSE, "left-handed", value),
                TOUCHPAD_LEFT_HANDED
            )
            .as_deref(),
            Some(expected),
            "mouse left-handed {value}"
        );
    }
}

#[test]
fn touchpad_with_its_own_handedness_ignores_the_mouse() {
    let (sway, daemon) = Daemon::start("left-handed-right", &keyfile("right"));
    sway.wait_for_startup();
    let seen = sway.commands().len();
    assert_eq!(
        change(
            &sway,
            &daemon,
            (MOUSE, "left-handed", "true"),
            "input type:pointer left_handed"
        )
        .as_deref(),
        Some("input type:pointer left_handed enabled")
    );
    thread::sleep(Duration::from_millis(300));
    let after: Vec<String> = sway.commands().into_iter().skip(seen).collect();
    assert!(
        !after.iter().any(|c| c.starts_with(TOUCHPAD_LEFT_HANDED)),
        "sway got {after:?}"
    );
}
