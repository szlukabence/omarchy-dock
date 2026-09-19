//! Media players, over MPRIS.
//!
//! Every mainstream Linux media player — Spotify, browsers, mpv, Rhythmbox —
//! publishes itself on the session bus as `org.mpris.MediaPlayer2.<name>`. The
//! dock watches those names come and go and reads each player's state, so the
//! app playing music can show a progress ring on its icon and offer
//! play/pause, next and previous in its context menu.
//!
//! Omarchy's shell has a media service of its own, but it is an optional
//! plugin (disabled on some machines), so the dock talks to MPRIS directly
//! rather than depending on it.
//!
//! Nothing is polled while nothing plays. Player state changes arrive as
//! `PropertiesChanged` signals; the playback *position* is the exception — the
//! specification deliberately does not signal it — so it is re-read once a
//! second, only while something is actually playing.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::time::Duration;
use zbus::zvariant::OwnedValue;

use crate::event::{AppEvent, Sender};

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";

/// One media player, as the dock shows it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Player {
    /// Bus name, e.g. `org.mpris.MediaPlayer2.spotify`. Where commands go.
    pub bus: String,
    /// Desktop entry id the player declares, e.g. `spotify`. What ties it to a
    /// dock icon.
    pub desktop_entry: String,
    pub identity: String,
    pub playing: bool,
    pub title: String,
    pub artist: String,
    /// Track length and position in microseconds; zero when unknown.
    pub length_us: i64,
    pub position_us: i64,
    pub can_next: bool,
    pub can_previous: bool,
}

impl Player {
    /// Fraction of the track played, when that is knowable.
    pub fn progress(&self) -> Option<f64> {
        (self.length_us > 0).then(|| (self.position_us as f64 / self.length_us as f64).clamp(0.0, 1.0))
    }

    /// Whether this player belongs to the dock item with desktop id `key`.
    ///
    /// By the `DesktopEntry` the player declares, falling back to its bus name
    /// suffix for players that declare none. Exact matches only: a browser's
    /// players all declare the browser, and guessing which web app a tab
    /// belongs to would put the wrong ring on the wrong icon.
    pub fn belongs_to(&self, key: &str) -> bool {
        if !self.desktop_entry.is_empty() {
            return self.desktop_entry.eq_ignore_ascii_case(key);
        }
        self.bus
            .strip_prefix(PREFIX)
            .is_some_and(|name| name.eq_ignore_ascii_case(key))
    }
}

/// A transport command for one player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    PlayPause,
    Next,
    Previous,
}

