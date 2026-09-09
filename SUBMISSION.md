# Marketplace submission — omarchyplugins.com

**Draft.** Not part of the plugin. This is the submission form filled in ahead of
time so it can be reviewed before it is sent. Field values below are the ones
the form actually offers.

Form: <https://github.com/HANCORE-linux/omarchy-plugin-marketplace/issues/new?template=submit-plugin.yml>

---

**Repository URL** (required)

```
https://github.com/szlukabence/omarchy-dock
```

**Category** (required, pick one of: Appearance, Desktop, Developer Tools,
Hardware, Kids, Productivity, System, Widgets, Other)

```
Desktop
```

Rationale: it is a piece of desktop furniture. *Appearance* is the runner-up
because the dock is drawn from the theme's own tokens, but that category reads
as theming rather than a thing you use; *Productivity* overclaims.

**Tags** (required, max 3 — more than three is an automatic rejection)

```
Launcher, Hyprland, Workspaces
```

There is no `Dock` tag. `Launcher` and `Hyprland` are certain: it launches and
focuses applications, and it drives the compositor directly over its socket.
`Workspaces` covers the workspace strip and the drop-to-send gesture — swap it
for `System` if the tray host matters more to a reviewer. `Quickshell` would be
misleading, since the plugin is a supervisor and the dock itself is GTK4, not
QML; `Bar` would be wrong, as this does not touch the bar.

**Maintainer notes** (optional)

```
Supervisor plugin for omarchy-dock, a GTK4 layer-shell dock (MIT, same author).
The plugin is not the dock: it starts and stops the binary so the dock can be
enabled and disabled like any other component. The clone this plugin arrives in
is the full source tree, so it can build the binary it supervises:

  cd ~/.config/omarchy/plugins/omarchy-dock && ./install.sh

That runs makepkg, so pacman owns the binaries. If the binary is missing, the
plugin sends a notification naming that command rather than failing with
"command not found". An AUR package is prepared in packaging/aur/ and will
replace this step once it is published.

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
| 1 | The repository is public and contains installation and removal instructions | Yes — README has `omarchy plugin add`, the AUR package, and a Removal section covering all four pieces |
| 2 | I have documented the plugin license and any external dependencies | Yes — MIT in `LICENSE`; the Requirements table names every dependency and its license |
| 3 | I own or have permission to submit this plugin and its preview assets | Yes — sole author; the preview image is a screenshot of this dock on the author's own machine |
| 4 | The plugin does not overwrite user configuration without explicit consent | Yes. **The plugin submitted here writes nothing at all** — it only starts and stops a binary. The dock's own `omarchy-dockctl install` writes the files listed below, and only when a user runs it; it also refuses to touch the plugin directory when that directory is a git checkout, so a marketplace install is never overwritten |
| 5 | Approval is for listing and is not a security review | Understood |

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
