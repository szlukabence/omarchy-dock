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

## Blur

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
| `magnify.zoom`, `.lift`, `.stiffness`, `.damping_ratio` | Hover feel. Only the hovered icon scales |
| `autohide.mode` | `never` \| `intelligent` \| `always` (default: `intelligent`) |
| `launcher.enabled`, `.icon`, `.command` | Omarchy menu button at the head of the dock; empty command runs `omarchy-menu toggle` |
| `monitors.mode` | `all` \| `primary` \| `focused` |
| `items.folders` | Folder stacks, each with its own `enabled` flag; seeded from omadock's `pinnedFolders` |
| `dock.spacing` | Gap between icons; omit for automatic (derived from hover zoom) |
| `dock.tooltip_delay_ms` | Delay before a hovered icon's name appears |
| `theme.user_css` | Extra CSS layered over the generated stylesheet |

Put `"---"` anywhere in `items.pinned` to insert a divider; right-click one to
move or remove it. A divider is also
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
omarchy-dockctl reveal | hide | reload
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