#[zbus::proxy(interface = "org.mpris.MediaPlayer2", default_path = "/org/mpris/MediaPlayer2")]
trait Root {
    #[zbus(property)]
    fn identity(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn desktop_entry(&self) -> zbus::Result<String>;
}

#[zbus::proxy(interface = "org.mpris.MediaPlayer2.Player", default_path = "/org/mpris/MediaPlayer2")]
trait MprisPlayer {
    fn play_pause(&self) -> zbus::Result<()>;
    fn next(&self) -> zbus::Result<()>;
    fn previous(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn metadata(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
    #[zbus(property)]
    fn position(&self) -> zbus::Result<i64>;
    #[zbus(property)]
    fn can_go_next(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_go_previous(&self) -> zbus::Result<bool>;
}

/// Read a player's current state. Missing properties are tolerated: the
/// specification makes several optional and players omit them freely.
async fn read(conn: &zbus::Connection, bus: &str) -> Result<Player> {
    let root = RootProxy::builder(conn)
        .destination(bus.to_string())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    let p = MprisPlayerProxy::builder(conn)
        .destination(bus.to_string())?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;

    let meta = p.metadata().await.unwrap_or_default();
    let text = |k: &str| {
        meta.get(k)
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_default()
    };
    // xesam:artist is a list; show them joined.
    let artist = meta
        .get("xesam:artist")
        .and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok())
        .map(|a| a.join(", "))
        .unwrap_or_default();
    // mpris:length is specified as x (i64), but some players send t (u64).
    let length_us = meta
        .get("mpris:length")
        .and_then(|v| {
            i64::try_from(v.try_clone().ok()?)
                .ok()
                .or_else(|| u64::try_from(v.try_clone().ok()?).ok().map(|u| u as i64))
        })
        .unwrap_or(0);

    Ok(Player {
        bus: bus.to_string(),
        desktop_entry: root.desktop_entry().await.unwrap_or_default(),
        identity: root.identity().await.unwrap_or_default(),
        playing: p.playback_status().await.is_ok_and(|s| s == "Playing"),
        title: text("xesam:title"),
        artist,
        length_us,
        position_us: p.position().await.unwrap_or(0),
        can_next: p.can_go_next().await.unwrap_or(false),
        can_previous: p.can_go_previous().await.unwrap_or(false),
    })
}

async fn snapshot(conn: &zbus::Connection) -> Vec<Player> {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else { return Vec::new() };
    let names = dbus.list_names().await.unwrap_or_default();
    let mut players = Vec::new();
    for name in names.iter().map(|n| n.as_str()).filter(|n| n.starts_with(PREFIX)) {
        match read(conn, name).await {
            Ok(p) => players.push(p),
            Err(e) => tracing::debug!(name, error = %e, "cannot read media player"),
        }
    }
    players.sort_by(|a, b| a.bus.cmp(&b.bus));
    players
}

/// Send one transport command.
pub async fn command(conn: &zbus::Connection, bus: &str, action: Action) -> Result<()> {
    let p = MprisPlayerProxy::builder(conn).destination(bus.to_string())?.build().await?;
    match action {
        Action::PlayPause => p.play_pause().await,
        Action::Next => p.next().await,
        Action::Previous => p.previous().await,
    }
    .with_context(|| format!("{bus} refused {action:?}"))
}

/// Watch media players until the process exits, reporting the whole set on
/// every change.
pub async fn serve(tx: Sender) -> Result<()> {
    let conn = zbus::Connection::session().await.context("connecting to the session bus")?;

    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut owners = dbus.receive_name_owner_changed().await?;

    // Any player's PropertiesChanged on the MPRIS path.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .path(PATH)?
        .build();
    let mut changes = zbus::MessageStream::for_match_rule(rule, &conn, None).await?;

    let mut last = snapshot(&conn).await;
    let _ = tx.send(AppEvent::Media(last.clone())).await;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let playing = last.iter().any(|p| p.playing);
        tokio::select! {
            Some(signal) = owners.next() => {
                let Ok(args) = signal.args() else { continue };
                if !args.name().as_str().starts_with(PREFIX) { continue }
            }
            Some(_) = changes.next() => {}
            // Position is the one property the spec does not signal, so it is
            // re-read on a timer — and only while something plays.
            _ = tick.tick(), if playing => {}
            else => break,
        }
        let now = snapshot(&conn).await;
        if now != last {
            last = now;
            if tx.send(AppEvent::Media(last.clone())).await.is_err() {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(bus: &str, entry: &str) -> Player {
        Player { bus: bus.into(), desktop_entry: entry.into(), ..Default::default() }
    }

    #[test]
    fn a_player_belongs_to_the_icon_it_names() {
        assert!(player("org.mpris.MediaPlayer2.spotify", "spotify").belongs_to("spotify"));
        assert!(player("org.mpris.MediaPlayer2.spotify", "Spotify").belongs_to("spotify"));
        assert!(!player("org.mpris.MediaPlayer2.spotify", "spotify").belongs_to("chromium"));
    }

    #[test]
    fn a_player_without_a_desktop_entry_matches_by_bus_name() {
        assert!(player("org.mpris.MediaPlayer2.mpv", "").belongs_to("mpv"));
        // A browser instance suffix must not match the browser loosely.
        assert!(!player("org.mpris.MediaPlayer2.chromium.instance42", "").belongs_to("chromium"));
    }

    #[test]
    fn progress_is_clamped_and_unknown_without_a_length() {
        let mut p = player("b", "e");
        assert_eq!(p.progress(), None);
        p.length_us = 200;
        p.position_us = 50;
        assert_eq!(p.progress(), Some(0.25));
        p.position_us = 900; // players overshoot at track change
        assert_eq!(p.progress(), Some(1.0));
    }
}
