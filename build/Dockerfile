# ═══════════════════════════════════════════════════════════
#  nesbox jailer image — the host-side chroot target
#
#  Not a bootable OS. A directory tree a jailer would chroot into before it
#  execs nesbox: patched virglrenderer (this repo's patches/, applied to the
#  upstream commit
#  patches/0002 names as its base), nesbox itself, and Mesa — pulled from
#  nestrilabs/nestri:base rather than rebuilt here, so the guest and host
#  sides of the virtio-gpu native-context protocol are always the same
#  patched Mesa and never quietly drift onto two different ones.
#
#  Build from the repo root:
#    docker build -f build/Dockerfile -t nesbox-jail:latest .
#  (`make build` in this directory does exactly that.) Needs
#  nestrilabs/nestri:base to already exist, locally or pullable — see
#  nestri/build/ in the nestri repo.
#
#  Built and verified end to end once already — see build/README.md for
#  exactly what that confirmed.
# ═══════════════════════════════════════════════════════════

# Declared here, at the top of the file, rather than immediately before the
# FROM that uses it further down — podman's builder (buildah, not BuildKit)
# only resolves an ARG referenced in a later FROM if it was declared before
# the *first* FROM. A per-stage ARG placed right above the FROM that uses it
# is valid Dockerfile syntax and works under BuildKit, but fails buildah with
# a misleading "no FROM statement found", confirmed by reproducing it in
# isolation before moving this.
ARG NESTRI_BASE_IMAGE=nestrilabs/nestri:base

FROM docker.io/archlinux:base-devel AS initial
RUN pacman -Syu --noconfirm

FROM initial AS builder
RUN pacman -S --noconfirm --needed \
        meson ninja cmake pkgconf git python python-yaml \
        libdrm libepoxy libx11 libxext libxfixes libxxf86vm \
        wayland wayland-protocols \
        vulkan-headers vulkan-icd-loader \
        mesa \
        rust \
    && pacman -Scc --noconfirm
WORKDIR /build
ENV ARTIFACTS=/artifacts


# ───────────────────────────────────────────────────────────
#  virglrenderer — patched
#
#  Base commit is the one patches/0002's own header names ("Applies on top
#  of 0001, against virglrenderer 7fcfce4"); the patches are the source of
#  truth for what changed and why, not this file.
# ───────────────────────────────────────────────────────────
FROM builder AS virgl-build

ARG VIRGLRENDERER_GIT=https://gitlab.freedesktop.org/virgl/virglrenderer.git
ARG VIRGLRENDERER_COMMIT=7fcfce4

COPY patches/ /build/patches/

