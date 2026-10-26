//! GNOME's Typing accessibility settings (sticky, slow, bounce and mouse
//! keys) must reach sway's keyboard accessibility options, when sway has
//! them.

mod common;

use common::{Daemon, FakeSway};
use std::thread;
use std::time::{Duration, Instant};

const A11Y: &str = "org.gnome.desktop.a11y.keyboard";

/// A sway with the keyboard accessibility options reports them on every
/// keyboard.
const A11Y_INPUTS: &str = r#"[
  {"identifier": "1:1:AT_Translated_Set_2_keyboard", "name": "AT Translated Set 2 keyboard",
   "vendor": 1, "product": 1, "type": "keyboard",
   "xkb_active_layout_name": "English (US)", "xkb_layout_names": ["English (US)"],
   "xkb_active_layout_index": 0, "sticky_keys": false, "sticky_keys_two_key_off": true,
   "slow_keys": 0, "bounce_keys": 0, "mouse_keys": false, "mouse_keys_init_delay": 300,
   "mouse_keys_accel_time": 300, "mouse_keys_max_speed": 1000,
   "libinput": {"send_events": "enabled"}}
]"#;

const OPTIONS: [&str; 8] = [
    "sticky_keys",
    "sticky_keys_two_key_off",
    "slow_keys",
    "bounce_keys",
    "mouse_keys",
    "mouse_keys_init_delay",
    "mouse_keys_accel_time",
    "mouse_keys_max_speed",
];

fn a11y_commands(commands: &[String]) -> Vec<String> {
    commands
        .iter()
        .filter(|c| {
            OPTIONS
                .iter()
                .any(|o| c.starts_with(&format!("input type:keyboard {o} ")))
        })
        .cloned()
        .collect()
}

const ALL_ON: &str = "[org/gnome/desktop/a11y/keyboard]\n\
    stickykeys-enable=true\n\
    stickykeys-two-key-off=true\n\
    slowkeys-enable=true\n\
    slowkeys-delay=250\n\
    bouncekeys-enable=true\n\
    bouncekeys-delay=400\n\
    mousekeys-enable=true\n\
    mousekeys-max-speed=500\n\
    mousekeys-accel-time=1000\n\
    mousekeys-init-delay=100\n";

#[test]
fn typing_settings_are_applied_at_startup() {
    let (sway, _daemon) = Daemon::start_with_inputs("a11y-startup", ALL_ON, A11Y_INPUTS);
    sway.wait_for_startup();
    assert_eq!(
        a11y_commands(&sway.commands()),
        [
            "input type:keyboard sticky_keys_two_key_off enabled",
            "input type:keyboard sticky_keys enabled",
            "input type:keyboard slow_keys 250",
            "input type:keyboard bounce_keys 400",
            "input type:keyboard mouse_keys_max_speed 500",
            "input type:keyboard mouse_keys_accel_time 1000",
            "input type:keyboard mouse_keys_init_delay 100",
            "input type:keyboard mouse_keys enabled",
        ]
    );
}

/// With the features off in GNOME (the default) the daemon does not touch
/// sway's options, so `input` lines in the user's sway config keep working.
#[test]
fn defaults_leave_sway_config_alone() {
    let (sway, _daemon) = Daemon::start_with_inputs(
        "a11y-default",
        "[org/gnome/desktop/a11y/keyboard]\nslowkeys-delay=500\n",
        A11Y_INPUTS,
    );
    sway.wait_for_startup();
    let commands = sway.commands();
    assert!(a11y_commands(&commands).is_empty(), "sway got {commands:?}");
}

/// Change a key and return the accessibility commands sway gets for it,
/// once `count` of them have arrived. With `count` 0, wait until the daemon
/// has handled a later change of another key instead.
fn change_a11y(
    sway: &FakeSway,
    daemon: &Daemon,
    key: &str,
    value: &str,
    count: usize,
) -> Vec<String> {
    let seen = sway.commands().len();
    daemon.gsettings_set(A11Y, key, value);
    if count == 0 {
        let fence = sway.commands().len();
        let size = 20 + fence % 50;
        daemon.gsettings_set(
            "org.gnome.desktop.interface",
            "cursor-size",
            &size.to_string(),
        );
        assert!(sway.wait_for(fence, "seat * xcursor_theme").is_some());
    } else {
        let deadline = Instant::now() + Duration::from_secs(10);
        while a11y_commands(&sway.commands()[seen..]).len() < count && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
    }
    // Anything sent after the expected commands shows up too.
    thread::sleep(Duration::from_millis(400));
    a11y_commands(&sway.commands()[seen..])
}

