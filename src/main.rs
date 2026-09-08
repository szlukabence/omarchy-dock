//! omarchy-dock — a fast, modern dock for Omarchy (Hyprland / Arch).

mod anim;
mod app;
mod autohide;
mod config;
mod event;
mod desktop;
mod hypr;
mod ipc_ctl;
mod omarchy;
mod runtime;
mod stacks;
mod state;
mod theme;
mod ui;

use gtk4 as gtk;
use gtk::glib;

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "omarchy_dock=info".into()),
        )
        .init();

    app::run()
}
