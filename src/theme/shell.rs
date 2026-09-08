//! Omarchy shell design tokens: the active theme's `shell.toml`.
//!
//! `colors.toml` is only the palette. `shell.toml` is the *shape* of Omarchy's
//! UI — the exact background alpha of a popup, the fill behind a hovered row,
//! the border every card draws, the spacing and type scale everything derives
//! from. The bar, the menu, notifications and the lock screen all read it, so
//! a dock that reads it too stops looking like a different program.
//!
//! Every value has a fallback derived from the palette, because a theme need
//! not ship the file at all: Omarchy generates it, but a hand-written or older
//! theme may not have one.
//!
//! Two details are not in this file and are read from elsewhere:
//! corner radius, which the shell mirrors from Hyprland's
//! `decoration:rounding`, and the bar's position, which lives in `shell.json`.

use std::collections::HashMap;
use std::path::Path;

use super::{Palette, Rgb};

/// A border, which Omarchy allows to be either a flat colour or a Hyprland
/// gradient (`"rgba(798186ee) rgba(caccccee) 45deg"`).
///
/// Cards conventionally use the same gradient as Hyprland's active window
/// border, which is what visually ties a surface to the focused window.
#[derive(Debug, Clone, PartialEq)]
pub struct Border {
    /// Colour stops with their own alpha, in order. Never empty.
    pub stops: Vec<(Rgb, f64)>,
    pub angle: f64,
}

impl Border {
    pub fn solid(color: Rgb) -> Self {
        Self { stops: vec![(color, 1.0)], angle: 0.0 }
    }

    pub fn is_gradient(&self) -> bool {
        self.stops.len() > 1
    }

    /// The single colour to use where a gradient cannot be drawn.
    ///
    /// Averaging rather than taking the first stop: Omarchy's border gradients
    /// run from a muted tone to a bright one, and either end alone misreads
    /// the surface's weight.
    pub fn flatten(&self) -> (Rgb, f64) {
        let n = self.stops.len() as f64;
        let sum = self.stops.iter().fold((0.0, 0.0, 0.0, 0.0), |acc, (c, a)| {
            (acc.0 + c.r as f64, acc.1 + c.g as f64, acc.2 + c.b as f64, acc.3 + a)
        });
        (
            Rgb {
                r: (sum.0 / n).round() as u8,
                g: (sum.1 / n).round() as u8,
                b: (sum.2 / n).round() as u8,
            },
            sum.3 / n,
        )
    }

    /// CSS `linear-gradient(...)`, for use as a `border-image-source`.
    pub fn to_css_gradient(&self, alpha: f64) -> String {
        let stops: Vec<String> = self
            .stops
            .iter()
            .map(|(c, a)| c.with_alpha(a * alpha))
            .collect();
        // GTK measures a linear-gradient angle clockwise from "to top", which
        // is what Hyprland's degrees mean too.
        format!("linear-gradient({}deg, {})", self.angle.round(), stops.join(", "))
    }

    /// CSS colour for the flattened form.
    pub fn to_css_solid(&self, alpha: f64) -> String {
        let (c, a) = self.flatten();
        c.with_alpha(a * alpha)
    }
}

/// A card-like surface: popups, tooltips, menus, notifications.
#[derive(Debug, Clone, PartialEq)]
pub struct Surface {
    pub background: Rgb,
    pub background_alpha: f64,
    pub text: Rgb,
    pub border: Border,
    pub border_alpha: f64,
    pub border_width: f64,
}

impl Surface {
    /// The background as a CSS colour, alpha applied.
    pub fn background_css(&self) -> String {
        self.background.with_alpha(self.background_alpha)
    }
}

/// The `[menu]` surface, which adds the selected-row treatment that every
/// Omarchy list uses.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuTokens {
    pub surface: Surface,
    pub selected_background: Rgb,
    pub selected_background_alpha: f64,
    pub selected_text: Rgb,
    pub selected_border: Border,
    pub selected_border_alpha: f64,
}

