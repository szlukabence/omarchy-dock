//! Dispatchers.
//!
//! Hyprland 0.56 replaced the classic `/dispatch <name> <args>` protocol with
//! Lua: the socket now evaluates `hl.dsp.*` expressions. The old text form
//! fails with "expected a dispatcher (e.g. hl.dsp.window.close())".
//!
//! **Every dispatcher here targets an explicit window.** The bare forms
//! (`hl.dsp.window.close()`, `.move()`, …) act on whatever is *currently
//! focused*, which is almost never what a dock click means — the user clicked
//! a specific icon, and focus may have moved since. Acting on the active
//! window closed the wrong application during development.

// Wired to clicks and context menus in Phase 5; verified live in Phase 2.
#![allow(dead_code)]

use super::{request, Address};
use anyhow::Result;

/// Run a Lua dispatcher expression.
async fn run(expr: &str) -> Result<()> {
    let reply = request::raw(&format!("/dispatch {expr}")).await?;
    let reply = reply.trim();
    // Hyprland answers "ok" on success and an `error: …` string otherwise.
    if reply.eq_ignore_ascii_case("ok") || reply.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("dispatch `{expr}` failed: {reply}"))
    }
}

/// Escape a string for embedding in a single-quoted Lua literal.
fn lua_str(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Focus a specific window by address.
pub async fn focus_window(addr: &Address) -> Result<()> {
    run(&format!(
        "hl.dsp.focus({{ window = {} }})",
        lua_str(&format!("address:{}", addr.prefixed()))
    ))
    .await
}

/// Switch to a workspace by name (`"3"`, `"e+1"`, `"previous"`).
pub async fn focus_workspace(name: &str) -> Result<()> {
    run(&format!("hl.dsp.focus({{ workspace = {} }})", lua_str(name))).await
}

/// Toggle a special (scratchpad) workspace.
pub async fn toggle_special(name: &str) -> Result<()> {
    run(&format!("hl.dsp.workspace.toggle_special({})", lua_str(name))).await
}

/// Close one specific window. Never the active one implicitly.
pub async fn close_window(addr: &Address) -> Result<()> {
    run(&format!(
        "hl.dsp.window.close({{ window = {} }})",
        lua_str(&format!("address:{}", addr.prefixed()))
    ))
    .await
}

/// Move a specific window to a workspace, optionally following it.
pub async fn move_window_to_workspace(addr: &Address, workspace: &str, follow: bool) -> Result<()> {
    run(&format!(
        "hl.dsp.window.move({{ window = {}, workspace = {}, follow = {follow} }})",
        lua_str(&format!("address:{}", addr.prefixed())),
        lua_str(workspace),
    ))
    .await
}

/// Launch a command through Hyprland so it inherits the compositor's
/// environment rather than the dock's.
pub async fn exec(command: &str) -> Result<()> {
    run(&format!("hl.dsp.exec_cmd({})", lua_str(command))).await
}

/// Toggle the tiling layout's split orientation.
pub async fn toggle_split() -> Result<()> {
    run("hl.dsp.layout('togglesplit')").await
}
