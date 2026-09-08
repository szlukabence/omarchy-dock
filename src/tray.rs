//! System tray: a StatusNotifierItem host.
//!
//! The tray is a D-Bus protocol, not an X11 leftover. Applications register a
//! `StatusNotifierItem` with a `StatusNotifierWatcher`, and any number of
//! *hosts* may then display them. Omarchy's shell already runs a watcher and a
//! host for the bar's tray widget; this adds a second host, which the protocol
//! explicitly allows — items are broadcast to every registered host.
//!
//! Only what a dock needs is implemented:
//!
//! * **Discovery** — register as a host, read the watcher's current items, and
//!   follow registration and unregistration signals.
//! * **Presentation** — `IconName` against the icon theme first, falling back
//!   to the raw `IconPixmap` an app supplies when it ships no themed icon.
//! * **Activation** — left click `Activate`, middle click `SecondaryActivate`,
//!   right click `ContextMenu`, which asks the application to post its own
//!   menu. Implementing DBusMenu to draw that menu ourselves would mean
//!   reproducing another application's UI, and getting it subtly wrong.
//!
//! **Known limitation.** `ContextMenu` is optional, and some items serve a
//! menu only over DBusMenu via their `Menu` property — cc-switch on this
//! machine is one. Right-clicking those does nothing, and the dock says so in
//! its log rather than pretending otherwise. Supporting them means
//! implementing DBusMenu, which is a whole protocol, not a fallback.
//!
//! Everything here runs on the worker's Tokio runtime and reaches the dock as
//! an `AppEvent`, because GTK widgets are main-thread-only.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::collections::HashMap;

use crate::event::{AppEvent, Sender};

/// One tray item as the dock needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct TrayItem {
    /// `<bus name><object path>`, which is what the watcher hands out and what
    /// every method call has to be addressed to. Also the item's identity
    /// across updates.
    pub service: String,
    /// Application id, e.g. "ChatGPT". Used as the fallback label.
    pub id: String,
    pub title: String,
    /// Themed icon name, if the app supplies one.
    pub icon_name: String,
    /// Extra directory to search for `icon_name`, which apps shipping their
    /// own icons outside the theme set.
    pub icon_theme_path: String,
    /// Raw ARGB32 icon, network byte order, as a fallback when there is no
    /// themed icon: width, height, pixels.
    ///
    /// Shared rather than owned because the whole item list is cloned on every
    /// dock rebuild, and an icon's worth of pixels per item per rebuild is
    /// real work for no reason.
    pub pixmap: Option<std::sync::Arc<(i32, i32, Vec<u8>)>>,
    /// "Passive", "Active" or "NeedsAttention".
    pub status: String,
}

impl TrayItem {
    /// What to show under the icon.
    pub fn label(&self) -> &str {
        if !self.title.is_empty() {
            &self.title
        } else {
            &self.id
        }
    }

    /// Whether the item is asking for attention, which the dock bounces for.
    pub fn needs_attention(&self) -> bool {
        self.status == "NeedsAttention"
    }
}

/// Which button was used on a tray icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Click {
    Primary,
    Secondary,
    Middle,
}

// ── D-Bus proxies ───────────────────────────────────────────────────────────

