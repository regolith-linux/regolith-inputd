use gio::{prelude::ApplicationExtManual, traits::ApplicationExt, Application, ApplicationFlags};
use regolith_inputd::SettingsManager;

fn main() {
    pretty_env_logger::init();
    let app = Application::new(Some("org.regolith.inputd"), ApplicationFlags::IS_SERVICE);
    let mut manager = SettingsManager::new();
    // sway need not be running yet: the settings are applied, and the input
    // sources followed, whenever one is reachable.
    manager.start_monitoring(regolith_inputd::source_switching::start());
    app.hold();
    app.run();
}
