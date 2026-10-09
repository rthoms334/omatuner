//! `tuner` binary: CLI → paths/config/logging → single-instance handoff → services → window.
//! Owner: WP-10b.
//!
//! Startup budget (first frame < 300 ms): font resolution (`fc-match`/`fc-list`, ~145 ms) +
//! font file reads and theme loading (`hyprctl`) run on helper threads in parallel with
//! `Services::start` (mpv init, store open/migrate) and are joined in the eframe creation
//! callback.

use std::sync::Arc;

use clap::Parser;
use eframe::egui;

use tuner::cli::Cli;
use tuner::config::Config;
use tuner::fonts::{FontSet, LoadedFonts};
use tuner::ipc::IpcRequest;
use tuner::msg::EventBus;
use tuner::paths::AppPaths;
use tuner::services::Services;
use tuner::theme::Theme;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let paths = match &cli.profile_dir {
        Some(dir) => AppPaths::rooted(dir),
        None => AppPaths::from_env()?,
    };
    let (config, config_err) = match Config::load(&paths.config_file()) {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e)),
    };
    let logs = tuner::logging::init(&paths, cli.debug || config.general.debug_log);
    if let Some(e) = config_err {
        tracing::error!("{e}; using defaults (your file was not modified)");
    }

    let request = IpcRequest { view: cli.view, play: cli.play.clone(), inputs: cli.inputs.clone() };
    if tuner::ipc::try_handoff(&paths.ipc_socket(), &request)? {
        return Ok(());
    }
    // SKELETON-STUB(WP-06): `--refresh` runs RefreshStale headlessly and exits.

    let (bus, events) = EventBus::new(None);
    let ui_scale = config.interface.ui_scale;
    let follow_theme = config.integration.follow_omarchy_theme;
    let font_cfg = config.fonts.clone();
    let fonts_thread = std::thread::Builder::new().name("fonts".into()).spawn(move || FontSet::resolve(&font_cfg).load())?;
    let theme_paths = paths.clone();
    let theme_thread = std::thread::Builder::new()
        .name("theme".into())
        .spawn(move || if follow_theme { Theme::load(&theme_paths, ui_scale) } else { Theme::fallback() })?;
    let services = Services::start(paths.clone(), config, logs, bus.clone())?;

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title(tuner::APP_NAME)
            .with_app_id(tuner::APP_ID)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([360.0, 240.0]),
        ..Default::default()
    };
    eframe::run_native(
        tuner::APP_NAME,
        options,
        Box::new(move |cc| {
            // The bus was created before the window; attaching the context here makes every
            // clone (player, jobs, store writer) wake the UI when it sends.
            bus.attach_egui(cc.egui_ctx.clone());
            let fonts: LoadedFonts = fonts_thread.join().unwrap_or_default();
            let theme = Arc::new(theme_thread.join().unwrap_or_else(|_| Theme::fallback()));
            tracing::info!("theme '{}' ({:?})", theme.name, theme.palette.mode);
            Ok(Box::new(tuner::app::TunerApp::new(cc, services, events, theme, fonts, request)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("window: {e}"))?;
    Ok(())
}
