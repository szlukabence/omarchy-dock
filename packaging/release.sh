#!/usr/bin/env bash
# Build the release artefacts for one version from this checkout, into dist/:
#
#   omarchy-dock-<ver>-<rel>-<arch>.pkg.tar.zst   what `pacman -U` installs
#   omarchy-dock-<ver>-<arch>.tar.gz              the bare binaries, for PKGBUILD-bin
#   SHA256SUMS
#
#   packaging/release.sh 1.3.0        (a leading "v" is accepted)
#
# The release workflow runs exactly this on every pushed tag, so a release can
# be rehearsed locally before tagging. Must not run as root: makepkg refuses.

set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:?usage: packaging/release.sh VERSION}"
version="${version#v}"
if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: '$version' is not a version like 1.3.0" >&2
  exit 1
fi

# Every place that states the version has to agree with the tag, or the
# package, the plugin manifest and the binary would each claim a different one.
mismatch=0
check() {
  if [[ $2 != "$version" ]]; then
    echo "error: $1 says ${2:-nothing}, the tag says $version" >&2
    mismatch=1
  fi
}
check Cargo.toml "$(sed -n 's/^version *= *"\([^"]*\)".*/\1/p' Cargo.toml | head -1)"
check manifest.json "$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' manifest.json | head -1)"
check packaging/local/PKGBUILD "$(sed -n 's/^pkgver=//p' packaging/local/PKGBUILD)"
# packaging/aur/PKGBUILD is left out on purpose: it pins the release commit,
# which does not exist until the tag does, so it is bumped after the release.
if (( mismatch )); then
  echo "Bump them all to $version, commit, and move the tag." >&2
  exit 1
fi

# The compiler is pinned like the source: the release is built by exactly the
# Rust release rust-toolchain.toml names, or not at all.
rustver="$(sed -n 's/^channel *= *"\([^"]*\)".*/\1/p' rust-toolchain.toml)"
if [[ $(sed -n 's/^_rustver=//p' packaging/local/PKGBUILD) != "$rustver" ]]; then
  echo "error: packaging/local/PKGBUILD and rust-toolchain.toml name different Rust releases" >&2
  exit 1
fi
export RUSTUP_TOOLCHAIN="$rustver"
if [[ $(rustc --version) != "rustc $rustver "* ]]; then
  echo "error: this is $(rustc --version), the release needs Rust $rustver" >&2
  exit 1
fi
cargo test --locked --release --bin omarchy-dock --bin omarchy-dockctl

# The package is built by the same PKGBUILD that installs a checkout locally,
# so what is released is what was tested above, not a second clone of it.
(cd packaging/local && makepkg -f --noconfirm)

arch="$(uname -m)"
pkgrel="$(sed -n 's/^pkgrel=//p' packaging/local/PKGBUILD)"
pkg="omarchy-dock-$version-$pkgrel-$arch.pkg.tar.zst"

rm -rf dist
mkdir dist
cp "packaging/local/$pkg" dist/

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
install -m0755 target/release/omarchy-dock target/release/omarchy-dockctl "$stage/"
install -m0644 README.md LICENSE THIRD_PARTY_NOTICES "$stage/"
tar -czf "dist/omarchy-dock-$version-$arch.tar.gz" --owner=0 --group=0 -C "$stage" \
  omarchy-dock omarchy-dockctl README.md LICENSE THIRD_PARTY_NOTICES

(cd dist && sha256sum -- * > SHA256SUMS)
echo
cat dist/SHA256SUMS