/// `[controls]`: the shared interaction states every Omarchy control paints.
#[derive(Debug, Clone, PartialEq)]
pub struct Controls {
    pub color: Rgb,
    pub normal_fill_alpha: f64,
    pub normal_border_alpha: f64,
    pub normal_border_width: f64,
    pub hover_fill_alpha: f64,
    pub hover_border_alpha: f64,
    pub hover_border_width: f64,
    pub selected_fill_alpha: f64,
    pub selected_border_alpha: f64,
    pub pressed_fill_alpha: f64,
}

/// `[spacing]` and `[font]`, which together set the shell's overall scale.
///
/// The shell computes `fontScale = base_size / 12` and, when
/// `spacing.scale_with_font` is set, folds it into the spacing scale — so
/// `omarchy display text size` resizes every surface at once. The dock uses
/// the same product so it grows and shrinks with the bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Metrics {
    pub spacing_scale: f64,
    pub spacing_scale_with_font: bool,
    pub font_base_size: f64,
    /// Named `[spacing]` overrides, in px, before scaling.
    spacing_tokens: HashMap<String, f64>,
    /// Named `[font]` overrides, in px.
    font_tokens: HashMap<String, f64>,
}

impl Metrics {
    /// `base-size / 12`, the shell's own font scale.
    pub fn font_scale(&self) -> f64 {
        (self.font_base_size / 12.0).max(1.0 / 12.0)
    }

    /// The multiplier the shell applies to every spacing token.
    pub fn spacing_factor(&self) -> f64 {
        self.spacing_scale * if self.spacing_scale_with_font { self.font_scale() } else { 1.0 }
    }

    /// A `[spacing]` token in px, scaled — e.g. `space("row-padding-x", 12.0)`.
    pub fn space(&self, key: &str, fallback: f64) -> f64 {
        let base = self.spacing_tokens.get(key).copied().unwrap_or(fallback);
        (base * self.spacing_factor()).round()
    }

    /// A `[font]` token in px. Sizes derive from `base-size` unless the theme
    /// pinned that specific token.
    pub fn font(&self, key: &str, ratio: f64) -> f64 {
        self.font_tokens
            .get(key)
            .copied()
            .unwrap_or_else(|| (self.font_base_size * ratio).round())
            .max(1.0)
    }
}

/// `[bar]`: used to keep the dock the same weight as the bar it faces.
#[derive(Debug, Clone, PartialEq)]
pub struct BarTokens {
    pub background: Rgb,
    pub background_alpha: f64,
    pub text: Rgb,
    pub active: Rgb,
    pub scale_with_font: bool,
    pub size_horizontal: f64,
    pub size_vertical: f64,
}

/// Every token the dock consumes from the active theme's `shell.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct Shell {
    pub bar: BarTokens,
    pub popups: Surface,
    pub tooltip: Surface,
    pub menu: MenuTokens,
    pub notifications: Surface,
    pub controls: Controls,
    pub metrics: Metrics,
}

impl Shell {
    /// Tokens derived from the palette alone, matching the shell's own
    /// defaults. Used when a theme ships no `shell.toml`.
    pub fn fallback(palette: &Palette) -> Self {
        let bg = palette.background();
        let fg = palette.foreground();
        let border = Border::solid(fg);

        let surface = |alpha: f64| Surface {
            background: bg,
            background_alpha: alpha,
            text: fg,
            border: border.clone(),
            border_alpha: 1.0,
            border_width: 1.0,
        };

        Self {
            bar: BarTokens {
                background: bg,
                background_alpha: 1.0,
                text: fg,
                active: palette.accent(),
                scale_with_font: true,
                size_horizontal: 26.0,
                size_vertical: 28.0,
            },
            popups: surface(1.0),
            tooltip: surface(0.97),
            menu: MenuTokens {
                surface: surface(1.0),
                selected_background: fg,
                selected_background_alpha: 0.08,
                selected_text: fg,
                selected_border: border.clone(),
                selected_border_alpha: 0.25,
            },
            notifications: surface(1.0),
            controls: Controls {
                color: fg,
                normal_fill_alpha: 0.04,
                normal_border_alpha: 0.4,
                normal_border_width: 1.0,
                hover_fill_alpha: 0.08,
                hover_border_alpha: 0.25,
                hover_border_width: 1.0,
                selected_fill_alpha: 0.18,
                selected_border_alpha: 1.0,
                pressed_fill_alpha: 0.22,
            },
            metrics: Metrics {
                spacing_scale: 1.0,
                spacing_scale_with_font: true,
                font_base_size: 12.0,
                spacing_tokens: HashMap::new(),
                font_tokens: HashMap::new(),
            },
        }
    }

