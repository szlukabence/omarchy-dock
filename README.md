# omarchy-dock

A fast, modern dock for [Omarchy](https://omarchy.org/) (Hyprland / Arch), in
Rust + GTK4 with `gtk4-layer-shell`.

## Status

Working: launcher button, separators, folder stacks and Trash, layer-shell
surface with Hyprland blur, single-icon hover
magnification, live theming from the active Omarchy palette, Hyprland IPC,
window/app matching with running indicators and window-count badges,
click-to-focus/cycle/launch, context menus, auto-hide, multi-monitor, and a
control socket for hotkeys.

Not yet: drag-and-drop, and the status widgets (clock, battery, network,
MPRIS).

## Build

```bash
cargo build --release
./target/release/omarchy-dock
```

Requires GTK 4, `gtk4-layer-shell`, and a running Hyprland.

## Looking like Omarchy

By default the dock draws itself the way the Omarchy shell draws the bar, the
menu and notifications, by reading the active theme's `shell.toml` — the same
design tokens every other surface is built from:

- panel and tooltip from `[popups]` / `[tooltip]`, menus and settings from
  `[menu]` (including its selected-row treatment), hover from `[controls]`
- the border is the Hyprland active-border gradient the shell references
- corner radius mirrors Hyprland's `decoration:rounding`, which is where the
  shell gets its own — set rounding to 0 and the dock is square too
- sizes follow the shell's scale, so `omarchy display text size` resizes the
  dock along with the bar
- the dock's own furniture (launcher, folder stacks, Trash) is drawn as
  monochrome glyphs, so only real applications carry colour. The launcher uses
  the same Omarchy mark as the bar's menu widget.

Set `theme.style = "glass"` for the translucent rounded slab instead.

## Omarchy integration

```bash
omarchy-dockctl install     # theme-set hook, shell plugin, menu entries
omarchy-dockctl status
omarchy-dockctl uninstall
```

| Piece | What it gives you |
| --- | --- |
| `theme-set` hook | Theme changes reach the dock the moment Omarchy finishes applying them, rather than whenever an inotify watch fires — so it can never restyle from a half-written theme |
| Shell plugin | The dock appears in `omarchy menu plugin`; enabling starts it, disabling stops it, and it autostarts with the shell |
| Menu entries | `Dock` on the Omarchy menu and in its search: reveal, auto-hide, settings, reload, restart |

Everything lands in `~/.config/omarchy/` and is removed by `uninstall`. The
menu extension file is shared with you, so it is edited between markers and
never rewritten.

Beyond that the dock also opens the shell's real surfaces (right-click the
launcher: Omarchy menu, themes, backgrounds, clipboard, emojis) rather than
drawing its own, reports through Omarchy notifications, and offsets itself past
the bar when both want the same screen edge (`dock.avoid_bar`).

## Blur

Only needed for `theme.style = "glass"` — the Omarchy style is opaque.

Omarchy ships with blur **disabled globally**, and Hyprland's per-layer blur
does nothing until the blur subsystem is on. Add to `~/.config/hypr/looknfeel.lua`:

```lua
hl.config({ decoration = { blur = { enabled = true, size = 6, passes = 3 } } })
hl.layer_rule({ match = { namespace = "^omarchy-dock$" }, blur = true, ignore_alpha = 0.2 })
```

## Configuration

`~/.config/omarchy-dock/config.toml`, created on first run and **seeded from
Omarchy's own `dock.json`**, so switching from the stock dock keeps your pins.
Edits apply on save — no restart. Changing the theme with `omarchy theme set`
restyles the dock live.

Notable keys:

| Key | Meaning |
| --- | --- |
| `dock.position` | `bottom` \| `top` \| `left` \| `right` |
| `dock.icon_size`, `dock.spacing` | Sizing; spacing defaults to whatever keeps magnified icons from overlapping |
| `magnify.hover` | `fill` (default, the shell's own hover treatment) \| `scale` (dock magnification) \| `none` |
| `magnify.zoom`, `.lift`, `.stiffness`, `.damping_ratio` | Hover feel for `scale`. Only the hovered icon scales |
| `autohide.mode` | `never` \| `intelligent` \| `always` (default: `intelligent`) |
| `launcher.enabled`, `.icon`, `.command` | Omarchy menu button at the head of the dock; empty command runs `omarchy-menu toggle` |
| `monitors.mode` | `all` \| `primary` \| `focused` |
| `items.folders` | Folder stacks, each with its own `enabled` flag; seeded from omadock's `pinnedFolders` |
| `dock.spacing` | Gap between icons; omit for automatic (derived from hover zoom) |
| `dock.tooltip_delay_ms` | Delay before a hovered icon's name appears |
| `theme.style` | `omarchy` (default) \| `glass` |
| `theme.follow_shell_scale` | Track the shell's spacing/font scale (default: on) |
| `theme.radius` | Corner radius; omit to mirror Hyprland's `decoration:rounding` |
| `theme.user_css` | Extra CSS layered over the generated stylesheet |
| `items.glyph_ui` | Draw launcher, folders and Trash as monochrome glyphs (default: on) |
| `dock.avoid_bar` | Offset past the Omarchy bar when both share an edge (default: on) |

Put `"---"` anywhere in `items.pinned` to insert a divider, or add one from the
settings panel. Drag pinned icons and dividers to reorder them — the dock parts to show where
the drop will land; right-click a divider to remove it. A divider is also
added automatically between pinned apps and running-but-unpinned ones, and
before Trash; dividers that would end up at either end are dropped.

Right-click the launcher button for a settings panel covering position,
auto-hide, icon size, hover zoom, icon spacing, and per-folder, Trash and
running-app toggles. It writes
`config.toml`, so the panel, a hand-edited file, and `omarchy-dockctl` all take
the same path into the running dock.

`theme.user_css` can reference the live palette, e.g. `@omarchy_accent`,
`@dock_bg`, `@dock_fg`.

## Hotkeys

A layer-shell surface cannot grab global shortcuts — the compositor owns them.
So bind keys to `omarchy-dockctl`:

```bash
omarchy-dockctl activate 3        # focus / cycle / launch dock item 3
omarchy-dockctl toggle-autohide
omarchy-dockctl reveal | hide | reload | restyle
```

> **`SUPER + 1..9` is already taken.** Omarchy binds it to workspace switching
> (`/usr/share/omarchy/default/hypr/bindings/tiling.lua`). Using it for the dock
> means giving that up, so pick a free chord instead — for example:

```lua
-- ~/.config/hypr/bindings.lua
for i = 1, 9 do
  o.bind("SUPER + ALT + " .. i, "Dock item " .. i,
    hl.dsp.exec_cmd("omarchy-dockctl activate " .. i))
end
```

To use `SUPER + 1..9` anyway, unbind it first — and accept that workspace
switching moves elsewhere:

```lua
for i = 1, 9 do hl.unbind("SUPER + " .. i) end
```

## Notes on Hyprland 0.56

The IPC protocol is split. **Dispatch** is Lua
(`hl.dsp.focus({ window = 'address:0x…' })`); the classic
`/dispatch focuswindow address:…` form is gone. The **event stream** still uses
the old `name>>payload` text format. The two also disagree on addresses —
events omit the `0x` that JSON includes.

Sockets live in `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`, not
`/tmp/hypr/`.
