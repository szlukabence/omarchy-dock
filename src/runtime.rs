//! The async worker thread.
//!
//! GTK owns the main thread, so everything async — Hyprland IPC now, D-Bus
//! later — lives on a Tokio runtime here and reaches the UI only as messages
//! on the event channel. Nothing in this module may touch a GTK type.

use crate::event::{AppEvent, Sender};
use crate::hypr;

/// Start the worker. The returned handle can be dropped; the thread owns its
/// runtime and keeps running until the process exits.
pub fn spawn(tx: Sender) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("omarchy-dock-async".into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(error = %e, "cannot start async runtime");
                return;
            }
        };

        rt.block_on(async move {
            // Prime the UI with a full snapshot before streaming deltas, so
            // the dock reflects reality even for windows opened before start.
            match hypr::request::clients().await {
                Ok(clients) => {
                    tracing::info!(count = clients.len(), "initial client snapshot");
                    let _ = tx.send(AppEvent::HyprSnapshot(clients)).await;
                }
                Err(e) => tracing::warn!(error = %e, "no initial snapshot"),
            }

            let tx2 = tx.clone();
            // `listen` only returns if it gives up reconnecting.
            let _ = hypr::events::listen(move |event| {
                // Unbounded channel, so this never blocks the reader task.
                let _ = tx2.send_blocking(AppEvent::Hypr(event));
            })
            .await;
        });
    })
}
