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
             reload             re-read config and rebuild"
        );
        return std::process::ExitCode::from(2);
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
