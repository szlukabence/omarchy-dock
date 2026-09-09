# Marketplace submission — omarchyplugins.com

**Draft.** Not part of the plugin. This is the submission form filled in ahead of
time so it can be reviewed before it is sent. Two fields are marked TODO: the
allowed values are defined by the form itself, which has not been read here.

Form: <https://github.com/HANCORE-linux/omarchy-plugin-marketplace/issues/new?template=submit-plugin.yml>

---

**Repository URL** (required)

```
https://github.com/szlukabence/Omarchy-dock
```

**Category** (required, pick one)

```
TODO — check the form's list.
```

Suggestion: whichever of *Productivity* / *Desktop* / *Utilities* exists. The
plugin's payoff is launching and switching applications, not system state.

**Tags** (required, max 3)

```
TODO — check the form's list. Suggested: Dock, Hyprland, Launcher
```

`Quickshell` would be misleading: the plugin is a supervisor, and the dock
itself is a GTK4 layer-shell process, not QML. `Bar` would be wrong too — this
does not touch the bar, though it can host the tray the bar would otherwise.

**Maintainer notes** (optional)

```
Supervisor plugin for omarchy-dock, a GTK4 layer-shell dock (MIT, same author).
The plugin is not the dock: it starts and stops the binary so the dock can be
enabled and disabled like any other component. Install the binary with:

  omarchy pkg aur add omarchy-dock-bin

If it is missing, the plugin sends a notification naming that package rather
than failing with "command not found".

The dock reads the active theme's shell.toml and draws itself with the same
tokens as the bar and menus — background, borders, hover fills, corner radius
(mirrored from Hyprland's decoration:rounding) and type scale — so it tracks
`omarchy theme set` and `omarchy display text size` without a restart.

What it runs: `omarchy-shell` IPC to raise the shell's own menu, theme picker,
background picker, clipboard and emoji surfaces rather than drawing lookalikes;
`omarchy notification send` for its own messages; `omarchy launch or focus
webapp` for web-app pins; and whatever Exec line a pinned .desktop entry
carries. Hyprland is driven over its own socket with Lua dispatchers, always
addressed to a specific window rather than "the focused one".

Every subprocess is spawned as an argument vector, never as a shell string,
except the plugin's own start/stop line and pinned .desktop Exec lines, which
are shell commands by definition. Values that reach a command line — a web-app
class and URL — are single-quoted.

No sudo. No network access. /tmp is read once, for Omarchy's own
screen-recording marker (/tmp/omarchy-screenrecord-filename), so the dock can
get out of a recording; it is never written.

It registers a second StatusNotifierItem host alongside the shell's, which the
protocol explicitly allows — items are broadcast to every host. Tray hosting is
off by default so items are not shown twice; turn off omarchy.tray to move them.

Verified on Omarchy 4.0.2, Hyprland 0.56.2. `omarchy plugin validate` exits 0
against a fresh clone.
```

**Submission checklist** (all five required)

| # | Item | Status |
|---|---|---|
| 1 | Repository is public with installation/removal instructions | Yes — README has `omarchy plugin add`, the AUR package, and a Removal section covering all four pieces |
| 2 | License and dependencies documented | Yes — MIT in `LICENSE`; the Requirements table names every dependency and its license |
| 3 | Ownership/permission confirmed | Yes — sole author; the preview image is a screenshot of this dock on the author's own machine |
| 4 | Plugin respects user configuration | Yes, with disclosure — see the table below and the "What it writes" section of the README |
| 5 | Approval is listing-only, not a security review | Understood |

## What it writes outside its own config

All of it only in response to `omarchy-dockctl install`, and all of it reversed
by `omarchy-dockctl uninstall`. Nothing is written on start, on poll, or on open.

| Path | What |
|---|---|
| `~/.config/omarchy/hooks/theme-set.d/omarchy-dock` | A hook that restyles the dock after a theme change |
| `~/.config/omarchy/plugins/omarchy-dock/` | The supervisor plugin — **skipped entirely when that directory is a git checkout**, so a marketplace install is never overwritten and `omarchy plugin update` keeps working |
| `~/.config/omarchy/shell.json` | One entry in `plugins[]`, which is how the shell records a plugin as enabled. No other key is touched |
| `~/.config/omarchy/extensions/omarchy-menu.jsonc` | A block between markers, spliced in rather than rewriting the file; removed cleanly |
| `~/.config/hypr/looknfeel.lua` | **Only** under `install --blur`, which is opt-in and never part of a plain install: global blur plus a layer rule, needed solely by `theme.style = "glass"`. Marker-fenced, and validated with `hyprctl configerrors` afterwards |

Its own settings live in `~/.config/omarchy-dock/config.toml`.

## Against the automated security baseline

| Flagged pattern | This plugin |
|---|---|
| Download-to-shell execution (`curl … \| sh`) | No network access at all |
| Unpinned external git sources | No git operations |
| Passwordless sudoers policies | No sudo anywhere |
| Privileged process control via shared `/tmp` state | One `/tmp` path is **read** — Omarchy's own screen-recording marker — to decide whether to hide. Never written, and nothing is executed from it |
