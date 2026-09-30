//! The async worker thread.
//!
//! GTK owns the main thread, so everything async — Hyprland IPC now, D-Bus
//! later — lives on a Tokio runtime here and reaches the UI only as messages
//! on the event channel. Nothing in this module may touch a GTK type.

use crate::event::{AppEvent, Sender};
use crate::hypr;

/// How long to wait before servicing a snapshot request, so a burst (opening
/// several windows, or a workspace switch) collapses into one `j/clients`.
const COALESCE: std::time::Duration = std::time::Duration::from_millis(60);

/// Handle used by the UI to ask for a fresh window snapshot.
pub type SnapshotRequest = tokio::sync::mpsc::Sender<()>;

/// An action the UI wants performed. Dispatchers are async and must not run on
/// the GTK thread, so clicks become messages.
#[derive(Debug, Clone)]
pub enum DockCommand {
    /// Focus one specific window.
    Focus(hypr::Address),
    /// Close one specific window.
    Close(hypr::Address),
    /// Run a command line, via Hyprland so it inherits the compositor's
    /// environment rather than the dock's.
    Exec(String),
    /// Show or hide a special (scratchpad) workspace.
    ToggleSpecial(String),
    /// Switch to a workspace by name.
    FocusWorkspace(String),
    /// Send one window to a workspace without following it — what dropping a
    /// dock icon onto a workspace tile means.
    SendToWorkspace { window: hypr::Address, workspace: String },
    /// Park a window on `special:minimized`, tagged with the workspace it is
    /// on so it can go back there.
    /// `group` is its tab group, which Hyprland moves along with it.
    Minimize {
        window: hypr::Address,
        workspace: String,
        stale_home: Option<String>,
        group: Vec<hypr::Address>,
    },
    /// Send a minimized window home, or to the workspace in front when it
    /// has none, and focus it.
    Restore { window: hypr::Address, home: Option<String>, group: Vec<hypr::Address> },
    /// Send a minimized window to a workspace without following it, as
    /// dropping its app on a workspace tile does, and drop its home tag.
    Unpark {
        window: hypr::Address,
        workspace: String,
        home: Option<String>,
        group: Vec<hypr::Address>,
    },
    /// A transport command for one media player, addressed by bus name.
    Media { bus: String, action: crate::media::Action },
    /// Deliver a click to a system-tray item, addressed by its D-Bus service.
    TrayClick { service: String, click: crate::tray::Click },
}

impl DockCommand {
    pub fn minimize(window: &hypr::Address, meta: &crate::state::WindowMeta) -> Self {
        DockCommand::Minimize {
            window: window.clone(),
            workspace: meta.workspace.clone(),
            stale_home: meta.home.clone(),
            group: meta.group.clone(),
        }
    }

    pub fn restore(window: &hypr::Address, meta: &crate::state::WindowMeta) -> Self {
        DockCommand::Restore {
            window: window.clone(),
            home: meta.home.clone(),
            group: meta.group.clone(),
        }
    }

    /// Bring one window to the front: restored if minimized, else focused.
    pub fn open_window(window: &hypr::Address, meta: Option<&crate::state::WindowMeta>) -> Self {
        match meta {
            Some(m) if m.minimized => Self::restore(window, m),
            _ => DockCommand::Focus(window.clone()),
        }
    }

    /// What a left-click on `item` sends, if anything.
    pub fn for_click(item: &crate::state::DockItem) -> Option<Self> {
        use crate::state::Click;
        let meta = |a: &hypr::Address| item.meta_of(a).cloned().unwrap_or_default();
        match item.click() {
            Click::Launch => (!item.exec.is_empty()).then(|| DockCommand::Exec(item.exec.clone())),
            Click::Focus(a) => Some(DockCommand::Focus(a)),
            Click::Minimize(a) => Some(Self::minimize(&a, &meta(&a))),
            Click::Restore(a) => Some(Self::restore(&a, &meta(&a))),
        }
    }
}

pub type CommandSender = tokio::sync::mpsc::Sender<DockCommand>;

/// The handles the UI needs to drive the worker.
#[derive(Clone)]
pub struct Handles {
    pub snapshot: SnapshotRequest,
    pub commands: CommandSender,
    /// Whether notification badges are on, which is whether notifications
    /// are watched at all.
    pub badges: tokio::sync::watch::Sender<bool>,
}