    /// Parse `<theme-dir>/shell.toml`, falling back per-key on anything the
    /// theme omits.
    pub fn load(dir: &Path, palette: &Palette) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join("shell.toml")).ok()?;
        let table: toml::Table = text.parse().ok()?;
        Some(Self::from_table(&table, palette))
    }

    /// The active theme's tokens, or palette-derived defaults.
    pub fn current(palette: &Palette) -> Self {
        super::current_theme_dir()
            .and_then(|d| Shell::load(&d, palette))
            .unwrap_or_else(|| {
                tracing::debug!("theme ships no shell.toml; deriving tokens from the palette");
                Shell::fallback(palette)
            })
    }

    fn from_table(table: &toml::Table, palette: &Palette) -> Self {
        let base = Self::fallback(palette);
        let hypr = table.get("hyprland").and_then(|v| v.as_table());
        let sec = |name: &str| table.get(name).and_then(|v| v.as_table()).cloned();

        let surface = |name: &str, fb: &Surface| -> Surface {
            let Some(t) = sec(name) else { return fb.clone() };
            Surface {
                background: color(&t, "background", fb.background),
                background_alpha: num(&t, "background-alpha", fb.background_alpha),
                text: color(&t, "text", fb.text),
                border: border(&t, "border", hypr, &fb.border),
                border_alpha: num(&t, "border-alpha", fb.border_alpha),
                border_width: num(&t, "border-width", fb.border_width),
            }
        };

        let popups = surface("popups", &base.popups);
        let tooltip = surface("tooltip", &base.tooltip);
        let notifications = surface("notifications", &base.notifications);
        let menu_surface = surface("menu", &base.menu.surface);

        let menu = match sec("menu") {
            Some(t) => MenuTokens {
                selected_background: color(
                    &t,
                    "selected-background",
                    base.menu.selected_background,
                ),
                selected_background_alpha: num(
                    &t,
                    "selected-background-alpha",
                    base.menu.selected_background_alpha,
                ),
                selected_text: color(&t, "selected-text", base.menu.selected_text),
                selected_border: border(
                    &t,
                    "selected-border",
                    hypr,
                    &base.menu.selected_border,
                ),
                selected_border_alpha: num(
                    &t,
                    "selected-border-alpha",
                    base.menu.selected_border_alpha,
                ),
                surface: menu_surface,
            },
            None => MenuTokens { surface: menu_surface, ..base.menu.clone() },
        };

        let controls = match sec("controls") {
            Some(t) => {
                let c = &base.controls;
                let normal_width = num(&t, "normal-border-width", c.normal_border_width);
                Controls {
                    color: color(&t, "normal-color", c.color),
                    normal_fill_alpha: num(&t, "normal-fill-alpha", c.normal_fill_alpha),
                    normal_border_alpha: num(&t, "normal-border-alpha", c.normal_border_alpha),
                    normal_border_width: normal_width,
                    hover_fill_alpha: num(&t, "hover-cursor-fill-alpha", c.hover_fill_alpha),
                    hover_border_alpha: num(
                        &t,
                        "hover-cursor-border-alpha",
                        c.hover_border_alpha,
                    ),
                    // The shell defaults this to the normal width, not its own.
                    hover_border_width: num(&t, "hover-cursor-border-width", normal_width),
                    selected_fill_alpha: num(&t, "selected-fill-alpha", c.selected_fill_alpha),
                    selected_border_alpha: num(
                        &t,
                        "selected-border-alpha",
                        c.selected_border_alpha,
                    ),
                    pressed_fill_alpha: num(&t, "pressed-fill-alpha", c.pressed_fill_alpha),
                }
            }
            None => base.controls.clone(),
        };

        let bar = match sec("bar") {
            Some(t) => BarTokens {
                background: color(&t, "background", base.bar.background),
                background_alpha: num(&t, "background-alpha", base.bar.background_alpha),
                text: color(&t, "text", base.bar.text),
                active: color(&t, "active", base.bar.active),
                scale_with_font: flag(&t, "scale-with-font", base.bar.scale_with_font),
                size_horizontal: num(&t, "size-horizontal", base.bar.size_horizontal),
                size_vertical: num(&t, "size-vertical", base.bar.size_vertical),
            },
            None => base.bar.clone(),
        };

        let spacing = sec("spacing");
        let font = sec("font");
        let metrics = Metrics {
            spacing_scale: spacing
                .as_ref()
                .map_or(1.0, |t| num(t, "scale", 1.0)),
            spacing_scale_with_font: spacing
                .as_ref()
                .map_or(true, |t| flag(t, "scale-with-font", true)),
            font_base_size: font.as_ref().map_or(12.0, |t| num(t, "base-size", 12.0)),
            // Every remaining numeric key is a per-token pixel override. They
            // are commented out in the generated file, so this is usually
            // empty — but a theme that pins one must be honoured.
            spacing_tokens: numeric_keys(spacing.as_ref(), &["scale"]),
            font_tokens: numeric_keys(font.as_ref(), &["base-size"]),
        };

        Self { bar, popups, tooltip, menu, notifications, controls, metrics }
    }
}

