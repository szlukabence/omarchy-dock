#!/usr/bin/env bash
#
# Build and install omarchy-dock from this checkout.
#
# `omarchy plugin add` clones this repository into
# ~/.config/omarchy/plugins/omarchy-dock/, and that clone is the full source
# tree — so the plugin you just installed can build the binary it supervises.
# Run this from wherever the checkout lives:
#
#   cd ~/.config/omarchy/plugins/omarchy-dock && ./install.sh
#
# The build goes through makepkg, so pacman owns the files and upgrading or
# removing them is a normal package operation rather than tracking down loose
# binaries. Everything it produces — target/, pkg/, src/, the package archive —
# is gitignored, so a plugin checkout stays clean and `omarchy plugin update`
# keeps working.

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

die() {
  printf '\nomarchy-dock: %s\n' "$1" >&2
  exit 1
}

for cmd in makepkg cargo; do
  command -v "$cmd" >/dev/null 2>&1 || die "$cmd is not installed.
  makepkg comes with base-devel:  sudo pacman -S --needed base-devel
  cargo comes with rust:          sudo pacman -S --needed rust"
done

echo "==> Building omarchy-dock (this compiles a GTK4 Rust project; give it a minute)"
cd "$root/packaging/local"

# -si builds and installs. The pacman step asks for your password; that is the
# only privileged thing here.
makepkg -si --noconfirm

command -v omarchy-dock >/dev/null 2>&1 ||
  die "the package installed but omarchy-dock is not on PATH.
  If you have a stale copy in ~/.local/bin, remove it — that directory can
  precede /usr/bin and would shadow the package."

echo
echo "==> Wiring into Omarchy"
omarchy-dockctl install

cat <<'EOF'

Done. Start it now with:

  omarchy-dock &

or just log out and back in — the plugin starts it with the shell.

Settings:  right-click the launcher, or `omarchy-dockctl settings`
Removal:   see the Removal section of README.md
EOF
