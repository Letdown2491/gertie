#!/bin/sh
# Installs gertie for the current user: the binary, the desktop entry,
# and the icon. Idempotent; rerun after updates. No root: everything
# lives under ~/.local.
#
# The icon is SVG-only (the riddim pattern): one file into
# hicolor/scalable/apps/, and the theme renders it at whatever size a
# dock, menu, or alt-tabber asks for. No raster pipeline to get wrong;
# hand-rendered PNGs shipped a white-background 64px icon that stuck
# in the icon cache. The stale PNGs from that attempt are removed on
# install so the theme's SVG wins everywhere.
#
# The binary is expected at target/debug/gertie (or target/release/
# gertie), where scripts/build.sh leaves it: this repo builds inside a
# container (the host has no linker), so the script installs the
# already-built binary rather than recompiling on the host.
set -eu

prefix="$HOME/.local"
share="$prefix/share"
repo="$(cd "$(dirname "$0")/.." && pwd)"

binary="$repo/target/debug/gertie"
[ -f "$binary" ] || binary="$repo/target/release/gertie"
if [ ! -f "$binary" ]; then
    echo "no built binary: run scripts/build.sh build first" >&2
    exit 1
fi

install -Dm755 "$binary" "$prefix/bin/gertie"
install -Dm644 "$repo/assets/io.github.Letdown2491.gertie.desktop" \
    "$share/applications/io.github.Letdown2491.gertie.desktop"
# The icon: SVG-only. Remove the stale PNGs the earlier attempt left
# in the cache's path, then install the SVG under scalable. (POSIX sh:
# no brace expansion, hence the loop.)
for size in 64 128 256 512; do
    rm -f "$share/icons/hicolor/${size}x${size}/apps/io.github.Letdown2491.gertie.png"
done
install -Dm644 "$repo/assets/gertie-icon.svg" \
    "$share/icons/hicolor/scalable/apps/io.github.Letdown2491.gertie.svg"

command -v update-desktop-database >/dev/null &&
    update-desktop-database "$share/applications" || true
command -v gtk4-update-icon-cache >/dev/null &&
    gtk4-update-icon-cache -f "$share/icons/hicolor" || true
command -v gtk-update-icon-cache >/dev/null &&
    gtk-update-icon-cache -f "$share/icons/hicolor" || true

echo "installed: $prefix/bin/gertie"
echo "launcher:  $share/applications/io.github.Letdown2491.gertie.desktop"
echo "icon:      $share/icons/hicolor/scalable/apps/ (SVG, theme-rendered)"