/// Collect every numeric key except the named structural ones.
fn numeric_keys(t: Option<&toml::Table>, skip: &[&str]) -> HashMap<String, f64> {
    let Some(t) = t else { return HashMap::new() };
    t.iter()
        .filter(|(k, _)| !skip.contains(&k.as_str()))
        .filter_map(|(k, v)| Some((k.clone(), as_f64(v)?)))
        .collect()
}

fn as_f64(v: &toml::Value) -> Option<f64> {
    v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
}

fn num(t: &toml::Table, key: &str, fallback: f64) -> f64 {
    t.get(key).and_then(as_f64).unwrap_or(fallback)
}

fn flag(t: &toml::Table, key: &str, fallback: bool) -> bool {
    t.get(key).and_then(|v| v.as_bool()).unwrap_or(fallback)
}

fn color(t: &toml::Table, key: &str, fallback: Rgb) -> Rgb {
    t.get(key)
        .and_then(|v| v.as_str())
        .and_then(Rgb::parse)
        .unwrap_or(fallback)
}

/// Resolve a border value, which may name a shared `[hyprland]` token.
fn border(
    t: &toml::Table,
    key: &str,
    hypr: Option<&toml::Table>,
    fallback: &Border,
) -> Border {
    let Some(raw) = t.get(key).and_then(|v| v.as_str()) else {
        return fallback.clone();
    };

    // `border = "hyprland.active-border"` points at the shared token block, so
    // every surface stays aligned with the window border gradient.
    let raw = match raw.strip_prefix("hyprland.") {
        Some(name) => match hypr.and_then(|h| h.get(name)).and_then(|v| v.as_str()) {
            Some(v) => v,
            None => return fallback.clone(),
        },
        None => raw,
    };

    parse_border(raw).unwrap_or_else(|| fallback.clone())
}

