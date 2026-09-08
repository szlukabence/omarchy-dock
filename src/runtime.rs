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

/// Start the worker. The thread owns its runtime and runs until process exit.
pub fn spawn(tx: Sender) -> std::io::Result<SnapshotRequest> {
    // Capacity 1: requests are "please resync", so a queued one is as good as
    // ten. `try_send` failing on a full channel is the desired coalescing.
    let (req_tx, mut req_rx) = tokio::sync::mpsc::channel::<()>(1);

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

            while req_rx.recv().await.is_some() {
                tokio::time::sleep(COALESCE).await;
                // Drop anything that piled up during the wait; one query
                // answers them all.
                while req_rx.try_recv().is_ok() {}
                snapshot(&tx).await;
            }
        });
    })?;

    Ok(req_tx)
}

async fn snapshot(tx: &Sender) {
    match hypr::request::clients().await {
        Ok(clients) => {
            let _ = tx.send(AppEvent::HyprSnapshot(clients)).await;
        }
        Err(e) => tracing::warn!(error = %e, "client snapshot failed"),
    }
}
