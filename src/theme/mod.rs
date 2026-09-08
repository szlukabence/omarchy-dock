//! Omarchy theme integration.
//!
//! Omarchy 4 has no CSS and no Pywal. The active theme is a symlink,
//! `~/.local/state/omarchy/current/theme`, pointing at a directory whose
//! `colors.toml` holds the palette. Theme key sets are *not* uniform — some
//! themes omit `orange`/`brown`, and some add Hyprland-specific entries whose
//! values are `rgba(798186ee)`, which is not valid CSS. So the palette is
//! parsed as a map with fallbacks rather than a fixed struct.

pub mod css;
pub mod shell;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Parse the colour forms that appear in `colors.toml`.
    ///
    /// Accepts `#rrggbb`, bare `rrggbb`, and Hyprland's `rgb(rrggbb)` /
    /// `rgba(rrggbbaa)`. The alpha of the latter is dropped: the dock controls
    /// its own opacity, and blending is the compositor's job.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let body = if let Some(rest) = s.strip_prefix("rgba(") {
            rest.strip_suffix(')')?
        } else if let Some(rest) = s.strip_prefix("rgb(") {
            rest.strip_suffix(')')?
        } else {
            s
        };
        let hex = body.trim().trim_start_matches('#');
        // Hyprland's rgba() carries 8 digits; take the leading rgb triple.
        if hex.len() < 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        Some(Self {
            r: u8::from_str_radix(&hex[0..2], 16).ok()?,
            g: u8::from_str_radix(&hex[2..4], 16).ok()?,
            b: u8::from_str_radix(&hex[4..6], 16).ok()?,
        })
    }

    /// Like `parse`, but keeps the alpha of a Hyprland `rgba(rrggbbaa)`.
    ///
    /// Border gradients carry their own per-stop alpha, which the shell then
    /// multiplies by the surface's `border-alpha`. Dropping it here would make
    /// every card's hairline read heavier than Omarchy draws it.
    pub fn parse_rgba(s: &str) -> Option<(Self, f64)> {
        let rgb = Self::parse(s)?;
        let body = s
            .trim()
            .strip_prefix("rgba(")
            .and_then(|r| r.strip_suffix(')'))
            .unwrap_or(s.trim());
        let hex = body.trim().trim_start_matches('#');
        let alpha = if hex.len() >= 8 {
            u8::from_str_radix(&hex[6..8], 16).map(|a| a as f64 / 255.0).unwrap_or(1.0)
        } else {
            1.0
        };
        Some((rgb, alpha))
    }

    pub fn to_css(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    pub fn with_alpha(self, a: f64) -> String {
        format!("rgba({}, {}, {}, {:.3})", self.r, self.g, self.b, a.clamp(0.0, 1.0))
    }

    /// Perceptual lightness, 0..1. Used to decide whether hairlines and
    /// shadows should lighten or darken against the panel.
    pub fn luminance(self) -> f64 {
        let f = |c: u8| {
            let c = c as f64 / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(self.r) + 0.7152 * f(self.g) + 0.0722 * f(self.b)
    }

    /// Blend towards `other` by `t` (0..1).
    pub fn mix(self, other: Rgb, t: f64) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let lerp = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
        Rgb { r: lerp(self.r, other.r), g: lerp(self.g, other.g), b: lerp(self.b, other.b) }
    }
}

#[derive(Debug, Clone)]
pub struct Palette {
    pub name: String,
    pub mode: Mode,
    colors: HashMap<String, Rgb>,
}

impl Default for Palette {
    /// A neutral dark palette, used when no Omarchy theme can be resolved.
    fn default() -> Self {
        let mut colors = HashMap::new();
        colors.insert("background".into(), Rgb { r: 0x14, g: 0x15, b: 0x1e });
        colors.insert("foreground".into(), Rgb { r: 0xc8, g: 0xcc, b: 0xd4 });
        colors.insert("accent".into(), Rgb { r: 0x7a, g: 0xa2, b: 0xf7 });
        Self { name: "fallback".into(), mode: Mode::Dark, colors }
    }
}

