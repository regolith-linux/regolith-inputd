//! Trackballs follow GNOME's trackball settings (acceleration profile,
//! scroll-wheel emulation button and its lock, middle-click emulation), and
//! mice follow the mouse's middle-click emulation.

mod common;

use common::{schema_has_key, Daemon, FakeSway};
use serde_json::{json, Value};

const MOUSE: &str = "org.gnome.desktop.peripherals.mouse";
const TRACKBALL: &str = "org.gnome.desktop.peripherals.trackball";

const BALL: &str = "1149:32792:Kensington_Expert_TrackBall";
const MOUSE_ID: &str = "1133:49271:Logitech_USB_Optical_Mouse";
const STICK: &str = "2:10:TPPS/2_IBM_TrackPoint";
const TOUCHPAD: &str = "1267:12445:ELAN0001:00_04F3:309D_Touchpad";

fn pointer(identifier: &str, name: &str, middle_emulation: bool) -> Value {
    let mut libinput = json!({"send_events": "enabled", "accel_speed": 0.0,
                              "accel_profile": "adaptive", "natural_scroll": "disabled",
                              "left_handed": "disabled", "scroll_method": "none",
                              "scroll_button": 274, "scroll_button_lock": "disabled"});
    if middle_emulation {
        libinput["middle_emulation"] = "disabled".into();
    }
    let ids: Vec<u32> = identifier
        .split(':')
        .take(2)
        .map(|id| id.parse().unwrap())
        .collect();
    json!({"identifier": identifier, "name": name, "vendor": ids[0], "product": ids[1],
           "type": "pointer", "libinput": libinput})
}

fn trackball() -> Value {
    pointer(BALL, "Kensington Expert TrackBall", true)
}

/// A keyboard, a mouse, a pointing stick and a touchpad, all offering
/// middle-click emulation, and a trackball unless `with_trackball` is false.
fn inputs(with_trackball: bool) -> String {
    let mut devices = vec![
        json!({"identifier": "1:1:AT_Translated_Set_2_keyboard",
               "name": "AT Translated Set 2 keyboard", "vendor": 1, "product": 1,
               "type": "keyboard", "xkb_active_layout_name": "English (US)",
               "xkb_layout_names": ["English (US)"], "xkb_active_layout_index": 0,
               "libinput": {"send_events": "enabled"}}),
        pointer(MOUSE_ID, "Logitech USB Optical Mouse", true),
        pointer(STICK, "TPPS/2 IBM TrackPoint", true),
        json!({"identifier": TOUCHPAD, "name": "ELAN0001:00 04F3:309D Touchpad",
               "vendor": 1267, "product": 12445, "type": "touchpad",
               "libinput": {"send_events": "enabled", "tap": "disabled",
                            "middle_emulation": "disabled", "scroll_method": "two_finger",
                            "click_method": "button_areas", "accel_profile": "adaptive"}}),
    ];
    if with_trackball {
        devices.push(trackball());
    }
    Value::from(devices).to_string()
}

fn input(identifier: &str, setting: &str) -> String {
    format!("input \"{identifier}\" {setting}")
}

fn middle_click_keys() -> bool {
    schema_has_key(MOUSE, "middle-click-emulation")
        && schema_has_key(TRACKBALL, "middle-click-emulation")
}

/// Every command sway got for `identifier`.
fn commands_for(sway: &FakeSway, identifier: &str) -> Vec<String> {
    let prefix = format!("input \"{identifier}\" ");
    sway.single_commands()
        .into_iter()
        .filter(|c| c.starts_with(&prefix))
        .collect()
}

const KEYFILE: &str = "[org/gnome/desktop/peripherals/trackball]\n\
                       accel-profile='flat'\n\
                       scroll-wheel-emulation-button=8\n\
                       scroll-wheel-emulation-button-lock=true\n\
                       middle-click-emulation=true\n\
                       [org/gnome/desktop/peripherals/mouse]\n\
                       middle-click-emulation=true\n";

fn assert_trackball_settings(sway: &FakeSway, skip: usize) {
    let mut expected = vec![
        "accel_profile flat",
        "scroll_method on_button_down",
        // X11 button 8, the side button: evdev BTN_SIDE.
        "scroll_button 275",
        "scroll_button_lock enabled",
    ];
    if middle_click_keys() {
        expected.push("middle_emulation enabled");
    }
    for setting in expected {
        assert!(
            sway.wait_for_command(skip, &input(BALL, setting)),
            "{setting}: sway got {:?}",
            sway.commands()
        );
    }
}

