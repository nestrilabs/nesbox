# build/ — the jailer image

Builds the directory tree a jailer would chroot into before it execs
nesbox: patched `virglrenderer` (this repo's
`patches/`, applied to upstream), `nesbox` itself, and Mesa — reused from
`nestrilabs/nestri:base`, not rebuilt, so the guest and host sides of the
virtio-gpu native-context protocol are always the same patched Mesa commit.

```
build/
├── Dockerfile          builder, virgl-build, nesbox-build, mesa-source, jail
├── scripts/
│   └── materialize.sh   unpack the image into a plain directory (local testing)
├── Makefile
└── output/              `make materialize` writes here (gitignored)
```

```sh
make build          # docker build -t nesbox-jail:latest
make materialize     # + unpack into output/jail/
```

Needs `nestrilabs/nestri:base` to already exist — build it from
`nestri/build/` in the [`nestri`](https://github.com/nestrilabs/nestri)
repo first.

## Why this is not a bootable image

Unlike `nestri/build/`, this is not a guest disk image with an init system —
it's a chroot target for one process the jailer is about to exec. No OpenRC,
no fstab, no getty. Just enough userspace for `nesbox` to run: Mesa, patched
`virglrenderer`, and `nesbox`'s own dynamic link closure.

## What a first build attempt already confirmed

`virgl-build` has actually been run against a real checkout of commit
`7fcfce4`. Two things this Dockerfile started out guessing at are settled
now, not just asserted:

- **`git apply patches/*.patch` applies cleanly.** The patches read as
  plain unified diffs rather than `git format-patch` output, and that read
  was right — no `git am` needed.
- **The DRM native-context meson option is not `-Ddrm=true`.** That was
  this repo's first guess and meson rejected it outright (`Unknown option:
  "drm"`). The real option, from `meson_options.txt` at that commit, is
  `drm-renderers` — an array, not a boolean — with `amdgpu-experimental`
  and `i915-experimental` among its choices. `patches/0001` and `0002` only
  touch `src/drm/amdgpu/`, which lines up: `amdgpu-experimental` is the one
  the patches actually need; `i915-experimental` is included because
  nesbox's own README lists Intel as a supported host GPU too, even though
  no patch here touches it yet.

Everything past that point in the Dockerfile — `nesbox-build`, and the
`jail` stage that pulls Mesa from `nestrilabs/nestri:base` — has not been
build-tested yet.

## The Mesa closure is imprecise, on purpose

`jail`'s final stage copies the whole `/usr/lib` and `/usr/share/vulkan` out
of `nestrilabs/nestri:base`, not a curated list of Mesa's own files. That
image carries no manifest distinguishing Mesa's files from everything else
it installed once published — guessing filenames (`libEGL*`,
`dri/*_dri.so`, ...) risks silently missing one, which fails as a runtime
`dlopen` error nobody sees until nesbox is already running inside the jail.
Pulling the tree wholesale costs some unused files (`pipewire`,
`wireplumber`, ...) landing in the jail; it costs nothing to the boundary
itself, since nothing in that tree is reachable except by `nesbox`. A
precise Mesa-only manifest — the same mechanism `nestri/build`'s own
`strip-manifest` already uses internally — is a reasonable follow-up once
this has built once for real.

## What this does not do

Nothing here allocates a uid, unshares a mount namespace, or bind-mounts
`/dev/dri`, `/dev/kvm`, `/sys`, `/proc` or the metrics socket into the jail
— that is the jailer binary's own job, proposed but not yet implemented.
This directory only builds what the jailer chroots *into*.

Materializing the built image onto shared storage in production is whatever
launches the box's job — `scripts/materialize.sh` here is a stand-in for
local testing, not that mechanism.
