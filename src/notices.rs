//! Notifications as they are sent, for unread badges.
//!
//! Every notification on the desktop is a `Notify` call to
//! `org.freedesktop.Notifications` on the session bus, whoever displays it.
//! The dock watches those calls as a bus monitor — it listens, it never
//! answers, so the shell's notification service is untouched — and keeps
//! only who sent each one: the app's name, its desktop id, and for a browser
//! the site it came from. The text of a notification is never kept.
//!
//! It watches only while badges are on (`items.notification_badges`, off by
//! default): turning them off drops the bus connection, and with it the
//! monitor.
//!
//! The shell's own history was the other candidate, but it holds only the
//! last few notifications that left the screen and has no notion of "read",
//! so it cannot answer "how many has Gmail sent since I last looked".

use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

use crate::event::{AppEvent, Sender};

/// Who a notification came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Notice {
    pub app_name: String,
    /// The `desktop-entry` hint, e.g. `chromium`; empty when not given.
    pub desktop_entry: String,
    /// For a browser, the host of the site that sent it.
    pub origin: Option<String>,
}

/// Browsers whose notifications stand for a site rather than for the browser.
const BROWSERS: [&str; 8] =
    ["chromium", "chrome", "google-chrome", "microsoft-edge", "brave", "vivaldi", "opera", "helium"];

fn is_browser(n: &Notice) -> bool {
    let name = |s: &str| {
        let s = s.to_ascii_lowercase();
        BROWSERS.iter().any(|b| s.starts_with(b) || s.replace(' ', "-").starts_with(b))
    };
    name(&n.desktop_entry) || name(&n.app_name)
}

/// The host of a URL, lowercased.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    let host = host.split(':').next()?.to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// The site a browser notification came from.
///
/// Chromium names the origin two ways, depending on what the notification
/// server supports: a KDE hint, or a link (or bare host) on the first line of
/// the body. A bare host is only trusted from a browser, where that is what
/// the first line means.
fn origin(hints: &HashMap<String, OwnedValue>, body: &str, browser: bool) -> Option<String> {
    let hint = hints
        .get("x-kde-origin-name")
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok());
    if let Some(h) = hint.filter(|h| !h.is_empty()) {
        return url_host(&h).or_else(|| Some(h.to_ascii_lowercase()));
    }
    if let Some(i) = body.find("href=") {
        let rest = &body[i + 5..];
        let quote = rest.chars().next()?;
        let url = rest[1..].split(quote).next()?;
        if let Some(host) = url_host(url) {
            return Some(host);
        }
    }
    if browser {
        let first = body.lines().next()?.trim();
        let bare = !first.is_empty() && first.contains('.') && !first.contains(char::is_whitespace);
        if bare {
            return url_host(first).or_else(|| Some(first.to_ascii_lowercase()));
        }
    }
    None
}

/// Read one `Notify` call. `None` for updates to a notification already seen
/// and for transient ones (volume, brightness), which are not "unread" things.
fn notice(
    app_name: String,
    replaces_id: u32,
    body: &str,
    hints: &HashMap<String, OwnedValue>,
) -> Option<Notice> {
    if replaces_id != 0 {
        return None;
    }
    if hints.get("transient").and_then(|v| bool::try_from(v).ok()) == Some(true) {
        return None;
    }
    let desktop_entry = hints
        .get("desktop-entry")
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default();
    let mut n = Notice { app_name, desktop_entry, origin: None };
    n.origin = origin(hints, body, is_browser(&n));
    Some(n)
}

/// Watch for notifications while `enabled` says badges are on, until the
/// process exits.
pub async fn serve(tx: Sender, mut enabled: tokio::sync::watch::Receiver<bool>) -> Result<()> {
    loop {
        // Wait for badges to be turned on. A closed channel means the UI is
        // gone, and so is anyone to show a badge to.
        while !*enabled.borrow_and_update() {
            if enabled.changed().await.is_err() {
                return Ok(());
            }
        }
        tracing::info!("watching notifications for unread badges");
        tokio::select! {
            done = watch(&tx) => return done,
            () = turned_off(&mut enabled) => {
                // Leaving `watch` drops its connection, which ends the monitor.
                tracing::info!("notification badges off; stopped watching notifications");
            }
        }
    }
}

