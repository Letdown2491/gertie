#!/usr/bin/env bash
# Assemble gertie's Orchard app tree: the built binary into files/bin,
# beside the committed metadata and the export/ files (desktop entry,
# AppStream metainfo, icon) that orchard-publish turns into a
# flatpak-shaped OCI image. The freedesktop runtime supplies the native
# libraries the binary links (wayland, xkbcommon, fontconfig); nothing
# else ships.
set -euo pipefail
cd "$(dirname "$0")/.."

binary=target/release/gertie
if [[ ! -x $binary ]]; then
    echo "missing $binary; run cargo build --release -p gertie first" >&2
    exit 1
fi

mkdir -p self/tree/files/bin
install -m 0755 "$binary" self/tree/files/bin/gertie

# The metadata's runtime ref carries the arch, and the export's commit
# ref comes from it (app/<id>/<arch>/stable). The tree is assembled
# per-host (CI legs: x86_64 and aarch64; flatpak's arch names match
# uname -m), so stamp the host arch into the runtime ref. On the arch
# the metadata already pins this is a no-op rewrite.
arch=$(uname -m)
sed -i "s|^runtime=org\.freedesktop\.Platform/.*/25\.08$|runtime=org.freedesktop.Platform/$arch/25.08|" \
    self/tree/metadata

# The desktop tree: entry, AppStream metainfo, icon (the flatpak id is
# the icon name the desktop entry already references).
mkdir -p self/tree/export/share/applications
mkdir -p self/tree/export/share/metainfo
mkdir -p self/tree/export/share/icons/hicolor/scalable/apps
install -m 0644 assets/io.github.Letdown2491.gertie.desktop \
    self/tree/export/share/applications/io.github.Letdown2491.gertie.desktop
install -m 0644 assets/io.github.Letdown2491.gertie.metainfo.xml \
    self/tree/export/share/metainfo/io.github.Letdown2491.gertie.metainfo.xml
install -m 0644 assets/gertie-icon.svg \
    self/tree/export/share/icons/hicolor/scalable/apps/io.github.Letdown2491.gertie.svg

echo "assembled self/tree: $(du -sh self/tree/files | cut -f1)"
