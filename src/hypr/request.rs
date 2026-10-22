//! Request/response IPC over `.socket.sock`.
//!
//! Each request is a fresh connection: write the command, read to EOF, done.
//! Commands prefixed `j/` return JSON.

// workspaces()/monitors()/active_window() are used by the state engine and
// multi-monitor logic in Phases 3 and 6.
#![allow(dead_code)]

use super::{model::*, request_socket};
use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Send one command and return the raw reply.
pub async fn raw(command: &str) -> Result<String> {
    let path = request_socket()?;
    let mut stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to {}", path.display()))?;

    stream.write_all(command.as_bytes()).await.context("writing request")?;
    // Hyprland replies once the request is complete; half-closing signals that.
    stream.shutdown().await.ok();

    let mut buf = String::new();
    stream.read_to_string(&mut buf).await.context("reading reply")?;
    Ok(buf)
}

/// Send a `j/` command and deserialise the JSON reply.
async fn json<T: serde::de::DeserializeOwned>(command: &str) -> Result<T> {
    let body = raw(command).await?;
    serde_json::from_str(&body)
        .with_context(|| format!("parsing reply to `{command}`: {}", truncate(&body)))
}

pub async fn clients() -> Result<Vec<Client>> {
    json("j/clients").await
}

pub async fn workspaces() -> Result<Vec<Workspace>> {
    json("j/workspaces").await
}

pub async fn monitors() -> Result<Vec<Monitor>> {
    json("j/monitors").await
}

/// Where the pointer is, in layout coordinates.
pub async fn cursor_pos() -> Result<(f64, f64)> {
    #[derive(serde::Deserialize)]
    struct Pos {
        x: f64,
        y: f64,
    }
    let p: Pos = json("j/cursorpos").await?;
    Ok((p.x, p.y))
}

/// The focused window, if any. Hyprland returns `{}` when nothing is focused,
/// which is not a `Client`, so parse leniently.
pub async fn active_window() -> Result<Option<Client>> {
    let body = raw("j/activewindow").await?;
    if body.trim().is_empty() || body.trim() == "{}" {
        return Ok(None);
    }
    Ok(serde_json::from_str(&body).ok())
}

fn truncate(s: &str) -> String {
    let s = s.trim();
    if s.len() > 200 {
        // At a character boundary: a reply full of titles is rarely ASCII.
        let end = (0..=200).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0);
        format!("{}…", &s[..end])
    } else {
        s.to_owned()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_long_reply_is_cut_between_characters() {
        // Byte 200 falls inside an "é": cutting there would panic.
        let reply = format!("a{}", "é".repeat(150));
        let cut = super::truncate(&reply);
        assert!(cut.ends_with('…'));
        assert_eq!(cut.trim_end_matches('…'), format!("a{}", "é".repeat(99)));
        assert_eq!(super::truncate(" short "), "short");
    }
}
