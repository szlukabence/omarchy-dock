//! `omarchy-dockctl` — send a command to a running omarchy-dock.
//!
//! Exists because a layer-shell surface cannot bind global hotkeys. Wire it up
//! in `~/.config/hypr/bindings.lua`:
//!
//! ```lua
//! for i = 1, 9 do
//!   o.bind("SUPER + " .. i, "Dock item " .. i,
//!     hl.dsp.exec_cmd("omarchy-dockctl activate " .. i))
//! end
//! ```

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

// Shared with the dock binary by path: a two-binary crate has no library to
// put it in, and it is not worth becoming one for a single module.
#[path = "../integrate.rs"]
mod integrate;
#[path = "../bar_widgets.rs"]
mod bar_widgets;
#[path = "../safe_write.rs"]
mod safe_write;

/// Where the dock listens: the user's private runtime directory, never `/tmp`
/// (see `ipc_ctl::socket_path`).
fn socket_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR").filter(|b| !b.is_empty())?;
    Some(PathBuf::from(base).join("omarchy-dock.sock"))
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "usage: omarchy-dockctl <command>\n\n\
             commands:\n  \
             activate <1-9>     focus, cycle, or launch that dock item\n  \
             reveal             show the dock now\n  \
             hide               hide the dock now\n  \
             toggle-autohide    switch auto-hide on or off\n  \
             reload             re-read config and rebuild\n  \
             restyle            re-read the Omarchy theme only\n  \
             settings           open the settings window\n\n\
             omarchy integration:\n  \
             install [--blur] [--keys[=MODS]]\n                     \
             theme-set hook, shell plugin, and menu entries;\n                     \
             --blur also sets up Hyprland blur for `style = glass`;\n                     \
             --keys binds MODS + 1..9 to dock apps (default SUPER+CTRL+ALT)\n  \
             uninstall          remove all three\n  \
             status             show what is installed"
        );
        return std::process::ExitCode::from(2);
    }

    // These act on the filesystem rather than on a running dock, so they are
    // handled before we try to reach the socket — installing is exactly what
    // someone does *before* the dock is running.
    match args[0].as_str() {
        "install" => {
            // `--blur` also turns on Hyprland blur, which only the glass style
            // needs and which Omarchy ships off for the whole desktop.
            let blur = args.iter().any(|a| a == "--blur");
            // `--keys` alone takes the default chord; `--keys=SUPER+CTRL+ALT`
            // names one.
            let keys = args.iter().find_map(|a| match a.as_str() {
                "--keys" => Some(integrate::DEFAULT_KEY_MODS.to_string()),
                a => a.strip_prefix("--keys=").map(str::to_string),
            });
            return report(integrate::install(blur, keys.as_deref()), "installed");
        }
        "uninstall" => return report(integrate::uninstall(), "removed"),
        "status" => {
            let reports = match integrate::status() {
                Ok(reports) => reports,
                Err(e) => {
                    eprintln!("omarchy-dockctl: {e:#}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            for r in reports {
                let mark = if r.installed { "✓" } else { "·" };
                println!("{mark} {:<16} {}", r.label, r.path.display());
            }
            return std::process::ExitCode::SUCCESS;
        }
        _ => {}
    }

    let Some(path) = socket_path() else {
        eprintln!("omarchy-dockctl: XDG_RUNTIME_DIR is not set, so there is no dock to reach");
        return std::process::ExitCode::FAILURE;
    };
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("omarchy-dockctl: cannot reach the dock at {}: {e}", path.display());
            eprintln!("is omarchy-dock running?");
            return std::process::ExitCode::FAILURE;
        }
    };

    // One command per line; the dock reads lines.
    let line = format!("{}\n", args.join(" "));
    if let Err(e) = stream.write_all(line.as_bytes()) {
        eprintln!("omarchy-dockctl: write failed: {e}");
        return std::process::ExitCode::FAILURE;
    }

    std::process::ExitCode::SUCCESS
}

/// Print what an install or uninstall did, and turn a failure into an exit
/// code rather than a panic.
fn report(outcome: integrate::Outcome, verb: &str) -> std::process::ExitCode {
    let integrate::Outcome { reports, error } = outcome;
    for r in &reports {
        // Not everything reported was acted on: install also reports the
        // optional pieces it deliberately left alone, and uninstall what it
        // had to leave because it is not the dock's. `installed` is the state
        // afterwards, so for uninstall it is the piece that stayed.
        let verb = match (verb, r.installed) {
            ("removed", false) => "removed",
            ("removed", true) => "kept",
            (verb, true) => verb,
            (_, false) => "skipped",
        };
        println!("{verb}: {} — {}", r.label, r.path.display());
        if let Some(note) = &r.note {
            println!("          {note}");
        }
    }
    if let Some(e) = error {
        eprintln!("omarchy-dockctl: stopped here: {e:#}");
        if !reports.is_empty() {
            eprintln!("Only what is listed above was done.");
        }
        return std::process::ExitCode::FAILURE;
    }
    if verb == "installed" {
        println!(
            "\nThe shell picks up new plugins on its own; if it does not, run\n  \
             omarchy restart shell"
        );
    }
    std::process::ExitCode::SUCCESS
}
