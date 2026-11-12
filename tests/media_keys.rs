//! The Sound and Media and System keys chosen in Settings > Keyboard >
//! Shortcuts must be bound in sway, as gnome-settings-daemon grabs them in the
//! X11 session.

mod common;

use common::{change, Daemon};
use std::process::Command;

const MEDIA_KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";

/// The shortcuts handler needs gnome-settings-daemon's schemas.
fn have_media_keys_schema() -> bool {
    let output = Command::new("gsettings")
        .arg("list-schemas")
        .output()
        .unwrap();
    let found = String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|schema| schema == MEDIA_KEYS);
    if !found {
        eprintln!("skipping: {MEDIA_KEYS} is not installed");
    }
    found
}

/// Whether the installed media-keys schema has `key`: restart and power off
/// (`reboot`, `shutdown`) are newer than gnome-settings-daemon 46.
fn schema_has_key(key: &str) -> bool {
    let output = Command::new("gsettings")
        .args(["list-keys", MEDIA_KEYS])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|k| k == key)
}

#[test]
fn media_and_system_keys_are_bound_at_startup() {
    if !have_media_keys_schema() {
        return;
    }
    let (sway, _daemon) = Daemon::start(
        "media-keys-startup",
        "[org/gnome/settings-daemon/plugins/media-keys]\n\
         volume-up=['<Super>F12']\n\
         volume-mute=['<Super>F10']\n\
         mic-mute=['<Super>F9']\n\
         play=['<Super>p']\n\
         next=['<Super><Shift>n']\n\
         reboot=['<Control><Alt>r']\n\
         shutdown=['<Control><Alt>End']\n\
         email=['<Super>e']\n\
         eject=['<Super>j']\n",
    );
    sway.wait_for_startup();
    let commands = sway.commands();
    for (key, expected) in [
        (
            "volume-up",
            "bindsym --locked Mod4+F12 exec volumectl -u + $wm.media.volume.step",
        ),
        ("volume-mute", "bindsym --locked Mod4+F10 exec volumectl %"),
        ("mic-mute", "bindsym --locked Mod4+F9 exec volumectl -m %"),
        ("play", "bindsym --locked Mod4+p exec playerctl play-pause"),
        ("next", "bindsym --locked Mod4+Shift+n exec playerctl next"),
        (
            "reboot",
            r"bindsym Control+Mod1+r exec swaymsg exec \$sway.prompt.reboot",
        ),
        (
            "shutdown",
            r"bindsym Control+Mod1+End exec swaymsg exec \$sway.prompt.shutdown",
        ),
        (
            "email",
            "bindsym Mod4+e exec gtk-launch $(xdg-settings get default-url-scheme-handler mailto)",
        ),
        (
            "eject",
            "bindsym --locked Mod4+j exec regolith-sway-gsd-keys eject",
        ),
    ] {
        if !schema_has_key(key) {
            eprintln!("{MEDIA_KEYS} has no {key}, not checking it");
            continue;
        }
        assert!(
            commands.contains(&expected.to_string()),
            "missing {expected:?}; sway got {commands:?}"
        );
    }
}

#[test]
fn rebinding_and_clearing_a_media_key_follow_settings() {
    if !have_media_keys_schema() {
        return;
    }
    let (sway, daemon) = Daemon::start(
        "media-keys-change",
        "[org/gnome/settings-daemon/plugins/media-keys]\nvolume-down=['<Super>F11']\n",
    );
    sway.wait_for_startup();
    // A new key: the old one is unbound (with the same --locked flag, or sway
    // wouldn't find it) and the new one bound.
    let seen = sway.commands().len();
    daemon.gsettings_set(MEDIA_KEYS, "volume-down", "['<Super>minus']");
    assert_eq!(
        sway.wait_for(seen, "unbindsym").as_deref(),
        Some("unbindsym --locked Mod4+F11")
    );
    assert_eq!(
        sway.wait_for(seen, "bindsym").as_deref(),
        Some("bindsym --locked Mod4+minus exec volumectl -u - $wm.media.volume.step")
    );
    // Cleared in Settings: unbound.
    assert_eq!(
        change(
            &sway,
            &daemon,
            (MEDIA_KEYS, "volume-down", "['']"),
            "unbindsym"
        )
        .as_deref(),
        Some("unbindsym --locked Mod4+minus")
    );
}

/// Eject runs the session's eject helper, as the static XF86Eject binding in
/// regolith-wm-config does, and follows Settings like the other keys.
#[test]
fn rebinding_and_clearing_eject_follow_settings() {
    if !have_media_keys_schema() {
        return;
    }
    let (sway, daemon) = Daemon::start(
        "media-keys-eject",
        "[org/gnome/settings-daemon/plugins/media-keys]\neject=['<Super>j']\n",
    );
    sway.wait_for_startup();
    let commands = sway.commands();
    assert!(
        commands.contains(&"bindsym --locked Mod4+j exec regolith-sway-gsd-keys eject".to_string()),
        "sway got {commands:?}"
    );
    let seen = sway.commands().len();
    daemon.gsettings_set(MEDIA_KEYS, "eject", "['<Super><Shift>j']");
    assert_eq!(
        sway.wait_for(seen, "unbindsym").as_deref(),
        Some("unbindsym --locked Mod4+j")
    );
    assert_eq!(
        sway.wait_for(seen, "bindsym").as_deref(),
        Some("bindsym --locked Mod4+Shift+j exec regolith-sway-gsd-keys eject")
    );
    assert_eq!(
        change(&sway, &daemon, (MEDIA_KEYS, "eject", "['']"), "unbindsym").as_deref(),
        Some("unbindsym --locked Mod4+Shift+j")
    );
}

/// The schema's defaults bind only GNOME's Super+F1 for Help among the newly
/// mirrored keys; the other Sound and Media and System keys are empty.
#[test]
fn defaults_bind_only_help() {
    if !have_media_keys_schema() {
        return;
    }
    let (sway, _daemon) = Daemon::start("media-keys-defaults", "");
    sway.wait_for_startup();
    let commands = sway.commands();
    for unexpected in [
        "volumectl",
        "playerctl",
        "sway.prompt.reboot",
        "sway.prompt.shutdown",
        "regolith-sway-gsd-keys",
    ] {
        assert!(
            !commands.iter().any(|c| c.contains(unexpected)),
            "{unexpected} bound by default; sway got {commands:?}"
        );
    }
    assert!(
        commands.contains(
            &"bindsym Mod4+F1 exec gtk-launch $(xdg-settings get default-url-scheme-handler help)"
                .to_string()
        ),
        "sway got {commands:?}"
    );
}