/// Start the worker. The thread owns its runtime and runs until process exit.
pub fn spawn(tx: Sender, tray: bool, media: bool, badges: bool) -> std::io::Result<Handles> {
    // Capacity 1: requests are "please resync", so a queued one is as good as
    // ten. `try_send` failing on a full channel is the desired coalescing.
    let (req_tx, mut req_rx) = tokio::sync::mpsc::channel::<()>(1);
    // Commands are user actions; a small queue absorbs fast clicking without
    // ever dropping one silently.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<DockCommand>(32);
    let (badges_tx, badges_rx) = tokio::sync::watch::channel(badges);

    std::thread::Builder::new().name("omarchy-dock-async".into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(error = %e, "cannot start async runtime");
                return;
            }
        };

        rt.block_on(async move {
            // Prime the UI before streaming deltas, so windows opened before
            // the dock started are still represented.
            snapshot(&tx).await;

            // The event stream runs concurrently with snapshot servicing.
            let ev_tx = tx.clone();
            tokio::spawn(async move {
                let _ = hypr::events::listen(move |event| {
                    // Unbounded channel, so this never blocks the reader.
                    let _ = ev_tx.send_blocking(AppEvent::Hypr(event));
                })
                .await;
            });

            // The control socket is independent of Hyprland; if it cannot
            // bind (usually a second instance) the dock still works, just
            // without hotkeys.
            let ctl_tx = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::ipc_ctl::serve(ctl_tx).await {
                    tracing::warn!(error = %e, "control socket unavailable");
                }
            });

            // The tray host is optional in every sense: the session bus may
            // not be reachable, and no watcher may be running. Neither is a
            // reason for the dock not to start, so it is logged and dropped.
            if media {
                let media_tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::media::serve(media_tx).await {
                        tracing::warn!(error = %e, "media controls unavailable");
                    }
                });
            }

            // Notifications are watched only while badges are on; the
            // setting is live, so the watcher starts and stops with it.
            {
                let notices_tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::notices::serve(notices_tx, badges_rx).await {
                        tracing::warn!(error = %e, "notification badges unavailable");
                    }
                });
            }

            if tray {
                let tray_tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::tray::serve(tray_tx).await {
                        tracing::warn!(error = %e, "system tray unavailable");
                    }
                });
            }

            // Commands run concurrently with snapshot servicing so a slow
            // dispatch never delays a resync, or vice versa.
            tokio::spawn(async move {
                while let Some(cmd) = cmd_rx.recv().await {
                    if let Err(e) = execute(&cmd).await {
                        tracing::warn!(?cmd, error = %e, "command failed");
                    }
                }
            });

            while req_rx.recv().await.is_some() {
                tokio::time::sleep(COALESCE).await;
                // Drop anything that piled up during the wait; one query
                // answers them all.
                while req_rx.try_recv().is_ok() {}
                snapshot(&tx).await;
            }
        });
    })?;

    Ok(Handles { snapshot: req_tx, commands: cmd_tx, badges: badges_tx })
}

async fn execute(cmd: &DockCommand) -> anyhow::Result<()> {
    use hypr::dispatch;
    match cmd {
        DockCommand::Focus(addr) => dispatch::focus_window(addr).await,
        DockCommand::Close(addr) => dispatch::close_window(addr).await,
        DockCommand::Exec(cmd) => dispatch::exec(cmd).await,
        DockCommand::ToggleSpecial(name) => dispatch::toggle_special(name).await,
        DockCommand::FocusWorkspace(name) => dispatch::focus_workspace(name).await,
        DockCommand::Media { bus, action } => {
            let conn = zbus::Connection::session().await?;
            crate::media::command(&conn, bus, *action).await
        }
        DockCommand::TrayClick { service, click } => {
            // A fresh connection per click. Tray clicks are rare and
            // user-driven, and holding a session-bus connection alive purely
            // for them would mean threading it through the command loop.
            let conn = zbus::Connection::session().await?;
            crate::tray::click(&conn, service, *click).await
        }
        DockCommand::SendToWorkspace { window, workspace } => {
            // `follow = false`: the user dropped an icon onto a workspace to
            // put it away, not to go there.
            dispatch::move_window_to_workspace(window, workspace, false).await
        }
        DockCommand::Minimize { window, workspace, stale_home, group } => {
            let steps = hypr::minimize::minimize_steps(workspace, stale_home.as_deref());
            run_steps(window, group, &steps).await
        }
        DockCommand::Restore { window, home, group } => {
            // The workspace in front is only needed without a home, and a
            // window with one must not stay parked because this query failed.
            let current = if home.as_deref().is_some_and(hypr::minimize::is_home) {
                String::new()
            } else {
                hypr::request::monitors()
                    .await?
                    .into_iter()
                    .find(|m| m.focused)
                    .map(|m| m.active_workspace.name)
                    .unwrap_or_else(|| "1".into())
            };
            let steps = hypr::minimize::restore_steps(home.as_deref(), &current);
            run_steps(window, group, &steps).await
        }
        DockCommand::Unpark { window, workspace, home, group } => {
            let steps = hypr::minimize::unpark_steps(home.as_deref(), workspace);
            run_steps(window, group, &steps).await
        }
    }
}

