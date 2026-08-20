#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$project_root"

version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
if [ -z "$version" ]; then
    version="0.1.0"
fi

dist_dir="$project_root/target/dist"
deb_root="$project_root/target/package-staging/wmcterm-deb"
windows_lib_dir="$project_root/target/windows-libs"
mkdir -p "$dist_dir" "$project_root/target/package-staging" "$windows_lib_dir"

echo "Building Linux x86_64 release binary"
cargo build --release --target x86_64-unknown-linux-gnu --bin wmcterm

rm -rf "$deb_root"
install -d \
    "$deb_root/DEBIAN" \
    "$deb_root/usr/bin" \
    "$deb_root/usr/share/applications" \
    "$deb_root/usr/share/icons/hicolor/256x256/apps" \
    "$deb_root/usr/share/doc/wmcterm" \
    "$deb_root/usr/lib/wmcterm"
install -m 0755 target/x86_64-unknown-linux-gnu/release/wmcterm "$deb_root/usr/bin/wmcterm"
strip --strip-unneeded "$deb_root/usr/bin/wmcterm"
install -m 0755 scripts/update-agent-integration.sh "$deb_root/usr/lib/wmcterm/update-agent-integration"
install -m 0644 packaging/linux/wmcterm.desktop "$deb_root/usr/share/applications/wmcterm.desktop"
install -m 0644 assets/icons/256x256.png "$deb_root/usr/share/icons/hicolor/256x256/apps/wmcterm.png"
install -m 0644 packaging/linux/copyright "$deb_root/usr/share/doc/wmcterm/copyright"
installed_size="$(du -sk "$deb_root/usr" | cut -f1)"
cat >"$deb_root/DEBIAN/control" <<EOF
Package: wmcterm
Version: $version
Section: utils
Priority: optional
Architecture: amd64
Installed-Size: $installed_size
Depends: libc6 (>= 2.31), libgcc-s1, python3
Recommends: libnotify-bin, xdg-utils
Maintainer: wmcterm contributors
Description: GPU terminal workspace for humans and AI agents
 wmcterm is a GPU-rendered terminal workspace with panes, workspaces,
 notifications, Agent Hooks and a local control interface.
EOF
chmod 0644 "$deb_root/DEBIAN/control"
dpkg-deb --root-owner-group --build "$deb_root" "$dist_dir/wmcterm_${version}_amd64.deb"

echo "Building Windows x86_64 release binary"
# libghostty-vt 0.2.1 emits a COFF archive named ghostty-vt-static.lib but its
# MinGW link directive searches for libghostty-vt.a. `cargo check` builds the
# archive without reaching the final link; expose the same archive under the
# GNU name until the upstream crate fixes its Windows-GNU link metadata.
cargo check --release --target x86_64-pc-windows-gnu --bin wmcterm
ghostty_archive="$(find target/x86_64-pc-windows-gnu/release/build \
    -path '*/ghostty-install/lib/ghostty-vt-static.lib' -print -quit)"
if [ -z "$ghostty_archive" ]; then
    echo "could not locate the Windows libghostty-vt archive" >&2
    exit 1
fi
install -m 0644 "$ghostty_archive" "$windows_lib_dir/libghostty-vt.a"
RUSTFLAGS="-L native=$windows_lib_dir ${RUSTFLAGS:-}" \
    cargo build --release --target x86_64-pc-windows-gnu --bin wmcterm
x86_64-w64-mingw32-strip target/x86_64-pc-windows-gnu/release/wmcterm.exe

makensis \
    -DVERSION="$version" \
    -DSOURCE_EXE="$project_root/target/x86_64-pc-windows-gnu/release/wmcterm.exe" \
    -DPRODUCT_ICON="$project_root/assets/icons/icon.ico" \
    -DOUTPUT_FILE="$dist_dir/wmcterm-${version}-windows-x86_64-setup.exe" \
    packaging/windows/wmcterm.nsi

echo "Release packages:"
ls -lh "$dist_dir/wmcterm_${version}_amd64.deb" \
    "$dist_dir/wmcterm-${version}-windows-x86_64-setup.exe"
