# Maintainer: Bence Szluka <szlukabence@gmail.com>
#
# Builds from the working tree, so `makepkg -si` installs exactly what is
# checked out. Point `source` at a release tarball or a git URL to publish it.

pkgname=omarchy-dock
pkgver=0.1.0
pkgrel=1
pkgdesc="A fast, modern dock for Omarchy (Hyprland / Arch), in Rust + GTK4"
arch=('x86_64' 'aarch64')
url="https://github.com/szlukabence/omarchy-dock"
license=('MIT')

# gtk4-layer-shell is the only non-obvious one: it is the C library the dock
# binds to for anchoring, exclusive zones and layer namespaces.
depends=('gtk4' 'gtk4-layer-shell' 'glib2' 'cairo' 'pango' 'gdk-pixbuf2')
makedepends=('cargo')

# Runtime companions. None are hard requirements — the dock degrades to "that
# feature does nothing" rather than failing — so they are optional.
optdepends=(
  'omarchy: theme tokens, menu, notifications, and the shell integration'
  'hyprland: window state, workspaces, and dispatching'
  'nerd-fonts-jetbrains-mono: glyphs for the launcher, folder stacks and Trash'
)

options=('!lto')
source=()
sha256sums=()

prepare() {
  cd "$startdir"
  # Vendor into the build so the package builds offline and reproducibly.
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "$startdir"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR=target
  # The spike binary is a development artefact and is not shipped.
  cargo build --frozen --release --bin omarchy-dock --bin omarchy-dockctl
}

check() {
  cd "$startdir"
  export RUSTUP_TOOLCHAIN=stable
  cargo test --frozen --release --bin omarchy-dock --bin omarchy-dockctl
}

package() {
  cd "$startdir"
  install -Dm0755 "target/release/omarchy-dock" "$pkgdir/usr/bin/omarchy-dock"
  install -Dm0755 "target/release/omarchy-dockctl" "$pkgdir/usr/bin/omarchy-dockctl"
  install -Dm0644 "README.md" "$pkgdir/usr/share/doc/$pkgname/README.md"
  install -Dm0644 "LICENSE" "$pkgdir/usr/share/licenses/$pkgname/LICENSE" 2>/dev/null || true
}
