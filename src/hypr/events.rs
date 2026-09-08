//! The `.socket2.sock` push event stream.
//!
//! Lines arrive as `name>>payload`, with payload fields comma-separated.
//! Unlike dispatch, this format is unchanged in 0.56.
//!
//! Two parsing hazards:
//!
//! * **Titles contain commas.** `activewindow>>code,Foo, bar - VS Code` must
//!   split on the *first* comma only, or titles get truncated.
//! * **Addresses have no `0x` here** but do in JSON, so both go through
//!   `Address::parse`.

use super::{event_socket, Address};
use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HyprEvent {
    /// Focus moved. Empty class means focus was lost entirely.
    ActiveWindow { class: String, title: String },
    ActiveWindowAddr(Option<Address>),
    OpenWindow { addr: Address, workspace: String, class: String, title: String },
    CloseWindow(Address),
    MoveWindow { addr: Address, workspace: String },
    WindowTitle { addr: Address, title: String },
    /// A window is requesting attention.
    Urgent(Address),
    Workspace { name: String },
    CreateWorkspace { name: String },
    DestroyWorkspace { name: String },
    /// Special (scratchpad) workspace shown or hidden. Empty name = hidden.
    ActiveSpecial { name: String, monitor: String },
    MonitorAdded(String),
    MonitorRemoved(String),
    /// Fullscreen state changed; the dock hides for fullscreen windows.
    Fullscreen(bool),
    /// The event socket dropped and was re-established. Listeners must
    /// resynchronise, since events emitted while disconnected are lost.
    Reconnected,
}

/// Parse one `name>>payload` line. Returns `None` for events the dock ignores.
pub fn parse(line: &str) -> Option<HyprEvent> {
    let (name, data) = line.split_once(">>")?;

    // Split off exactly `n` leading fields, leaving the remainder intact.
    // Titles may contain commas, so the tail must never be split further.
    let head = |n: usize| -> Vec<&str> { data.splitn(n, ',').collect() };

    Some(match name {
        "activewindow" => {
            let p = head(2);
            HyprEvent::ActiveWindow {
                class: p.first().unwrap_or(&"").to_string(),
                title: p.get(1).unwrap_or(&"").to_string(),
            }
        }
        "activewindowv2" => HyprEvent::ActiveWindowAddr(
            (!data.is_empty()).then(|| Address::parse(data)),
        ),
        "openwindow" => {
            // address,workspace,class,title
            let p = head(4);
            HyprEvent::OpenWindow {
                addr: Address::parse(p.first()?),
                workspace: p.get(1).unwrap_or(&"").to_string(),
                class: p.get(2).unwrap_or(&"").to_string(),
                title: p.get(3).unwrap_or(&"").to_string(),
            }
        }
        "closewindow" => HyprEvent::CloseWindow(Address::parse(data)),
        "movewindow" => {
            let p = head(2);
            HyprEvent::MoveWindow {
                addr: Address::parse(p.first()?),
                workspace: p.get(1).unwrap_or(&"").to_string(),
            }
        }
        "windowtitlev2" => {
            let p = head(2);
            HyprEvent::WindowTitle {
                addr: Address::parse(p.first()?),
                title: p.get(1).unwrap_or(&"").to_string(),
            }
        }
        "urgent" => HyprEvent::Urgent(Address::parse(data)),
        "workspace" => HyprEvent::Workspace { name: data.to_string() },
        "createworkspace" => HyprEvent::CreateWorkspace { name: data.to_string() },
        "destroyworkspace" => HyprEvent::DestroyWorkspace { name: data.to_string() },
        "activespecial" => {
            let p = head(2);
            HyprEvent::ActiveSpecial {
                name: p.first().unwrap_or(&"").to_string(),
                monitor: p.get(1).unwrap_or(&"").to_string(),
            }
        }
        "monitoradded" => HyprEvent::MonitorAdded(data.to_string()),
        "monitorremoved" => HyprEvent::MonitorRemoved(data.to_string()),
        "fullscreen" => HyprEvent::Fullscreen(data.trim() == "1"),
        // `windowtitle` (v1) is superseded by v2, which carries the title too.
        // Layout, submap and screencast events are not dock-relevant.
        _ => return None,
    })
}

/// Stream events forever, reconnecting if Hyprland restarts.
///
/// Each callback runs on the caller's task; it must not block. On reconnect a
/// `Reconnected` event is emitted first so the consumer can re-query full
/// state — events during the gap are gone for good.
pub async fn listen<F>(mut on_event: F) -> Result<()>
where
    F: FnMut(HyprEvent),
{
    let mut backoff = std::time::Duration::from_millis(250);
    let mut first = true;

    loop {
        match connect_and_read(&mut on_event, &mut first).await {
            Ok(()) => tracing::warn!("Hyprland event socket closed"),
            Err(e) => tracing::warn!(error = %e, "Hyprland event socket error"),
        }

        tokio::time::sleep(backoff).await;
        // Back off to at most 5s so a compositor restart is picked up quickly
        // without spinning if Hyprland is gone for good.
        backoff = (backoff * 2).min(std::time::Duration::from_secs(5));
        first = false;
    }
}

async fn connect_and_read<F>(on_event: &mut F, first: &mut bool) -> Result<()>
where
    F: FnMut(HyprEvent),
{
    let path = event_socket()?;
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to {}", path.display()))?;

    tracing::info!(path = %path.display(), "Hyprland event stream connected");
    if !*first {
        on_event(HyprEvent::Reconnected);
    }

    let mut lines = BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await.context("reading event")? {
        if let Some(event) = parse(&line) {
            on_event(event);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_only_on_the_first_comma_so_titles_survive() {
        // A real title from this machine, which contains commas and dashes.
        let e = parse("activewindow>>code,Omarchy dock, phase 2 - Visual Studio Code");
        assert_eq!(
            e,
            Some(HyprEvent::ActiveWindow {
                class: "code".into(),
                title: "Omarchy dock, phase 2 - Visual Studio Code".into(),
            })
        );
    }

    #[test]
    fn normalises_addresses_across_both_sources() {
        // Event stream omits `0x`; JSON includes it. Both must agree.
        let from_event = parse("closewindow>>5654d12de5b0").unwrap();
        assert_eq!(from_event, HyprEvent::CloseWindow(Address::parse("0x5654D12DE5B0")));
        assert_eq!(Address::parse("5654d12de5b0").prefixed(), "0x5654d12de5b0");
    }

    #[test]
    fn openwindow_keeps_title_commas_but_splits_leading_fields() {
        let e = parse("openwindow>>abc123,3,kitty,a,b,c").unwrap();
        assert_eq!(
            e,
            HyprEvent::OpenWindow {
                addr: Address::parse("abc123"),
                workspace: "3".into(),
                class: "kitty".into(),
                title: "a,b,c".into(),
            }
        );
    }

    #[test]
    fn focus_loss_and_unknown_events() {
        assert_eq!(parse("activewindowv2>>"), Some(HyprEvent::ActiveWindowAddr(None)));
        assert_eq!(parse("activelayout>>kb,Hungarian"), None);
        assert_eq!(parse("garbage without separator"), None);
    }
}
