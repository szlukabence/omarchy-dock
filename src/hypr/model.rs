//! Typed views over Hyprland's JSON, holding only the fields the dock uses.
//!
//! `serde` ignores unknown fields by default, so Hyprland gaining new keys
//! will not break deserialisation.

// Workspace/Monitor and several Client fields are consumed by the state
// engine in Phase 3 and the workspace pills in Phase 5.
#![allow(dead_code)]

use super::Address;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceRef {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    #[serde(deserialize_with = "de_address")]
    pub address: Address,
    pub class: String,
    pub title: String,
    pub initial_class: String,
    pub workspace: WorkspaceRef,
    pub monitor: i32,
    pub pid: i32,
    pub floating: bool,
    pub hidden: bool,
    pub mapped: bool,
    pub fullscreen: i32,
    pub at: (i32, i32),
    pub size: (i32, i32),
    /// Lower means more recently focused; 0 is the active window.
    #[serde(default)]
    pub focus_history_id: i32,
}

impl Client {
    /// Whether this client sits on a special (scratchpad) workspace.
    pub fn is_special(&self) -> bool {
        self.workspace.name.starts_with("special")
    }

    /// The best identifier to match against a desktop entry.
    pub fn match_key(&self) -> &str {
        if self.initial_class.is_empty() {
            &self.class
        } else {
            &self.initial_class
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: i32,
    pub name: String,
    pub monitor: String,
    #[serde(default)]
    pub monitor_id: i32,
    pub windows: i32,
}

impl Workspace {
    pub fn is_special(&self) -> bool {
        self.name.starts_with("special")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Monitor {
    pub id: i32,
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub x: i32,
    pub y: i32,
    pub scale: f32,
    pub focused: bool,
    pub active_workspace: WorkspaceRef,
    /// Name is empty when no special workspace is open on this monitor.
    #[serde(default)]
    pub special_workspace: Option<WorkspaceRef>,
}

fn de_address<'de, D>(d: D) -> Result<Address, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    Ok(Address::parse(&s))
}
