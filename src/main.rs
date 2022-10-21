use gio::{prelude::ApplicationExtManual, traits::ApplicationExt, Application, ApplicationFlags};
use log::error;
use regolith_inputd::SettingsManager;

fn main() {
    pretty_env_logger::init();
    let app = Application::new(Some("org.regolith.inputd"), ApplicationFlags::IS_SERVICE);
    let mut manager = SettingsManager::new();
    if let Err(e) = manager.start_monitoring() {
        error!("{e}");
        panic!();
    }
    app.hold();
    app.run();
}
