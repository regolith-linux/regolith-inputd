use gio::{prelude::ApplicationExtManual, traits::ApplicationExt, Application, ApplicationFlags};
use regolith_inputd::SettingsManager;

fn main() {
    let app = Application::new(Some("org.regolith.inputd"), ApplicationFlags::IS_SERVICE);
    let mut manager = SettingsManager::new();
    manager.start_monitoring();
    app.hold();
    app.run();
}
