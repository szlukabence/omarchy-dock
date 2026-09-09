# omarchy-dock

A dock for [Omarchy](https://omarchy.org/) that looks like it came with it.

Rust + GTK4 on `gtk4-layer-shell`, talking to Hyprland directly. It reads the
active Omarchy theme's own design tokens, so it is drawn with the same
background, borders, hover fills, corner radius and type scale as the bar and
the menus — rather than being a dock that merely runs on the same desktop.

![The dock on Omarchy](screenshot.png)

## Install

Two pieces, because that is how Omarchy distributes a plugin with a binary
behind it:

```bash
omarchy pkg aur add omarchy-dock-bin                                    # the dock
omarchy plugin add https://github.com/szlukabence/Omarchy-dock.git --enable   # the supervisor
```

`omarchy plugin add` only *clones* a repository — it never builds or runs
anything — so the plugin cannot compile a Rust project. The plugin is what
starts and stops the dock and puts it in `omarchy menu plugin`; the package is
the dock itself. Install the plugin alone and it tells you which package is
missing rather than failing with "command not found".

Then wire it into the rest of Omarchy:

```bash
omarchy-dockctl install     # theme-set hook and menu entries
```

### From source

```bash
cd packaging/local && makepkg -si
omarchy-dockctl install
```

`makepkg -p` takes a *filename in the current directory*, not a path, so running
it from the repository root fails with "must be in the current working
directory". `packaging/aur/` fetches a released tarball and is for publishing;
`packaging/local/` builds the tree it sits in.

### Removal

```bash
omarchy-dockctl uninstall              # hook, menu entries, shell.json entry
omarchy plugin remove omarchy-dock     # the plugin checkout
omarchy pkg remove omarchy-dock-bin    # the binaries
rm -rf ~/.config/omarchy-dock          # your settings, if you want them gone
```

`uninstall` reverses everything `install` did and nothing else. It will not
delete a git-managed plugin checkout — that is `omarchy plugin remove`'s job.

## What it does

- **Pinned and running apps**, with running indicators, window-count badges and
  urgency. Click to focus, click again to cycle windows, click an idle icon to
  launch.
- **Drag to reorder**, with the icons parting to show where the drop lands.
  User-placed dividers drag too.
- **Folder stacks and Trash**, drawn as monochrome glyphs so only real
  applications carry colour — the way the bar draws its widgets.
- **A workspace strip and scratchpad tile**, styled like the bar's own. Click a
  tile to switch; drop an app icon on one to send that window there.
- **Command tiles** — a glyph, a label and a shell command, exactly how Omarchy
  menu rows are defined, so anything reachable from `omarchy` can be a tile.
- **The system tray**, hosted in the dock instead of the bar if you prefer.
- **Intelligent auto-hide** that also gets out of the way of fullscreen windows
  and screen recordings.
- **Live theming**: change theme, font size or Hyprland's rounding and the dock
  follows without a restart.

## Requirements

| | | |
| --- | --- | --- |
| [GTK4](https://gitlab.gnome.org/GNOME/gtk) | LGPL-2.1 | the toolkit |
| [gtk4-layer-shell](https://github.com/wmww/gtk4-layer-shell) | MIT | anchoring the surface to a screen edge |
| [Hyprland](https://hypr.land/) | BSD-3-Clause | window state, workspaces, dispatching |
| [Omarchy](https://omarchy.org/) | MIT | theme tokens, menu, notifications, plugin host |
| A Nerd Font | varies | glyphs for the launcher, stacks and Trash |

Only GTK4 and gtk4-layer-shell are hard requirements. Without Hyprland the dock
shows pinned apps but knows nothing about windows; without Omarchy it falls back
to a palette of its own. Nothing here reaches the network.

## What it writes

Everything the dock changes outside its own `~/.config/omarchy-dock/`, and only
ever in response to an explicit action:

| Path | When | What |
| --- | --- | --- |
| `~/.config/omarchy/hooks/theme-set.d/omarchy-dock` | `dockctl install` | A hook that restyles the dock after a theme change |
| `~/.config/omarchy/plugins/omarchy-dock/` | `dockctl install` | The supervisor plugin — skipped if it is a git checkout |
| `~/.config/omarchy/shell.json` | `dockctl install` | One entry in `plugins[]`, which is how the shell records a plugin as enabled |
| `~/.config/omarchy/extensions/omarchy-menu.jsonc` | `dockctl install` | A block between markers, spliced in rather than rewriting the file |
| `~/.config/hypr/looknfeel.lua` | `dockctl install --blur` **only** | Global blur plus a layer rule, needed only by `theme.style = "glass"` |

`uninstall` removes all of it. Nothing is written on start, on poll, or on
open. There is no `sudo` anywhere, and no network access. `/tmp` is read once —
Omarchy's own screen-recording marker — and never written.

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
omarchy-dockctl install --blur   # ...and Hyprland blur, for style = "glass"
omarchy-dockctl status
omarchy-dockctl uninstall
```

| Piece | What it gives you |
| --- | --- |
| `theme-set` hook | Theme changes reach the dock the moment Omarchy finishes applying them, rather than whenever an inotify watch fires — so it can never restyle from a half-written theme |
| Shell plugin | The dock appears in `omarchy menu plugin`; enabling starts it, disabling stops it, and it autostarts with the shell. Skipped when the plugin is already a git checkout, so `omarchy plugin update` keeps working |
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
| `items.commands` | Command tiles: `id`, `label`, `glyph`, `command`. Pin one by putting `cmd:<id>` in `pinned` |
| `workspaces.enabled`, `.show_empty`, `.scratchpad` | Workspace strip and scratchpad tile (both off by default — the bar already has workspaces) |
| `tray.enabled`, `.show_passive` | Host the system tray in the dock (off by default — the bar already has one) |
| `autohide.hide_on_fullscreen`, `.hide_while_recording` | Get out of the way of fullscreen windows and screen recordings, whatever `mode` says |
| `dock.spacing` | Gap between icons; omit for automatic (derived from hover zoom) |
| `dock.tooltip_delay_ms` | Delay before a hovered icon's name appears |
| `theme.style` | `omarchy` (default) \| `glass` |
| `theme.follow_shell_scale` | Track the shell's spacing/font scale (default: on) |
| `theme.radius` | Corner radius; omit to mirror Hyprland's `decoration:rounding` |
| `theme.user_css` | Extra CSS layered over the generated stylesheet |
| `items.glyph_ui` | Draw launcher, folders and Trash as monochrome glyphs (default: on) |
| `dock.avoid_bar` | Offset past the Omarchy bar when both share an edge (default: on) |

The dock can host the **system tray** itself: it registers as a
StatusNotifierItem host alongside the shell's, so items appear in both until
you turn off the bar's `omarchy.tray` widget. Clicks go straight to the
application — left activates, middle is the secondary action, right asks the
app to post its own menu, which belongs to it rather than to the dock.

`ContextMenu` is optional in the protocol, though: an item that serves its menu
only over DBusMenu — cc-switch, for one — will not respond to a right click,
and the dock logs that rather than pretending otherwise. Drawing those menus
means implementing DBusMenu, which is a whole protocol rather than a fallback.

Workspace tiles switch on click; dropping an app icon on one sends that
window there, and dropping on the scratchpad tile stashes it. A Chromium web
app with no `.desktop` file can still be pinned — its window class encodes its
URL, so the dock reconstructs `omarchy launch or focus webapp` for it.

Put `"---"` anywhere in `items.pinned` to insert a divider, or add one from the
settings panel. Drag pinned icons and dividers to reorder them — the dock parts to show where
the drop will land; right-click a divider to remove it. A divider is also
added automatically between pinned apps and running-but-unpinned ones, and
before Trash; dividers that would end up at either end are dropped.

Right-click the launcher for a short menu: the Omarchy surfaces the dock can
raise, and "Settings…", which opens a proper window (also `omarchy-dockctl
settings`, or `Dock > Settings` on the Omarchy menu). Everything writes
`config.toml`, so the window, a hand-edited file, and `omarchy-dockctl` all take
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

## Development

```bash
cargo build --release
cargo test
cargo clippy --all-targets
```

`cargo build` alone does not change what runs if you installed the package —
rebuild it with `cd packaging/local && makepkg -f` and reinstall.

`src/bin/spike_magnify.rs` is the Phase-0 de-risking spike for hover
magnification, kept because it is the cheapest way to measure frame timing in
isolation. It is not shipped by either PKGBUILD.

## Notes on Hyprland 0.56

The IPC protocol is split. **Dispatch** is Lua
(`hl.dsp.focus({ window = 'address:0x…' })`); the classic
`/dispatch focuswindow address:…` form is gone. The **event stream** still uses
the old `name>>payload` text format. The two also disagree on addresses —
events omit the `0x` that JSON includes.

Sockets live in `$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE/`, not
`/tmp/hypr/`.

## License

MIT. See [LICENSE](LICENSE).
