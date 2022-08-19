use gio::{prelude::ApplicationExtManual, Application, ApplicationFlags};
use regolith_inputd::SettingsManager;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let app = Application::new(None, ApplicationFlags::IS_SERVICE);
    let mut manager = SettingsManager::new().await;
    manager.start_monitoring();
    let exit_code = app.run() as u8;
    ExitCode::from(exit_code)
}
