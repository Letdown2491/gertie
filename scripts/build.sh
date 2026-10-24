#!/usr/bin/env bash
# Builds gertie inside a container (kumaOS is atomic: no host toolchain).
# Derives localhost/gertie-dev from kuma-dev-gcc on first use; rustup and
# the cargo registry live in named volumes so later builds are incremental.
#
#   ./scripts/build.sh check             # cargo check, all targets
#   ./scripts/build.sh build             # debug build, installs to ~/.local/bin
#   ./scripts/build.sh build --release
#   ./scripts/build.sh test
#   ./scripts/build.sh clippy
#   ./scripts/build.sh fmt --all --check
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE=localhost/gertie-dev

if ! podman image exists "$IMAGE"; then
  podman build -t "$IMAGE" -f containers/gertie-dev.containerfile containers/
fi

podman run --rm \
  -v "$PWD":/work:Z -w /work \
  -v gertie-cargo-home:/root/.cargo \
  -v gertie-rustup-home:/root/.rustup \
  --env RUSTFLAGS='-C link-arg=-Wl,-rpath,$ORIGIN/gertie-libs' \
  "$IMAGE" \
  bash -c '
    if [ ! -x /root/.cargo/bin/cargo ]; then
      curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal --default-toolchain 1.97.1
    fi
    export PATH=/root/.cargo/bin:$PATH
    cargo "$@"
  ' _ "$@"

# The binary runs on the host, where the Wayland session, Vulkan driver,
# and font stack live. The container's library set is a superset of the
# host's: any shared library the binary needs that the host lacks
# (libxkbcommon-x11 on this Wayland-only OS, today) is copied from the
# image into gertie-libs/ beside the binary, which the rpath above resolves.
if [ "${1:-}" = "build" ]; then
  profile=debug
  for argument in "$@"; do
    case "$argument" in --release) profile=release ;; esac
  done
  binary="target/$profile/gertie"
  libs="target/$profile/gertie-libs"
  mkdir -p "$libs"
  missing=$(ldd "$binary" 2>/dev/null | awk '/not found/ {print $1}' | sort -u)
  if [ -n "$missing" ]; then
    podman run --rm -v "$PWD":/work:Z "$IMAGE" bash -c "
      for lib in $missing; do cp -L \"/usr/lib64/\$lib\" /work/$libs/; done
    "
  fi
  if ldd "$binary" 2>/dev/null | grep -q 'not found'; then
    echo "error: unresolved shared libraries:" >&2
    ldd "$binary" 2>/dev/null | grep 'not found' >&2
    exit 1
  fi
  # rename(2) over a running binary works where cp fails with Text file busy
  mkdir -p ~/.local/bin
  cp "$binary" ~/.local/bin/gertie.new
  mv -f ~/.local/bin/gertie.new ~/.local/bin/gertie
  rm -rf ~/.local/bin/gertie-libs
  cp -r "$libs" ~/.local/bin/gertie-libs
  echo "installed ~/.local/bin/gertie"
fi
