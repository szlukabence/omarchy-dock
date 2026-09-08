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

fn socket_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(base).join("omarchy-dock.sock")
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
             restyle            re-read the Omarchy theme only\n\n\
             omarchy integration:\n  \
             install [--blur]   theme-set hook, shell plugin, and menu entries;\n                     \
             --blur also sets up Hyprland blur for `style = glass`\n  \
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
            return report(integrate::install(blur), "installed");
        }
        "uninstall" => return report(integrate::uninstall(), "removed"),
        "status" => {
            for r in integrate::status() {
                let mark = if r.installed { "✓" } else { "·" };
                println!("{mark} {:<16} {}", r.label, r.path.display());
            }
            return std::process::ExitCode::SUCCESS;
        }
        _ => {}
    }

    let path = socket_path();
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
fn report(
    result: anyhow::Result<Vec<integrate::Report>>,
    verb: &str,
) -> std::process::ExitCode {
    match result {
        Ok(items) => {
            for r in &items {
                // Not everything reported was acted on: install also reports
                // the optional pieces it deliberately left alone.
                let verb = if r.installed { verb } else { "skipped" };
                println!("{verb}: {} — {}", r.label, r.path.display());
                if let Some(note) = &r.note {
                    println!("          {note}");
                }
            }
            if verb == "installed" {
                println!(
                    "\nThe shell picks up new plugins on its own; if it does not, run\n  \
                     omarchy restart shell"
                );
            }
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("omarchy-dockctl: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
