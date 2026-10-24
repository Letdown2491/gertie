# Dev image — kumaOS is atomic and has no host toolchain, so cargo runs in
# this container. Derived from the same base as riddim's dev image.
# Build:  podman build -t localhost/gertie-dev -f containers/gertie-dev.containerfile containers/
# Check:  podman run --rm -v "$PWD:/work:Z" -w /work localhost/gertie-dev cargo check
# (:Z relabels the mount for SELinux on atomic hosts.)
#
# The vulkan/wayland/xkb/fontconfig packages exist for GPUI. TLS stays on
# rustls, so no openssl-devel.

FROM localhost/kuma-dev-gcc

# The base locks /root down to r-x; cargo and rustup live on named
# volumes mounted here, so the mountpoints must exist at build time.
RUN chmod 755 /root \
    && mkdir -p /root/.cargo /root/.rustup \
    && dnf -y install \
        pkgconf-pkg-config \
        git \
        clang \
        fontconfig-devel \
        freetype-devel \
        libxkbcommon-devel \
        libxkbcommon-x11-devel \
        wayland-devel \
        libxcb-devel \
        vulkan-devel \
    && dnf clean all
