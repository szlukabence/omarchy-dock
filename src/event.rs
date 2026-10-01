//! Events crossing from worker threads into the GTK main context.
//!
//! GTK4 widgets are main-thread-only, so "separate threads" has to mean
//! message passing. Every background source — the file watcher now, Hyprland
//! IPC and D-Bus later — funnels into one `async_channel`, which the GTK side
//! drains inside `glib::spawn_future_local`. Nothing else touches widgets.

use crate::hypr::events::HyprEvent;
use crate::hypr::model::{Client, Monitor, Workspace};

#[derive(Debug, Clone)]
pub enum AppEvent {
    /// `config.toml` changed. Geometry may differ, so the dock is rebuilt.
    ConfigChanged,
    /// A `.desktop` file was added, changed or removed, so an application was
    /// installed or uninstalled.
    DesktopEntriesChanged,
    /// Something outside the config changed what the dock's hide policy should
    /// decide — a screen recording starting or stopping, say.
    HidePolicyChanged,
    /// Active Omarchy theme or the user's `style.css` changed. Restyle only,
    /// which is far cheaper than a rebuild and keeps hover state intact.
    StyleChanged,
    /// Full window and monitor state, sent at startup and after any reconnect.
    /// `focused` is `None` when Hyprland has no focused window at all.
    HyprSnapshot {
        clients: Vec<Client>,
        monitors: Vec<Monitor>,
        workspaces: Vec<Workspace>,
        focused: Option<crate::hypr::Address>,
    },
    /// An incremental Hyprland event.
    Hypr(HyprEvent),
    /// The current set of media players. Sent whole on every change.
    Media(Vec<crate::media::Player>),
    /// How many downloads are in progress in the Downloads folder, sent when
    /// that number changes.
    Downloads(usize),
    /// A notification was sent, by whom. For unread badges.
    Notified(crate::notices::Notice),
    /// The current set of system-tray items. Sent whole on every change.
    Tray(Vec<crate::tray::TrayItem>),
    /// The removable drives plugged in right now. Sent whole on every change.
    Drives(Vec<crate::drives::Drive>),
    /// A command from `omarchy-dockctl`.
    Control(crate::ipc_ctl::Control),
    /// The dock was asked to stop (SIGTERM, SIGINT or SIGHUP).
    Quit,
    /// A burst of focus events that landed on a minimized window is over.
    FocusSettled,
}

pub type Sender = async_channel::Sender<AppEvent>;
pub type Receiver = async_channel::Receiver<AppEvent>;

pub fn channel() -> (Sender, Receiver) {
    // Unbounded: the producers are low-rate and must never block a watcher
    // thread, and a debounced watcher already collapses bursts.
    async_channel::unbounded()
}
