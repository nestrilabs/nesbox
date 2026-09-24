# nesbox

**A fast microVM with GPU sharing. Guests render within 2% of bare metal, on the
same card, at the same CPU cost — and boot to userspace in about a second.**

nesbox runs a Linux guest with a real GPU attached, built for cloud gaming: boot
a VM, hand it a game, stream the result. Many guests share one bare-metal card;
none of them has a driver for it.

## Is this what you are looking for?

**Yes, if** you want many short-lived Linux VMs sharing one GPU, each rendering
or encoding, isolated from each other, started in the time it takes to launch a
process — and you can build your own guest kernel and Mesa.

**No, if** you want a general-purpose hypervisor, a desktop VM with a display
attached, Windows guests, or GPU passthrough of a whole card to one VM. nesbox
does none of those and is not trying to.

## The numbers

Guest against **the same machine's bare metal**, same headless Vulkan load,
median of three 30-second runs after an 8-second warm-up discard:

| GPU | how the guest reaches it | frame time vs bare metal | CPU vs bare metal |
|---|---|---|---|
| **RX 9060 XT** (RDNA 4) | amdgpu native context, in-process | **99–102%** at ≥3.4 ms/frame | — |
| **RTX 3060** | [virtio-nvgpu](https://github.com/nestrilabs/virtio-nvgpu), vhost-user | **98–100%** at ≥2 ms/frame | 0.39 s vs 0.40 s |

A 60 Hz frame is 16.7 ms and a 144 Hz frame is 6.9 ms, so "a game's frame" sits
well inside the range where the difference is under 2%. Below about 2 ms a frame
both paths cost real percentages, for opposite reasons — see
[`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) §16, which also states plainly what
these numbers do **not** support. (No other hypervisor was benchmarked, so
nothing here says "faster than" anything.)

Also measured, on one card, four guests at a time: throughput rises and each
guest's frame time stays even. §5 of the same document.

## What it is

nesbox is a VMM that puts a virtio-gpu device in every virtual machine. The GPU
is not an optional add-on: it is always present, always connected, always ready.

The primary use case is **cloud streaming**: many lightweight VMs sharing one
bare-metal GPU, each running a game or rendering workload, isolated from one
another, started in the time it takes to launch a process.

## How it works

### Kernel

nesbox boots an ELF `vmlinux` directly — no bootloader, no initrd. Point
`kernel_image_path` at a kernel you built and it is loaded straight into guest
memory, with the root filesystem coming from a virtio-blk device.

Building your own kernel is the point rather than a fallback: it is how you get
a recent one, and how you keep the guest down to the handful of drivers a
microVM actually needs.

### Devices

Everything is virtio 1.0 over PCI — no virtio-mmio, and ECAM rather than legacy
config access, so the guest sees a machine that looks like modern hardware.

| Device | Backed by |
|---|---|
| virtio-gpu | rutabaga_gfx, in-process |
| virtio-net | the host kernel, through vhost-net and a tap |
| virtio-vsock | the host kernel, through vhost-vsock |
| virtio-fs | virtiofsd, over vhost-user |
| virtio-blk | in-process, on a worker thread |
| virtio-console | in-process |

Where the kernel can do the work, it does: only the GPU has to live in the VMM
process, because rutabaga does.

### GPU

The virtio-gpu device is backed by
[rutabaga_gfx](https://crates.io/crates/rutabaga_gfx) over the **DRM native
context** path, which gives the guest close to direct access to the host GPU's
render engine — no intermediate translation layer, no shader recompilation on
the host side.

The guest needs no GPU driver of its own. It binds virtio-gpu, and Mesa inside
the guest talks the native-context protocol to the host's driver. On an Intel
Arc A310 host, `vulkaninfo` in the guest reports the card by name.

### What nesbox is not

nesbox is not a container runtime. It is not a full virtual machine with a BIOS,
ACPI tables for fifty devices, or a thirty-second boot. It is a microVM — a
minimal, purpose-built virtual machine containing what a gaming workload needs
and nothing it does not.

---

## Requirements

### Host

- Linux with KVM (`/dev/kvm`)
- A GPU with a DRM render node (`/dev/dri/renderD128`) — **Intel or AMD** for
  the native-context path below, or **NVIDIA** through `virtio-nvgpu`
- for Intel or AMD: `libvirglrenderer` built with the DRM native context for
  your GPU
- `virtiofsd`, for shared directories
- Host networking prepared once, by `scripts/nestri-net-setup.sh`. It makes the
  bridge and the persistent taps guests attach to. **nesbox itself needs no
  capabilities**: a tap that already exists and is owned by the user nesbox runs
  as can be opened unprivileged, so there is no `setcap` on the binary.

> [!IMPORTANT]
>
> **NVIDIA works, by a different route.** The native-context path above is for
> Intel and AMD. An NVIDIA guest instead runs
> [virtio-nvgpu](https://github.com/nestrilabs/virtio-nvgpu), a vhost-user
> device that forwards the driver's own ioctls, and needs NVIDIA's user-mode
> libraries inside the guest rather than Mesa. It renders, presents and encodes
> — the numbers above are from it — and it is young: one card, one driver
> version, one guest at a time. Build nesbox with `--no-default-features` for
> such a host; see below.
>
> Contributions and funding both help, and the second is why the first is
> slower than it could be. Please reach out.

### Guest

- A kernel with `VIRTIO_PCI`, `PCI_MMCONFIG`, `DRM_VIRTIO_GPU`, `VIRTIO_FS` and
  `VSOCKETS`
- Mesa built with virtio native context support for your GPU vendor
- No proprietary guest drivers, and no host GPU driver in the guest at all

---

## Getting started

```bash
cargo build --release
# two binaries: target/release/nesbox, and target/release/jailer beside it.
# The jailer is a host-side tool, run *before* nesbox and never inside the
# jail it builds -- see docs/SECURITY.md and build/README.md.

# On a host that only forwards NVIDIA ioctls, build without the renderer:
cargo build --release --no-default-features
# This drops the `virgl` feature, and with it rutabaga, virglrenderer and the
# Mesa stack that comes with them -- five host libraries and ~14 MiB of binary
# that such a host would never call. It also removes the build-time need for
# libvirglrenderer >= 1.3.0, which is newer than several distributions ship.
#
# The virtio-gpu device and the stats socket go with it. A config naming `gpu`
# or `stats-socket` is then refused by name during validation, before anything
# is opened -- never ignored.

sudo ./scripts/nestri-net-setup.sh

# once per host: IP forwarding and NAT so guests can reach the network.
# Explains each change and asks before making it.

./target/release/nesbox my-vm.json
```

See `examples/vm.json` for the configuration format.

### Running it under the jailer

Run that way and the box is isolated by the KVM boundary and a seccomp
filter, and nothing else. `tools/jailer` is the other way in: it chroots into
a jail image, drops to a uid of its own, and only then execs nesbox. It is
the thing you run; nesbox is what it hands off to.

```bash
cd build && make materialize    # output/jail/ + output/jailer
```

```bash
sudo ./build/output/jailer \
    --config    my-vm.json \
    --jail-root /path/to/build/output/jail \
    --uid 60000 --gid 60000
```

That is the whole command line. The jailer reads `my-vm.json` and works out
what that box needs — the kernel from `boot-source`, every
`drives[].path_on_host`, the render node from `gpu`, `/dev/net/tun` and
`/dev/vhost-net` if it has a `network`, `/dev/vhost-vsock` if it has a
`vsock`, the directory a `stats-socket` goes in, each
`shared-directories[].path-on-host` — and brings exactly those in, plus
`/dev/kvm`, `/proc`, `/sys` and the config file itself. There is no command
to name and no list of `--bind` flags to keep in step with the config: it
execs nesbox, which is the only thing it ever execs.

Everything comes in at the *same path* it has on the host, so the paths
inside the config keep working unchanged. They do all have to be absolute —
the jailer chroots before nesbox opens any of them, so a relative path would
resolve against a directory that is no longer there, and it says which field
is wrong rather than letting nesbox fail on it later.

Pass `--dry-run` first and it prints the list it derived, marks any path that
does not exist on this host, mounts nothing and needs no root — which is how
you find out a config is wrong without `sudo`.

The jail image is never written to. It is the read-only lower half of an
overlay whose upper half is a `tmpfs` private to that box, so the mount
points the jailer creates, virtiofsd's socket, and anything the box writes to
a path the image does not provide all land there and vanish when it exits.
That is what lets one image be materialized once and shared: a compromised
box cannot leave anything behind for the next one to load.

| | |
|---|---|
| `--nesbox-bin <path>` | nesbox *inside the jail image*. Default `/usr/bin/nesbox` |
| `--bind <path>` | an extra host path, repeatable. An escape hatch for something a config does not name |
| `--scratch-dir <path>` | where the overlay's writable layer goes. Default `/run/nesbox-jailer` |
| `--dry-run` | print what would be brought in, and why, then exit |

**A uid nothing else on the host is using is yours to get right when you run
this by hand.** The jailer refuses one held by a live process, because a
jailed process sharing a uid with a host process can read that process's
`/proc/<pid>/root` and reach straight back out of the jail — but that is a
guard against a colliding uid pool, not an allocator.

**The supervising agent is the allocator.** With jailing enabled it hands each
box a uid out of a stated range, keeps it for the life of the box, and builds
this command line itself — so typing it is for driving a box by hand. See
[SECURITY.md](docs/SECURITY.md) for what the jail does and does not bound.

---

## Status

nesbox is under active development and has not been deployed anywhere. What
works today, verified by running it:

- Boot to Alpine userspace in about a second, with SMP
- A guest that renders on the host GPU through native context
- Networking out to the internet, shared directories, and a vsock control
  channel
- Clean shutdown, with exit codes distinguishing a guest that powered off from
  one that died

What is missing is as important:

- **The jailer is not on by default.** `tools/jailer` chroots into a
  materialized jail image, bind-mounts in the paths a box needs, and drops
  from root to a per-guest uid before exec'ing nesbox — see [Running it under
  the jailer](#running-it-under-the-jailer). A supervising agent allocates the
  uid and runs the jailer for every box when jailing is enabled, but a box
  launched the ordinary way, by running nesbox directly, still runs as whoever
  launched it and is separated from its neighbours by the VM boundary and
  little else — see [SECURITY.md](docs/SECURITY.md).
- **No management API.** Configuration is a JSON file and the process is the
  interface. A read-only metrics socket exists ([STATS.md](docs/STATS.md)); there
  is no way to *control* a running box over it.
- **No snapshots** and no live migration. vCPU threads can be confined to a set of
  host CPUs with `cpu_affinity`, which places them but does not cap them.
- **Performance numbers live in [BENCHMARKS.md](docs/BENCHMARKS.md)**, measured on
  one host; this README quotes none of them.

## Isolation

Guests are isolated by KVM hardware virtualisation. The VMM process is also
confined by a seccomp-bpf allowlist, on by default — see [SECURITY.md](docs/SECURITY.md)
for what that does and, more usefully, what it does not. `tools/jailer` can
chroot a box into a materialized jail image with its own uid and mount
namespace, but nothing yet calls it before nesbox starts, so boxes sharing a
user account are still separated by the VM boundary and not much else.

Multiple nesbox VMs share the host GPU through its DRM render node, each with
its own renderer context and fence timeline, isolated by the kernel's DRM
scheduler.

> **Security note:** a misbehaving guest that exploits a bug in the host GPU
> driver could affect other VMs sharing that GPU. This is inherent to GPU
> sharing and not unique to nesbox. For workloads needing stronger GPU
> isolation, a dedicated GPU per tenant (SR-IOV or MIG where available) is the
> right tool.

## Roadmap

Roughly in priority order:


- **A management API** — the stats socket already reports GPU health and VRAM
  usage; lifecycle control over a socket is what is still missing.
- **Shader cache mounting** — a persistent host directory for virglrenderer, to
  avoid recompiling shaders every boot.
- **NVIDIA, beyond one card and one guest** — `virtio-nvgpu` renders, presents
  and encodes today, and the numbers at the top of this file are from it. What
  it has not done is share a card between two guests, run on more than one
  driver version, or run for longer than a few minutes at a time. That is the
  work, and contributions and sponsorship both help it along.
- **SR-IOV support** — dedicated GPU instances per VM where full hardware
  isolation is required.
- **Multi-GPU support** — select a GPU per VM, or stripe across cards.

---

## Acknowledgements

nesbox began as a fork of [Firecracker](https://firecracker-microvm.github.io/)
and its virtio-gpu work drew on [libkrun](https://github.com/containers/libkrun).
Neither remains in the codebase — the VMM was rewritten from scratch on the
rust-vmm crates — but both shaped it.
