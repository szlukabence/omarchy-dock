//! Events crossing from worker threads into the GTK main context.
//!
//! GTK4 widgets are main-thread-only, so "separate threads" has to mean
//! message passing. Every background source — the file watcher now, Hyprland
//! IPC and D-Bus later — funnels into one `async_channel`, which the GTK side
//! drains inside `glib::spawn_future_local`. Nothing else touches widgets.

use crate::hypr::events::HyprEvent;
use crate::hypr::model::Client;

#[derive(Debug, Clone)]
pub enum AppEvent {
    /// `config.toml` changed. Geometry may differ, so the dock is rebuilt.
    ConfigChanged,
    /// Active Omarchy theme or the user's `style.css` changed. Restyle only,
    /// which is far cheaper than a rebuild and keeps hover state intact.
    StyleChanged,
    /// A full window list, sent at startup and after any reconnect.
    HyprSnapshot(Vec<Client>),
    /// An incremental Hyprland event.
    Hypr(HyprEvent),
}

pub type Sender = async_channel::Sender<AppEvent>;
pub type Receiver = async_channel::Receiver<AppEvent>;

pub fn channel() -> (Sender, Receiver) {
    // Unbounded: the producers are low-rate and must never block a watcher
    // thread, and a debounced watcher already collapses bursts.
    async_channel::unbounded()
}