/// Run minimize or restore steps in order, stopping at the first failure so a
/// window is never moved without the tag that says where it belongs. Tags go
/// on the whole tab group, since the move takes the group along.
async fn run_steps(
    window: &hypr::Address,
    group: &[hypr::Address],
    steps: &[hypr::minimize::Step],
) -> anyhow::Result<()> {
    use hypr::{dispatch, minimize::Step};
    for step in steps {
        match step {
            Step::Tag(tag) => {
                for w in std::iter::once(window).chain(group) {
                    dispatch::tag_window(w, tag).await?;
                }
            }
            Step::Move { workspace, follow } => {
                dispatch::move_window_to_workspace(window, workspace, *follow).await?
            }
            Step::Focus => dispatch::focus_window(window).await?,
        }
    }
    Ok(())
}

async fn snapshot(tx: &Sender) {
    // Both queries in flight together: they are independent and the dock
    // needs them consistently, so serialising them only adds latency.
    let (clients, monitors, workspaces, active) = tokio::join!(
        hypr::request::clients(),
        hypr::request::monitors(),
        hypr::request::workspaces(),
        hypr::request::active_window(),
    );

    match clients {
        Ok(clients) => {
            let monitors = monitors.unwrap_or_else(|e| {
                tracing::warn!(error = %e, "monitor query failed");
                Vec::new()
            });
            // `j/activewindow` is authoritative and distinguishes "nothing is
            // focused" from "focus unknown"; focusHistoryID cannot, because it
            // is global and still names a window on another workspace.
            let workspaces = workspaces.unwrap_or_else(|e| {
                tracing::warn!(error = %e, "workspace query failed");
                Vec::new()
            });
            let focused = active.ok().flatten().map(|c| c.address);
            let _ = tx
                .send(AppEvent::HyprSnapshot { clients, monitors, workspaces, focused })
                .await;
        }
        Err(e) => tracing::warn!(error = %e, "client snapshot failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::WindowMeta;

    fn meta(workspace: &str, home: Option<&str>) -> WindowMeta {
        WindowMeta {
            workspace: workspace.into(),
            minimized: workspace == hypr::minimize::WORKSPACE,
            home: home.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn minimizing_remembers_where_the_window_was() {
        let a = hypr::Address::parse("0x1");
        match DockCommand::minimize(&a, &meta("3", Some("7"))) {
            DockCommand::Minimize { window, workspace, stale_home, .. } => {
                assert_eq!(window, a);
                assert_eq!(workspace, "3");
                assert_eq!(stale_home.as_deref(), Some("7"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_group_goes_and_comes_back_tagged_as_one() {
        // Hyprland moves a whole tab group with any one of its windows, so
        // every window in it has to carry the home tag, and lose it again.
        let a = hypr::Address::parse("0x1");
        let mut m = meta("3", None);
        m.group = vec![hypr::Address::parse("0x2")];
        match DockCommand::minimize(&a, &m) {
            DockCommand::Minimize { group, .. } => assert_eq!(group, m.group),
            other => panic!("{other:?}"),
        }
        match DockCommand::restore(&a, &m) {
            DockCommand::Restore { group, .. } => assert_eq!(group, m.group),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn opening_a_minimized_window_restores_it_and_any_other_is_focused() {
        let a = hypr::Address::parse("0x1");
        let parked = meta("special:minimized", Some("2"));
        assert!(matches!(
            DockCommand::open_window(&a, Some(&parked)),
            DockCommand::Restore { home: Some(ref h), .. } if h == "2"
        ));
        assert!(matches!(DockCommand::open_window(&a, Some(&meta("2", None))), DockCommand::Focus(_)));
        assert!(matches!(DockCommand::open_window(&a, None), DockCommand::Focus(_)));
    }
}