#[test]
fn trackball_and_mouse_settings_are_applied_at_startup() {
    let (sway, mut daemon) = Daemon::start_with_inputs("trackball-startup", KEYFILE, &inputs(true));
    sway.wait_for_startup();
    assert_trackball_settings(&sway, 0);
    if middle_click_keys() {
        assert!(sway.wait_for_command(0, &input(MOUSE_ID, "middle_emulation enabled")));
    }
    // The pointing stick and the touchpad keep theirs; the mouse takes
    // nothing of the trackball's.
    assert_eq!(commands_for(&sway, STICK), Vec::<String>::new());
    assert_eq!(commands_for(&sway, TOUCHPAD), Vec::<String>::new());
    let mouse = commands_for(&sway, MOUSE_ID);
    assert!(
        mouse
            .iter()
            .all(|c| c.ends_with("middle_emulation enabled")),
        "{mouse:?}"
    );
    assert!(daemon.is_running());
}

#[test]
fn a_trackball_plugged_in_gets_its_settings() {
    let (sway, mut daemon) = Daemon::start_with_inputs("trackball-added", KEYFILE, &inputs(false));
    sway.wait_for_startup();
    assert_eq!(commands_for(&sway, BALL), Vec::<String>::new());
    let seen = sway.commands().len();
    sway.add_input(trackball());
    assert_trackball_settings(&sway, seen);
    assert!(daemon.is_running());
}

/// "default" is libinput's default, adaptive, once a pointer profile has
/// taken effect; until then the trackball stays as sway has it.
#[test]
fn default_profile_waits_for_a_pointer_profile() {
    let (sway, daemon) = Daemon::start_with_inputs("trackball-default", "", &inputs(true));
    sway.wait_for_startup();
    let profiles: Vec<String> = commands_for(&sway, BALL)
        .into_iter()
        .filter(|c| c.contains("accel_profile"))
        .collect();
    assert_eq!(profiles, Vec::<String>::new());

    let seen = sway.commands().len();
    daemon.gsettings_set(MOUSE, "accel-profile", "flat");
    assert!(sway.wait_for_command(seen, "input type:pointer accel_profile flat"));
    assert!(sway.wait_for_command(seen, &input(BALL, "accel_profile adaptive")));

    let seen = sway.commands().len();
    daemon.gsettings_set(TRACKBALL, "accel-profile", "flat");
    assert!(sway.wait_for_command(seen, &input(BALL, "accel_profile flat")));
    // Never written back, to either schema.
    assert_eq!(daemon.gsettings_get(MOUSE, "accel-profile"), "'flat'");
    assert_eq!(daemon.gsettings_get(TRACKBALL, "accel-profile"), "'flat'");
}

#[test]
fn trackball_setting_changes_are_applied() {
    let (sway, daemon) = Daemon::start_with_inputs("trackball-change", "", &inputs(true));
    sway.wait_for_startup();
    for (key, value, expected) in [
        (
            "scroll-wheel-emulation-button",
            "2",
            vec!["scroll_method on_button_down", "scroll_button 274"],
        ),
        (
            "scroll-wheel-emulation-button",
            "3",
            vec!["scroll_button 273"],
        ),
        (
            "scroll-wheel-emulation-button",
            "9",
            vec!["scroll_button 276"],
        ),
        (
            "scroll-wheel-emulation-button-lock",
            "true",
            vec!["scroll_button_lock enabled"],
        ),
        (
            "scroll-wheel-emulation-button",
            "0",
            vec!["scroll_method none", "scroll_button_lock enabled"],
        ),
        ("accel-profile", "adaptive", vec!["accel_profile adaptive"]),
    ] {
        let seen = sway.commands().len();
        daemon.gsettings_set(TRACKBALL, key, value);
        for setting in expected {
            assert!(
                sway.wait_for_command(seen, &input(BALL, setting)),
                "{key}={value}: expected {setting}, sway got {:?}",
                &sway.commands()[seen..]
            );
        }
    }
    // The mouse got its own middle-click emulation at startup, and none of
    // the trackball's settings.
    let mouse = commands_for(&sway, MOUSE_ID);
    assert!(
        mouse
            .iter()
            .all(|c| c.ends_with(" middle_emulation disabled")),
        "{mouse:?}"
    );
}

#[test]
fn middle_click_emulation_reaches_mice_and_trackballs_apart() {
    if !middle_click_keys() {
        return;
    }
    let (sway, daemon) = Daemon::start_with_inputs("trackball-middle", "", &inputs(true));
    sway.wait_for_startup();

    let seen = sway.commands().len();
    daemon.gsettings_set(MOUSE, "middle-click-emulation", "true");
    assert!(sway.wait_for_command(seen, &input(MOUSE_ID, "middle_emulation enabled")));
    let seen_mouse = sway.commands()[seen..].to_vec();

    let seen = sway.commands().len();
    daemon.gsettings_set(TRACKBALL, "middle-click-emulation", "true");
    assert!(sway.wait_for_command(seen, &input(BALL, "middle_emulation enabled")));
    let seen_ball = sway.commands()[seen..].to_vec();

    for (list, other) in [(seen_mouse, BALL), (seen_ball, MOUSE_ID)] {
        assert!(
            !list
                .iter()
                .any(|c| c.contains(other) || c.contains(STICK) || c.contains(TOUCHPAD)),
            "{list:?}"
        );
    }
}