impl Palette {
    /// Look up a colour, trying each name in turn before falling back.
    pub fn get(&self, names: &[&str], fallback: Rgb) -> Rgb {
        names.iter().find_map(|n| self.colors.get(*n).copied()).unwrap_or(fallback)
    }

    pub fn background(&self) -> Rgb {
        self.get(&["background"], Rgb { r: 0x14, g: 0x15, b: 0x1e })
    }

    pub fn foreground(&self) -> Rgb {
        self.get(&["foreground"], Rgb { r: 0xc8, g: 0xcc, b: 0xd4 })
    }

    pub fn accent(&self) -> Rgb {
        self.get(&["accent", "blue"], self.foreground())
    }

    /// Colour for urgency/attention states. Themes may omit `orange`.
    pub fn urgent(&self) -> Rgb {
        self.get(&["bright_red", "red", "orange"], self.accent())
    }

    pub fn is_dark(&self) -> bool {
        self.mode == Mode::Dark
    }

    /// Every parsed colour, for emitting `@define-color` variables.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Rgb)> {
        self.colors.iter()
    }

    /// Parse `<dir>/colors.toml`.
    pub fn load(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join("colors.toml")).ok()?;
        let table: toml::Table = text.parse().ok()?;

        let mode = match table.get("mode").and_then(|v| v.as_str()) {
            Some("light") => Mode::Light,
            _ => Mode::Dark,
        };

        // Skip anything that is not a parseable colour, which drops the
        // Hyprland border entries that carry two space-separated values.
        let colors = table
            .iter()
            .filter(|(k, _)| *k != "mode")
            .filter_map(|(k, v)| Some((k.clone(), Rgb::parse(v.as_str()?)?)))
            .collect();

        Some(Self { name: theme_name().unwrap_or_else(|| "unknown".into()), mode, colors })
    }

    /// Load the currently active Omarchy theme, or the fallback palette.
    pub fn current() -> Self {
        current_theme_dir()
            .and_then(|d| Palette::load(&d))
            .unwrap_or_else(|| {
                tracing::warn!("no Omarchy theme found; using fallback palette");
                Palette::default()
            })
    }
}

/// The directory the `current/theme` symlink resolves to.
pub fn current_theme_dir() -> Option<PathBuf> {
    let link = state_dir().join("theme");
    std::fs::canonicalize(&link).ok()
}

/// Directory holding Omarchy's current-theme pointers. Watched for changes.
pub fn state_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".local/state/omarchy/current")
}

/// Human-readable name of the active theme, e.g. "Solitude".
pub fn theme_name() -> Option<String> {
    std::fs::read_to_string(state_dir().join("theme.name"))
        .ok()
        .map(|s| s.trim().to_owned())
}

/// The icon theme the active Omarchy theme requests, if any.
pub fn icon_theme() -> Option<String> {
    let dir = current_theme_dir()?;
    let name = std::fs::read_to_string(dir.join("icons.theme")).ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Hyprland's `decoration:rounding`, which the Omarchy shell mirrors as its
/// own corner radius.
///
/// Read straight off the request socket rather than by shelling out to
/// `hyprctl`: this runs on the GTK thread during a restyle, and spawning a
/// process there is both slower and one more thing to fail. Falls back to the
/// Hyprland default when anything goes wrong, including not running under
/// Hyprland at all.
pub fn hyprland_rounding() -> Option<f64> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    let path = crate::hypr::request_socket().ok()?;
    let mut stream = UnixStream::connect(path).ok()?;
    stream.write_all(b"j/getoption decoration:rounding").ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok();

    let mut body = String::new();
    stream.read_to_string(&mut body).ok()?;

    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    // `getoption` reports an int option under "int"; a missing option comes
    // back with `"set": false`, which is not a radius we should adopt.
    if v.get("set").and_then(|s| s.as_bool()) == Some(false) {
        return None;
    }
    v.get("int").and_then(|n| n.as_f64()).filter(|n| *n >= 0.0)
}
