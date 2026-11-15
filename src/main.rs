use gio::{prelude::ApplicationExtManual, traits::ApplicationExt, Application, ApplicationFlags};
use log::{error, LevelFilter};
use regolith_inputd::{layout_bindings, SettingsManager};

fn main() {
    // Without RUST_LOG, errors are logged, and the warnings of the layout
    // bindings: the reasons shortcuts may not follow the layout.
    let mut logger = pretty_env_logger::formatted_builder();
    match std::env::var("RUST_LOG") {
        Ok(filters) => logger.parse_filters(&filters),
        Err(_) => logger
            .filter_level(LevelFilter::Error)
            .filter_module("regolith_inputd::layout_bindings", LevelFilter::Warn),
    };
    logger.init();
    let app = Application::new(Some("org.regolith.inputd"), ApplicationFlags::IS_SERVICE);
    if layout_bindings::x11_session() {
        // Under X11 the daemon keeps i3's shortcuts on the layout in use, and
        // ends with i3.
        let (tx, rx) = glib::MainContext::channel::<()>(glib::PRIORITY_DEFAULT);
        let quit = app.clone();
        rx.attach(None, move |()| {
            quit.quit();
            glib::Continue(false)
        });
        layout_bindings::i3::start(move || {
            let _ = tx.send(());
        });
        app.hold();
        app.run();
        return;
    }
    let mut manager = SettingsManager::new();
    if let Err(e) = manager.start_monitoring() {
        error!("{e}");
        panic!();
    }
    regolith_inputd::source_switching::start();
    layout_bindings::sway::start();
    app.hold();
    app.run();
}
