import QtQuick
import Quickshell.Io

// Installed by omarchy-dock. Removed by `omarchy-dockctl uninstall`.
//
// omarchy-dock is a separate GTK4 layer-shell process rather than QML, so this
// plugin supervises it instead of drawing it: enabling the plugin starts the
// dock, disabling it stops the dock. That is what makes the dock appear in
// `omarchy menu plugin` and the plugin managers alongside every other
// component, and gives it an autostart tied to the shell's own lifecycle.
Item {
  id: root

  // Injected by omarchy-shell's service loader.
  property var shell: null

  // Start only if one is not already running: the user may have launched the
  // dock by hand, and two docks would fight over the control socket.
  //
  // The missing-binary case is handled explicitly rather than left to fail.
  // `omarchy plugin add` only clones a repo — it never builds anything — so a
  // plugin installed on its own has no binary behind it, and a bare
  // "command not found" at login says nothing about what to do next.
  Process {
    id: starter
    command: ["bash", "-lc",
      "if ! command -v omarchy-dock >/dev/null 2>&1; then " +
        "omarchy notification send --app-name Dock -u critical " +
        "'Dock is not installed' " +
        "'This plugin is only the supervisor. Install the dock with: omarchy pkg aur add omarchy-dock-bin'; " +
        "exit 0; " +
      "fi; " +
      "pgrep -x omarchy-dock >/dev/null || setsid uwsm-app -- omarchy-dock >/dev/null 2>&1 &"]
    running: true
  }

  Process {
    id: stopper
    command: ["pkill", "-x", "omarchy-dock"]
  }

  // Stopping on teardown is what makes disabling the plugin actually disable
  // the dock, rather than leaving an orphan running until the next login.
  Component.onDestruction: stopper.running = true
}
