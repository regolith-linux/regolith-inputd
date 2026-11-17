//! Touchscreens follow GNOME's `output` setting for their vendor and
//! product: at startup, when plugged in, when the setting changes, when
//! outputs come and go, and after sway reloads its config.

mod common;

use common::Daemon;
use serde_json::{json, Value};

/// The settings of the touchscreens with vendor 255 and product 16.
const ELAN: &str = "org.gnome.desktop.peripherals.touchscreen:\
                    /org/gnome/desktop/peripherals/touchscreens/00ff:0010/";
const ELAN_GROUP: &str = "[org/gnome/desktop/peripherals/touchscreens/00ff:0010]";

const KEYBOARD: &str = r#"
  {"identifier": "1:1:AT_Translated_Set_2_keyboard", "name": "AT Translated Set 2 keyboard",
   "vendor": 1, "product": 1, "type": "keyboard",
   "xkb_active_layout_name": "English (US)", "xkb_layout_names": ["English (US)"],
   "xkb_active_layout_index": 0, "libinput": {"send_events": "enabled"}}"#;

fn touchscreen(identifier: &str, vendor: u32, product: u32) -> Value {
    let name = identifier.splitn(3, ':').nth(2).unwrap().replace('_', " ");
    json!({"identifier": identifier, "name": name, "vendor": vendor, "product": product,
           "type": "touch",
           "libinput": {"send_events": "enabled", "calibration_matrix": [1.0, 0.0, 0.0, 0.0, 1.0, 0.0]}})
}

fn inputs(devices: &[Value]) -> String {
    let devices: Vec<String> = devices.iter().map(Value::to_string).collect();
    format!("[{KEYBOARD}, {}]", devices.join(", ")).replace(", ]", "]")
}

fn output(name: &str, make: &str, model: &str, serial: &str, active: bool) -> Value {
    json!({"name": name, "make": make, "model": model, "serial": serial, "active": active,
           "rect": {"x": 0, "y": 0, "width": if active { 1920 } else { 0 },
                    "height": if active { 1080 } else { 0 }}})
}

/// The built-in panel (an EDID with neither a model name nor a serial
/// string), and two identical external monitors.
fn outputs() -> Value {
    json!([
        output("eDP-1", "BOE", "0x0A1F", "Unknown", true),
        output("DP-1", "Dell Inc.", "DELL U2720Q", "8LXMZ13", true),
        output("DP-2", "Dell Inc.", "DELL U2720Q", "8LXMZ13", true),
    ])
}

fn map(identifier: &str, output: &str) -> String {
    format!("input \"{identifier}\" map_to_output {output}")
}

fn assert_no_mapping(sway: &common::FakeSway) {
    let commands = sway.single_commands();
    assert!(
        !commands.iter().any(|c| c.contains("map_to_output")),
        "sway got {commands:?}"
    );
}

const ELAN_ID: &str = "255:16:ELAN_Touchscreen";

#[test]
fn startup_maps_each_touchscreen_to_the_monitor_its_ids_name() {
    // Two devices with the same ids share one set of settings; the third
    // has GNOME's names for the built-in panel.
    let keyfile = format!(
        "{ELAN_GROUP}\n\
         output=['Dell Inc.', 'DELL U2720Q', '8LXMZ13', 'DP-2']\n\
         [org/gnome/desktop/peripherals/touchscreens/1234:5678]\n\
         output=['BOE', '0x0a1f', '0x00000000']\n"
    );
    let devices = [
        touchscreen(ELAN_ID, 255, 16),
        touchscreen("255:16:ELAN_Touchscreen_Stylus", 255, 16),
        touchscreen("4660:22136:Wacom_Touch", 4660, 22136),
    ];
    let (sway, mut daemon) = Daemon::start_with_devices(
        "touch-startup",
        &keyfile,
        &inputs(&devices),
        &outputs().to_string(),
    );
    sway.wait_for_startup();
    for (identifier, output) in [
        (ELAN_ID, "\"DP-2\""),
        ("255:16:ELAN_Touchscreen_Stylus", "\"DP-2\""),
        ("4660:22136:Wacom_Touch", "\"eDP-1\""),
    ] {
        assert!(
            sway.wait_for_command(0, &map(identifier, output)),
            "{identifier}: sway got {:?}",
            sway.commands()
        );
    }
    assert!(daemon.is_running());
}

/// An automatic setting is sway's own mapping, left alone, until the
/// daemon has mapped the device: then it goes back to the whole desktop.
#[test]
fn automatic_setting_leaves_sway_alone_until_the_daemon_mapped_the_device() {
    let (sway, daemon) = Daemon::start_with_devices(
        "touch-automatic",
        "",
        &inputs(&[touchscreen(ELAN_ID, 255, 16)]),
        &outputs().to_string(),
    );
    sway.wait_for_startup();
    assert_no_mapping(&sway);

    let seen = sway.commands().len();
    daemon.gsettings_set(ELAN, "output", "['Dell Inc.', 'DELL U2720Q', '8LXMZ13']");
    // Identical monitors and no connector: the first in sway's order.
    assert!(sway.wait_for_command(seen, &map(ELAN_ID, "\"DP-1\"")));

    let seen = sway.commands().len();
    daemon.gsettings_set(ELAN, "output", "['', '', '']");
    assert!(sway.wait_for_command(seen, &map(ELAN_ID, "*")));

    let seen = sway.commands().len();
    daemon.gsettings_set(
        ELAN,
        "output",
        "['Dell Inc.', 'DELL U2720Q', '8LXMZ13', 'HDMI-A-1']",
    );
    // Two monitors match and neither is on that connector: nothing chosen.
    assert!(sway.wait_for_command(seen, &map(ELAN_ID, "*")));
    // Read, never written back.
    assert_eq!(
        daemon.gsettings_get(ELAN, "output"),
        "['Dell Inc.', 'DELL U2720Q', '8LXMZ13', 'HDMI-A-1']"
    );
}

