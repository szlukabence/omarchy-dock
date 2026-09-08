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
    /// Show or hide a special (scratchpad) workspace. Used by the workspace
    /// pills in Phase 6.
    #[allow(dead_code)]
    ToggleSpecial(String),
}

pub type CommandSender = tokio::sync::mpsc::Sender<DockCommand>;

/// Both handles the UI needs to drive the worker.
#[derive(Clone)]
pub struct Handles {
    pub snapshot: SnapshotRequest,
    pub commands: CommandSender,
}

/// Start the worker. The thread owns its runtime and runs until process exit.
pub fn spawn(tx: Sender) -> std::io::Result<Handles> {
    // Capacity 1: requests are "please resync", so a queued one is as good as
    // ten. `try_send` failing on a full channel is the desired coalescing.
    let (req_tx, mut req_rx) = tokio::sync::mpsc::channel::<()>(1);
    // Commands are user actions; a small queue absorbs fast clicking without
    // ever dropping one silently.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<DockCommand>(32);

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

    Ok(Handles { snapshot: req_tx, commands: cmd_tx })
}

async fn execute(cmd: &DockCommand) -> anyhow::Result<()> {
    use hypr::dispatch;
    match cmd {
        DockCommand::Focus(addr) => dispatch::focus_window(addr).await,
        DockCommand::Close(addr) => dispatch::close_window(addr).await,
        DockCommand::Exec(cmd) => dispatch::exec(cmd).await,
        DockCommand::ToggleSpecial(name) => dispatch::toggle_special(name).await,
    }
}

async fn snapshot(tx: &Sender) {
    // Both queries in flight together: they are independent and the dock
    // needs them consistently, so serialising them only adds latency.
    let (clients, monitors, active) = tokio::join!(
        hypr::request::clients(),
        hypr::request::monitors(),
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
            let focused = active.ok().flatten().map(|c| c.address);
            let _ = tx.send(AppEvent::HyprSnapshot { clients, monitors, focused }).await;
        }
        Err(e) => tracing::warn!(error = %e, "client snapshot failed"),
    }
}
