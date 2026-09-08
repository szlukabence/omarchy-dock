//! Events crossing from worker threads into the GTK main context.
//!
//! GTK4 widgets are main-thread-only, so "separate threads" has to mean
//! message passing. Every background source — the file watcher now, Hyprland
//! IPC and D-Bus later — funnels into one `async_channel`, which the GTK side
//! drains inside `glib::spawn_future_local`. Nothing else touches widgets.

use crate::hypr::events::HyprEvent;
use crate::hypr::model::{Client, Monitor};

#[derive(Debug, Clone)]
pub enum AppEvent {
    /// `config.toml` changed. Geometry may differ, so the dock is rebuilt.
    ConfigChanged,
    /// Active Omarchy theme or the user's `style.css` changed. Restyle only,
    /// which is far cheaper than a rebuild and keeps hover state intact.
    StyleChanged,
    /// Full window and monitor state, sent at startup and after any reconnect.
    /// `focused` is `None` when Hyprland has no focused window at all.
    HyprSnapshot {
        clients: Vec<Client>,
        monitors: Vec<Monitor>,
        focused: Option<crate::hypr::Address>,
    },
    /// An incremental Hyprland event.
    Hypr(HyprEvent),
    /// A command from `omarchy-dockctl`.
    Control(crate::ipc_ctl::Control),
}

pub type Sender = async_channel::Sender<AppEvent>;
pub type Receiver = async_channel::Receiver<AppEvent>;

pub fn channel() -> (Sender, Receiver) {
    // Unbounded: the producers are low-rate and must never block a watcher
    // thread, and a debounced watcher already collapses bursts.
    async_channel::unbounded()
}