#[test]
fn typing_settings_follow_changes() {
    let (sway, daemon) = Daemon::start_with_inputs(
        "a11y-change",
        "[org/gnome/desktop/a11y/keyboard]\n",
        A11Y_INPUTS,
    );
    sway.wait_for_startup();
    let steps: &[(&str, &str, &[&str])] = &[
        (
            "stickykeys-enable",
            "true",
            &[
                "input type:keyboard sticky_keys_two_key_off disabled",
                "input type:keyboard sticky_keys enabled",
            ],
        ),
        (
            "stickykeys-two-key-off",
            "true",
            &[
                "input type:keyboard sticky_keys_two_key_off enabled",
                "input type:keyboard sticky_keys enabled",
            ],
        ),
        (
            "stickykeys-enable",
            "false",
            &["input type:keyboard sticky_keys disabled"],
        ),
        (
            "slowkeys-enable",
            "true",
            &["input type:keyboard slow_keys 300"],
        ),
        (
            "slowkeys-delay",
            "600",
            &["input type:keyboard slow_keys 600"],
        ),
        (
            "slowkeys-enable",
            "false",
            &["input type:keyboard slow_keys 0"],
        ),
        (
            "bouncekeys-enable",
            "true",
            &["input type:keyboard bounce_keys 300"],
        ),
        (
            "bouncekeys-delay",
            "150",
            &["input type:keyboard bounce_keys 150"],
        ),
        (
            "bouncekeys-enable",
            "false",
            &["input type:keyboard bounce_keys 0"],
        ),
        (
            "mousekeys-enable",
            "true",
            &[
                "input type:keyboard mouse_keys_max_speed 10",
                "input type:keyboard mouse_keys_accel_time 300",
                "input type:keyboard mouse_keys_init_delay 300",
                "input type:keyboard mouse_keys enabled",
            ],
        ),
        (
            "mousekeys-max-speed",
            "800",
            &[
                "input type:keyboard mouse_keys_max_speed 800",
                "input type:keyboard mouse_keys_accel_time 300",
                "input type:keyboard mouse_keys_init_delay 300",
                "input type:keyboard mouse_keys enabled",
            ],
        ),
        (
            "mousekeys-enable",
            "false",
            &["input type:keyboard mouse_keys disabled"],
        ),
        // Parameters of a feature that is off are not sent.
        ("mousekeys-init-delay", "50", &[]),
        ("slowkeys-delay", "200", &[]),
    ];
    for (key, value, expected) in steps {
        assert_eq!(
            change_a11y(&sway, &daemon, key, value, expected.len()),
            *expected,
            "{key} {value}"
        );
    }
}

#[test]
fn typing_settings_are_reapplied_after_reload() {
    let (sway, _daemon) = Daemon::start_with_inputs(
        "a11y-reload",
        "[org/gnome/desktop/a11y/keyboard]\nstickykeys-enable=true\nbouncekeys-enable=true\n",
        A11Y_INPUTS,
    );
    sway.wait_for_startup();
    let seen = sway.commands().len();
    // A reload drops the options set at run time; sway's config then
    // reports the reload as done.
    sway.send_tick(r#"{"status": "reload_done"}"#);
    assert_eq!(
        sway.wait_for(seen, "input type:keyboard sticky_keys ")
            .as_deref(),
        Some("input type:keyboard sticky_keys enabled")
    );
    assert_eq!(
        sway.wait_for(seen, "input type:keyboard bounce_keys ")
            .as_deref(),
        Some("input type:keyboard bounce_keys 300")
    );
}

/// A sway without the options rejects them as unknown commands: the daemon
/// sends none and keeps running.
#[test]
fn older_sway_gets_no_accessibility_commands() {
    let (sway, mut daemon) = Daemon::start("a11y-old-sway", ALL_ON);
    sway.wait_for_startup();
    daemon.gsettings_set(A11Y, "slowkeys-delay", "700");
    daemon.gsettings_set(A11Y, "stickykeys-enable", "false");
    // Wait for a command sent after the changes, then for any stragglers.
    let seen = sway.commands().len();
    daemon.gsettings_set("org.gnome.desktop.interface", "cursor-size", "40");
    assert!(sway.wait_for(seen, "seat * xcursor_theme").is_some());
    thread::sleep(Duration::from_millis(300));
    let commands = sway.commands();
    assert!(a11y_commands(&commands).is_empty(), "sway got {commands:?}");
    assert!(daemon.is_running());
}