/// Resolves once `enabled` goes false; never, if its sender is dropped.
async fn turned_off(enabled: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if enabled.changed().await.is_err() {
            return std::future::pending().await;
        }
        if !*enabled.borrow_and_update() {
            return;
        }
    }
}

/// Monitor `Notify` calls on a connection of its own until `tx` closes.
async fn watch(tx: &Sender) -> Result<()> {
    let conn = zbus::Connection::session().await.context("connecting to the session bus")?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::MethodCall)
        .interface("org.freedesktop.Notifications")?
        .member("Notify")?
        .build();
    zbus::fdo::MonitoringProxy::new(&conn)
        .await?
        .become_monitor(&[rule], 0)
        .await
        .context("becoming a bus monitor")?;

    type Notify<'a> =
        (String, u32, String, String, String, Vec<String>, HashMap<String, OwnedValue>, i32);
    let mut stream = zbus::MessageStream::from(&conn);
    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else { continue };
        let header = msg.header();
        if header.member().is_none_or(|m| m.as_str() != "Notify") {
            continue;
        }
        let Ok((app_name, replaces_id, _icon, _summary, body, _actions, hints, _timeout)) =
            msg.body().deserialize::<Notify>()
        else {
            continue;
        };
        if let Some(n) = notice(app_name, replaces_id, &body, &hints) {
            tracing::debug!(app = %n.app_name, entry = %n.desktop_entry, origin = ?n.origin, "notification");
            if tx.send(AppEvent::Notified(n)).await.is_err() {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hints(pairs: &[(&str, &str)]) -> HashMap<String, OwnedValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OwnedValue::try_from(zbus::zvariant::Value::from(*v)).unwrap()))
            .collect()
    }

    #[test]
    fn a_chromium_notification_names_its_site_by_link() {
        let h = hints(&[("desktop-entry", "chromium")]);
        let n = notice(
            "Chromium".into(),
            0,
            "<a href=\"https://mail.google.com/\">mail.google.com</a>\n\nNew mail",
            &h,
        )
        .unwrap();
        assert_eq!(n.desktop_entry, "chromium");
        assert_eq!(n.origin.as_deref(), Some("mail.google.com"));
    }

    #[test]
    fn a_chromium_notification_names_its_site_by_bare_host_or_kde_hint() {
        let h = hints(&[("desktop-entry", "chromium")]);
        let n = notice("Chromium".into(), 0, "www.messenger.com\n\nHi", &h).unwrap();
        assert_eq!(n.origin.as_deref(), Some("www.messenger.com"));

        let h = hints(&[("x-kde-origin-name", "https://outlook.live.com")]);
        let n = notice("Chromium".into(), 0, "whatever", &h).unwrap();
        assert_eq!(n.origin.as_deref(), Some("outlook.live.com"));
    }

    #[test]
    fn a_first_line_that_looks_like_a_host_means_nothing_from_a_non_browser() {
        let n = notice("Spotify".into(), 0, "feat.artist\nsong", &hints(&[])).unwrap();
        assert_eq!(n.origin, None);
    }

    #[tokio::test]
    async fn watching_stops_when_badges_are_turned_off() {
        let (tx, mut rx) = tokio::sync::watch::channel(true);
        let off = tokio::spawn(async move { turned_off(&mut rx).await });
        // Still on, even if the setting is saved again: keep watching.
        tx.send_replace(true);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!off.is_finished());
        tx.send_replace(false);
        tokio::time::timeout(std::time::Duration::from_secs(1), off).await.unwrap().unwrap();
    }

    #[test]
    fn badges_are_off_unless_asked_for() {
        assert!(!crate::config::Config::default().items.notification_badges);
    }

    #[test]
    fn updates_and_transient_notifications_are_not_counted() {
        assert!(notice("Chromium".into(), 7, "", &hints(&[])).is_none());
        let mut h = hints(&[]);
        h.insert("transient".into(), OwnedValue::from(true));
        assert!(notice("volume".into(), 0, "", &h).is_none());
    }
}