#[test]
fn touches_follow_the_monitor_as_it_comes_and_goes() {
    let keyfile = format!("{ELAN_GROUP}\noutput=['DEL', 'DELL U2720Q', '8LXMZ13']\n");
    let only_panel = json!([output("eDP-1", "BOE", "0x0A1F", "Unknown", true)]);
    let (sway, mut daemon) = Daemon::start_with_devices(
        "touch-outputs",
        &keyfile,
        &inputs(&[touchscreen(ELAN_ID, 255, 16)]),
        &only_panel.to_string(),
    );
    sway.wait_for_startup();
    assert!(sway.wait_for_command(0, &map(ELAN_ID, "*")));

    // Sway prints the make hwdata has for DEL; without the monitor's EDID
    // (no /sys/class/drm in the test) only sway's names can match.
    let seen = sway.commands().len();
    daemon.gsettings_set(ELAN, "output", "['Dell Inc.', 'DELL U2720Q', '8LXMZ13']");
    assert!(sway.wait_for_command(seen, &map(ELAN_ID, "*")));

    let seen = sway.commands().len();
    sway.set_outputs(
        &json!([
            output("eDP-1", "BOE", "0x0A1F", "Unknown", true),
            output("DP-1", "Dell Inc.", "DELL U2720Q", "8LXMZ13", true),
        ])
        .to_string(),
    );
    assert!(sway.wait_for_command(seen, &map(ELAN_ID, "\"DP-1\"")));

    // Turned off: sway can't map to it, touches span the desktop again.
    let seen = sway.commands().len();
    sway.set_outputs(
        &json!([
            output("eDP-1", "BOE", "0x0A1F", "Unknown", true),
            output("DP-1", "Dell Inc.", "DELL U2720Q", "8LXMZ13", false),
        ])
        .to_string(),
    );
    assert!(sway.wait_for_command(seen, &map(ELAN_ID, "*")));

    let seen = sway.commands().len();
    sway.set_outputs(&only_panel.to_string());
    assert!(sway.wait_for_command(seen, &map(ELAN_ID, "*")));
    assert!(daemon.is_running());
}

#[test]
fn a_touchscreen_plugged_in_is_mapped() {
    let keyfile = format!("{ELAN_GROUP}\noutput=['BOE', '0x0A1F', 'Unknown']\n");
    let (sway, daemon) = Daemon::start_with_devices(
        "touch-added",
        &keyfile,
        &inputs(&[]),
        &outputs().to_string(),
    );
    sway.wait_for_startup();
    assert_no_mapping(&sway);

    // Events are handled in order: once the second device is mapped, the
    // first (automatic) one has been seen and left alone.
    sway.add_input(touchscreen("4660:22136:Wacom_Touch", 4660, 22136));
    sway.add_input(touchscreen(ELAN_ID, 255, 16));
    assert!(sway.wait_for_command(0, &map(ELAN_ID, "\"eDP-1\"")));
    assert!(!sway
        .single_commands()
        .iter()
        .any(|c| c.contains("Wacom_Touch")));

    // A setting changed for a device plugged in after startup is followed.
    let seen = sway.commands().len();
    daemon.gsettings_set(
        ELAN,
        "output",
        "['Dell Inc.', 'DELL U2720Q', '8LXMZ13', 'DP-2']",
    );
    assert!(sway.wait_for_command(seen, &map(ELAN_ID, "\"DP-2\"")));
}

#[test]
fn reload_maps_touchscreens_again() {
    let keyfile = format!(
        "{ELAN_GROUP}\n\
         output=['Dell Inc.', 'DELL U2720Q', '8LXMZ13', 'DP-2']\n\
         [org/gnome/desktop/peripherals/touchscreens/1234:5678]\n\
         output=['BOE', '0x0a1f', '0x00000000']\n"
    );
    let devices = [
        touchscreen(ELAN_ID, 255, 16),
        touchscreen("4660:22136:Wacom_Touch", 4660, 22136),
    ];
    let (sway, daemon) = Daemon::start_with_devices(
        "touch-reload",
        &keyfile,
        &inputs(&devices),
        &outputs().to_string(),
    );
    sway.wait_for_startup();
    let wacom = map("4660:22136:Wacom_Touch", "\"eDP-1\"");
    assert!(sway.wait_for_command(0, &map(ELAN_ID, "\"DP-2\"")));

    let seen = sway.commands().len();
    sway.send_tick(r#"{"status": "reload_done"}"#);
    assert!(sway.wait_for_command(seen, &map(ELAN_ID, "\"DP-2\"")));
    assert!(sway.wait_for_command(seen, &wacom));

    let seen = sway.commands().len();
    daemon.gsettings_set(ELAN, "output", "@as []");
    assert!(sway.wait_for_command(seen, &map(ELAN_ID, "*")));

    // After a reload sway's automatic mapping is back: an automatic setting
    // leaves it alone while the other touchscreen is mapped again.
    let seen = sway.commands().len();
    sway.send_tick(r#"{"status": "reload_done"}"#);
    assert!(sway.wait_for_command(seen, &wacom));
    let after = &sway.commands()[seen..];
    assert!(
        !after.iter().any(|c| c.contains(ELAN_ID)),
        "sway got {after:?}"
    );
}