/// Parse a flat colour or a Hyprland gradient: stops, then an optional angle.
fn parse_border(raw: &str) -> Option<Border> {
    let mut stops = Vec::new();
    let mut angle = 0.0;

    for token in raw.split_whitespace() {
        if let Some(deg) = token.strip_suffix("deg") {
            if let Ok(v) = deg.parse::<f64>() {
                angle = v;
            }
            continue;
        }
        if let Some(stop) = Rgb::parse_rgba(token) {
            stops.push(stop);
        }
    }

    // Hyprland writes `rgba(rrggbbaa)` with no spaces inside the parens, so
    // whitespace splitting is safe; but bail rather than emit an empty border.
    (!stops.is_empty()).then_some(Border { stops, angle })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette::default()
    }

    #[test]
    fn a_border_reference_resolves_through_the_hyprland_block() {
        let toml = r##"
[hyprland]
active-border = "rgba(798186ee) rgba(caccccee) 45deg"

[popups]
border = "hyprland.active-border"
"##;
        let shell = Shell::from_table(&toml.parse().unwrap(), &palette());
        let b = &shell.popups.border;
        assert!(b.is_gradient());
        assert_eq!(b.angle, 45.0);
        assert_eq!(b.stops.len(), 2);
        assert_eq!(b.stops[0].0, Rgb { r: 0x79, g: 0x81, b: 0x86 });
        // `ee` is 238/255.
        assert!((b.stops[0].1 - 238.0 / 255.0).abs() < 0.01);
    }

    #[test]
    fn an_unresolvable_reference_keeps_the_fallback() {
        // No [hyprland] block: the surface must not end up with no border.
        let toml = r##"
[popups]
border = "hyprland.active-border"
"##;
        let shell = Shell::from_table(&toml.parse().unwrap(), &palette());
        assert_eq!(shell.popups.border, Shell::fallback(&palette()).popups.border);
    }

    #[test]
    fn a_flat_border_colour_parses_as_a_single_stop() {
        let toml = r##"
[menu]
selected-border = "#cacccc"
"##;
        let shell = Shell::from_table(&toml.parse().unwrap(), &palette());
        assert!(!shell.menu.selected_border.is_gradient());
        assert_eq!(
            shell.menu.selected_border.stops[0].0,
            Rgb { r: 0xca, g: 0xcc, b: 0xcc }
        );
    }

    #[test]
    fn missing_sections_fall_back_per_key() {
        // Only background is given; alpha and text must keep their defaults
        // rather than collapsing to zero.
        let toml = r##"
[popups]
background = "#101315"
"##;
        let shell = Shell::from_table(&toml.parse().unwrap(), &palette());
        let fb = Shell::fallback(&palette());
        assert_eq!(shell.popups.background, Rgb { r: 0x10, g: 0x13, b: 0x15 });
        assert_eq!(shell.popups.background_alpha, fb.popups.background_alpha);
        assert_eq!(shell.popups.text, fb.popups.text);
    }

    #[test]
    fn hover_border_width_defaults_to_the_normal_width() {
        // The shell derives it that way, so a theme that sets only the normal
        // width must not get a 1px hover border by accident.
        let toml = r##"
[controls]
normal-border-width = 2
"##;
        let shell = Shell::from_table(&toml.parse().unwrap(), &palette());
        assert_eq!(shell.controls.hover_border_width, 2.0);
    }

    #[test]
    fn the_scale_folds_the_font_size_in_the_way_the_shell_does() {
        let toml = r##"
[spacing]
scale = 2.0
scale-with-font = true

[font]
base-size = 24
"##;
        let shell = Shell::from_table(&toml.parse().unwrap(), &palette());
        // fontScale = 24/12 = 2, so the effective spacing factor is 4.
        assert_eq!(shell.metrics.font_scale(), 2.0);
        assert_eq!(shell.metrics.spacing_factor(), 4.0);
        assert_eq!(shell.metrics.space("row-padding-x", 12.0), 48.0);

        // Turning it off leaves spacing on its own scale.
        let toml = toml.replace("scale-with-font = true", "scale-with-font = false");
        let shell = Shell::from_table(&toml.parse().unwrap(), &palette());
        assert_eq!(shell.metrics.spacing_factor(), 2.0);
    }

    #[test]
    fn a_pinned_token_overrides_the_scale_but_still_scales() {
        let toml = r##"
[spacing]
scale = 1.0
row-padding-x = 20

[font]
base-size = 12
body = 17
"##;
        let shell = Shell::from_table(&toml.parse().unwrap(), &palette());
        assert_eq!(shell.metrics.space("row-padding-x", 12.0), 20.0);
        assert_eq!(shell.metrics.font("body", 1.0), 17.0);
        // Unpinned tokens still derive from base-size.
        assert_eq!(shell.metrics.font("heading", 1.333), 16.0);
    }

    #[test]
    fn a_gradient_flattens_to_its_average_rather_than_an_endpoint() {
        let b = parse_border("rgba(000000ff) rgba(ffffffff)").unwrap();
        let (c, a) = b.flatten();
        assert_eq!(c, Rgb { r: 128, g: 128, b: 128 });
        assert_eq!(a, 1.0);
    }
}