# drm-renderers is an array option (meson_options.txt), not a boolean — a
# first attempt at `-Ddrm=true` doesn't exist and fails meson setup outright.
# amdgpu-experimental matches where the patches actually touch
# (src/drm/amdgpu/); i915-experimental is Intel's equivalent, included
# because nesbox's own README lists both vendors as supported host GPUs.
RUN git clone "${VIRGLRENDERER_GIT}" /build/virglrenderer-src && \
    cd /build/virglrenderer-src && \
    git checkout "${VIRGLRENDERER_COMMIT}" && \
    git apply /build/patches/*.patch && \
    meson setup builddir \
        -Dprefix=/usr \
        -Dbuildtype=release \
        -Ddrm-renderers=amdgpu-experimental,i915-experimental \
        -Dvenus=false && \
    ninja -C builddir && \
    DESTDIR=/artifacts/virgl ninja -C builddir install && \
    find /artifacts/virgl -type f -printf '/%P\n' > /artifacts/virgl/.manifest && \
    rm -rf /build/virglrenderer-src


# ───────────────────────────────────────────────────────────
#  nesbox itself
# ───────────────────────────────────────────────────────────
FROM builder AS nesbox-src
WORKDIR /build/nesbox
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY vmm vmm
COPY virtio-devices virtio-devices
COPY pci pci
COPY tools tools

FROM nesbox-src AS nesbox-build
# rutabaga_gfx's build.rs finds libvirglrenderer via pkg-config for compiling
# against it, but pkg-config's own Libs: line evidently isn't what's putting
# it on the linker's search path here — a first attempt with only
# PKG_CONFIG_PATH and LD_LIBRARY_PATH set failed at the final link step
# ("unable to find library -lvirglrenderer"), because LD_LIBRARY_PATH is a
# runtime dynamic-loader path, not a build-time one. LIBRARY_PATH is gcc/ld's
# equivalent for -L at link time, and is what was actually missing.
#
# Arch does not package a DRM-native-context virglrenderer at all, so there
# is nothing to accidentally prefer over the patched build above —
# PKG_CONFIG_PATH is still set explicitly rather than relied on, so that
# stays true on purpose rather than by accident.
COPY --from=virgl-build /artifacts/virgl /virgl-devel
ENV PKG_CONFIG_PATH=/virgl-devel/usr/lib/pkgconfig
ENV LIBRARY_PATH=/virgl-devel/usr/lib
ENV LD_LIBRARY_PATH=/virgl-devel/usr/lib
# /root/.cargo/git too, not just registry: tools/nesprobe's ash dependency
# is a git source, which Cargo caches separately from crates.io downloads —
# missing this meant every build re-fetched it from GitHub regardless of
# whether it had changed.
RUN --mount=type=cache,target=/root/.cargo/registry \
    --mount=type=cache,target=/root/.cargo/git \
    --mount=type=cache,target=/build/nesbox/target \
    cargo build --release --workspace && \
    mkdir -p /artifacts/nesbox/usr/bin && \
    install -Dm755 target/release/nesbox /artifacts/nesbox/usr/bin/nesbox && \
    find /artifacts/nesbox -type f -printf '/%P\n' > /artifacts/nesbox/.manifest


# ───────────────────────────────────────────────────────────
#  The shared Mesa build — pulled, not rebuilt
#
#  NESTRI_BASE_IMAGE (declared at the top of this file — see the comment
#  there for why) defaults to the floating `:base` tag, which is mutable by
#  design — nestri publishes exactly one, continuously-updated `base` tag. A
#  republish can move it to a different Mesa commit with nothing on either
#  side of the native-context protocol to detect the mismatch: no version
#  negotiation exists, so a jail built against a different Mesa than the
#  guest it pairs with can fail rendering silently rather than refuse to
#  boot. Pin this to a digest (`nestrilabs/nestri@sha256:...`) for any build
#  meant to pair with a specific, already-built guest rootfs — the floating
#  default here is for local development, where "whatever base currently is"
#  is the point.
# ───────────────────────────────────────────────────────────
FROM ${NESTRI_BASE_IMAGE} AS mesa-source


# ───────────────────────────────────────────────────────────
#  jail — the tree the jailer actually chroots into
# ───────────────────────────────────────────────────────────
FROM docker.io/archlinux:base AS jail

# Runtime closure: what nesbox and virglrenderer actually link against once
# running, distinct from builder's much larger build-time list. Same reason
# as nestri/build's own os-base: the distro package lands first so every
# transitive runtime dependency is present and correctly versioned before
# our own builds overlay their files on top of it.
RUN pacman -Syu --noconfirm --needed \
        libdrm libepoxy libglvnd libx11 libxext libxfixes libxxf86vm \
        wayland vulkan-icd-loader mesa \
    && pacman -Scc --noconfirm

COPY --from=virgl-build  /artifacts/virgl/.manifest  /tmp/virgl.manifest
COPY --from=nesbox-build /artifacts/nesbox/.manifest /tmp/nesbox.manifest
RUN cat /tmp/virgl.manifest /tmp/nesbox.manifest > /tmp/.strip-manifest && \
    rm -f /tmp/virgl.manifest /tmp/nesbox.manifest

COPY --from=virgl-build  /artifacts/virgl  /
COPY --from=nesbox-build /artifacts/nesbox /

# Mesa: the whole /usr/lib and /usr/share/vulkan out of nestrilabs/nestri:base,
# not a curated subset. Imprecise on purpose rather than by oversight — that
# image has no manifest distinguishing "Mesa's files" from "everything else
# it installed" once published, so this pulls the tree wholesale rather than
# guessing filenames (libEGL*, dri/*_dri.so, ...) and risking a silent
# dlopen failure for whichever one was missed. It costs some irrelevant
# files (pipewire, wireplumber) landing in the jail unused; it does not cost
# anything to the jail's actual boundary, since nothing in this tree can be
# reached except by nesbox itself. A precise Mesa-only manifest, produced by
# nestri/build the same way the strip-manifest above already is, is a
# reasonable follow-up once this has actually been built once.
COPY --from=mesa-source /usr/lib          /usr/lib
COPY --from=mesa-source /usr/share/vulkan /usr/share/vulkan
RUN ldconfig

RUN while IFS= read -r f; do \
        [ -f "$f" ] && strip --strip-unneeded "$f" 2>/dev/null || true; \
    done < /tmp/.strip-manifest; \
    rm -rf /tmp/.strip-manifest

# No package manager needed once assembled — pacman's own database and
# cache are dead weight in a chroot nothing should be installing into at
# runtime, and removing them means there is nothing here for a compromised
# nesbox to invoke even if it tried.
RUN rm -rf /var/lib/pacman /var/cache/pacman \
        /usr/share/man /usr/share/doc /usr/share/locale \
        /usr/lib/cmake /usr/lib/pkgconfig /usr/share/pkgconfig /usr/include