#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait StatusNotifierWatcher {
    fn register_status_notifier_host(&self, service: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> zbus::Result<Vec<String>>;

    #[zbus(signal)]
    fn status_notifier_item_registered(&self, service: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn status_notifier_item_unregistered(&self, service: String) -> zbus::Result<()>;
}

/// The item interface.
///
/// Two spellings exist in the wild: KDE's original `org.kde.StatusNotifierItem`
/// and freedesktop's `org.freedesktop.StatusNotifierItem`. Applications
/// implement one or the other, so both are tried.
#[zbus::proxy(interface = "org.kde.StatusNotifierItem")]
trait StatusNotifierItem {
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn context_menu(&self, x: i32, y: i32) -> zbus::Result<()>;

    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn title(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn icon_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn icon_theme_path(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn icon_pixmap(&self) -> zbus::Result<Vec<(i32, i32, Vec<u8>)>>;
}

#[zbus::proxy(interface = "org.freedesktop.StatusNotifierItem")]
trait FreedesktopStatusNotifierItem {
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn secondary_activate(&self, x: i32, y: i32) -> zbus::Result<()>;
    fn context_menu(&self, x: i32, y: i32) -> zbus::Result<()>;

    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn title(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn icon_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn icon_theme_path(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn icon_pixmap(&self) -> zbus::Result<Vec<(i32, i32, Vec<u8>)>>;
}

/// Split the watcher's `<bus name><object path>` into its two halves.
///
/// The watcher hands out either `org.example.Item-123-1/StatusNotifierItem` or
/// a unique name with a path, `:1.42/org/ayatana/NotificationItem/foo`. An item
/// that gives no path is addressed at the conventional default.
fn split_service(service: &str) -> (String, String) {
    match service.find('/') {
        Some(at) => (service[..at].to_string(), service[at..].to_string()),
        None => (service.to_string(), "/StatusNotifierItem".to_string()),
    }
}

/// Read every property the dock displays.
///
/// A missing property is not an error: the specification makes most of them
/// optional, and applications routinely omit the ones they do not use. Each
/// falls back to empty rather than failing the whole item, or one absent
/// `IconThemePath` would hide an otherwise fine icon.
async fn read_item(conn: &zbus::Connection, service: &str) -> Result<TrayItem> {
    let (name, path) = split_service(service);

    macro_rules! collect {
        ($proxy:expr) => {{
            let p = $proxy;
            TrayItem {
                service: service.to_string(),
                id: p.id().await.unwrap_or_default(),
                title: p.title().await.unwrap_or_default(),
                status: p.status().await.unwrap_or_else(|_| "Active".into()),
                icon_name: p.icon_name().await.unwrap_or_default(),
                icon_theme_path: p.icon_theme_path().await.unwrap_or_default(),
                // Largest pixmap wins: apps ship several sizes and the dock's
                // icons are big, so scaling up a 16px one looks wretched.
                pixmap: p
                    .icon_pixmap()
                    .await
                    .ok()
                    .and_then(|v| v.into_iter().max_by_key(|(w, h, _)| w * h))
                    .filter(|(w, h, px)| {
                        // A truncated pixmap would be read past its end when
                        // uploaded as a texture.
                        *w > 0 && *h > 0 && px.len() >= (w * h * 4) as usize
                    })
                    .map(std::sync::Arc::new),
            }
        }};
    }

    // KDE's spelling first, since it is the one the specification names.
    let kde = StatusNotifierItemProxy::builder(conn)
        .destination(name.clone())?
        .path(path.clone())?
        .build()
        .await;

    if let Ok(p) = kde {
        // `Id` is mandatory, so its absence is how we detect the other
        // spelling rather than trusting the interface to exist.
        if p.id().await.is_ok() {
            return Ok(collect!(p));
        }
    }

    let fd = FreedesktopStatusNotifierItemProxy::builder(conn)
        .destination(name)?
        .path(path)?
        .build()
        .await
        .context("neither StatusNotifierItem interface responded")?;
    Ok(collect!(fd))
}

/// Deliver a click to an item.
pub async fn click(conn: &zbus::Connection, service: &str, click: Click) -> Result<()> {
    let (name, path) = split_service(service);

    // Position is where the menu should appear. A layer-shell surface has no
    // global coordinates to offer, and every implementation treats (0, 0) as
    // "you decide", which is what we want anyway.
    let (x, y) = (0, 0);

    let kde = StatusNotifierItemProxy::builder(conn)
        .destination(name.clone())?
        .path(path.clone())?
        .build()
        .await?;

    let result = match click {
        Click::Primary => kde.activate(x, y).await,
        Click::Secondary => kde.context_menu(x, y).await,
        Click::Middle => kde.secondary_activate(x, y).await,
    };
    if result.is_ok() {
        return Ok(());
    }

    let fd = FreedesktopStatusNotifierItemProxy::builder(conn)
        .destination(name)?
        .path(path)?
        .build()
        .await?;
    match click {
        Click::Primary => fd.activate(x, y).await,
        Click::Secondary => fd.context_menu(x, y).await,
        Click::Middle => fd.secondary_activate(x, y).await,
    }
    .with_context(|| match click {
        // The common case, and worth naming: `ContextMenu` is optional, and an
        // item that serves its menu only over DBusMenu will refuse it.
        Click::Secondary => format!(
            "{service} has no ContextMenu; it likely serves its menu over              DBusMenu, which the dock does not implement"
        ),
        _ => format!("{service} refused the click"),
    })?;
    Ok(())
}

/// Host the tray until the process exits, reporting the item list on change.
///
/// The whole list is sent on every change rather than deltas: it is a handful
/// of items, and a list the dock can render directly cannot drift out of sync
/// with one it has to maintain.
pub async fn serve(tx: Sender) -> Result<()> {
    let conn = zbus::Connection::session()
        .await
        .context("connecting to the session bus")?;

    let watcher = StatusNotifierWatcherProxy::new(&conn)
        .await
        .context("no StatusNotifierWatcher on the bus")?;

    // Registering makes the watcher broadcast item changes to us. The name is
    // conventional: hosts are `org.kde.StatusNotifierHost-<pid>`.
    let host_name = format!("org.kde.StatusNotifierHost-{}", std::process::id());
    conn.request_name(host_name.as_str())
        .await
        .with_context(|| format!("claiming {host_name}"))?;
    watcher
        .register_status_notifier_host(&host_name)
        .await
        .context("registering as a tray host")?;

    tracing::info!(host = %host_name, "tray host registered");

    let mut registered = watcher.receive_status_notifier_item_registered().await?;
    let mut unregistered = watcher.receive_status_notifier_item_unregistered().await?;

    let mut items: HashMap<String, TrayItem> = HashMap::new();

    // Seed from whatever is already registered: the dock may well start after
    // every tray application has.
    for service in watcher.registered_status_notifier_items().await.unwrap_or_default() {
        match read_item(&conn, &service).await {
            Ok(item) => {
                items.insert(service, item);
            }
            Err(e) => tracing::debug!(service, error = %e, "cannot read tray item"),
        }
    }
    send(&tx, &items).await;

    loop {
        tokio::select! {
            Some(signal) = registered.next() => {
                let Ok(args) = signal.args() else { continue };
                let service = args.service.to_string();
                match read_item(&conn, &service).await {
                    Ok(item) => { items.insert(service, item); }
                    Err(e) => tracing::debug!(service, error = %e, "cannot read new tray item"),
                }
                send(&tx, &items).await;
            }
            Some(signal) = unregistered.next() => {
                let Ok(args) = signal.args() else { continue };
                items.remove(&args.service.to_string());
                send(&tx, &items).await;
            }
            else => break,
        }
    }

    Ok(())
}

/// Send the current items, in a stable order.
async fn send(tx: &Sender, items: &HashMap<String, TrayItem>) {
    let mut list: Vec<TrayItem> = items.values().cloned().collect();
    // Sorted by service name so icons do not reshuffle themselves whenever a
    // hash map iterates in a different order.
    list.sort_by(|a, b| a.service.cmp(&b.service));
    tracing::debug!(count = list.len(), "tray items");
    let _ = tx.send(AppEvent::Tray(list)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_service_with_a_path_splits_at_the_first_slash() {
        // The two forms the watcher actually hands out.
        assert_eq!(
            split_service("org.freedesktop.StatusNotifierItem-230825-1/StatusNotifierItem"),
            ("org.freedesktop.StatusNotifierItem-230825-1".into(), "/StatusNotifierItem".into())
        );
        assert_eq!(
            split_service(":1.1262/org/ayatana/NotificationItem/tray_icon"),
            (":1.1262".into(), "/org/ayatana/NotificationItem/tray_icon".into())
        );
    }

    #[test]
    fn a_bare_bus_name_gets_the_conventional_path() {
        assert_eq!(
            split_service("org.example.Item"),
            ("org.example.Item".into(), "/StatusNotifierItem".into())
        );
    }

    #[test]
    fn the_label_prefers_the_title_but_falls_back_to_the_id() {
        let mut item = TrayItem {
            service: "s".into(),
            id: "ChatGPT".into(),
            title: String::new(),
            icon_name: String::new(),
            icon_theme_path: String::new(),
            pixmap: None,
            status: "Active".into(),
        };
        assert_eq!(item.label(), "ChatGPT");
        item.title = "ChatGPT — 2 unread".into();
        assert_eq!(item.label(), "ChatGPT — 2 unread");
    }

    #[test]
    fn only_needs_attention_counts_as_urgent() {
        let item = |status: &str| TrayItem {
            service: "s".into(),
            id: "x".into(),
            title: String::new(),
            icon_name: String::new(),
            icon_theme_path: String::new(),
            pixmap: None,
            status: status.into(),
        };
        assert!(item("NeedsAttention").needs_attention());
        assert!(!item("Active").needs_attention());
        assert!(!item("Passive").needs_attention());
    }
}
