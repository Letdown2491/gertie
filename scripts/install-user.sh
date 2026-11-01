#!/bin/sh
# Installs gertie for the current user: the binary, the desktop entry,
# and the icon (every hicolor size the SVG renders). Idempotent; rerun
# after updates. No root: everything lives under ~/.local.
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
for size in 64 128 256 512; do
    install -Dm644 "$repo/assets/gertie-$size.png" \
        "$share/icons/hicolor/${size}x${size}/apps/io.github.Letdown2491.gertie.png"
done

# The SVG scales to any size a theme asks for; the PNGs cover the
# fixed sizes hicolor expects without a rescale at lookup time.
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
echo "icon:      $share/icons/hicolor/{64,128,256,512}/apps/ + scalable/"
