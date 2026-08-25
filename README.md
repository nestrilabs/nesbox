# nesbox

**A microVM hypervisor for cloud streaming — GPU included.**

nesbox runs a Linux guest with a real GPU attached, built for cloud gaming: boot
a VM, hand it a game, stream the result. Each VM boots to userspace in about a
second and shares one bare-metal GPU with its neighbours.

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
- An **Intel or AMD GPU** with a DRM render node (`/dev/dri/renderD128`)
- `libvirglrenderer` built with the DRM native context for your GPU
- `virtiofsd`, for shared directories
- Host networking prepared once, by `scripts/nestri-net-setup.sh`. It makes the
  bridge and the persistent taps guests attach to. **nesbox itself needs no
  capabilities**: a tap that already exists and is owned by the user nesbox runs
  as can be opened unprivileged, so there is no `setcap` on the binary.

> [!IMPORTANT]
>
> **Nvidia GPUs are not currently supported.**
> We are developing `virtio-nvgpu`, a custom virtio driver for Nvidia hardware
> [here](https://github.com/nestrilabs/virtio-nvgpu).
> This work requires dedicated engineering time. If you would like to help fund
> or contribute to it, please reach out.

### Guest

- A kernel with `VIRTIO_PCI`, `PCI_MMCONFIG`, `DRM_VIRTIO_GPU`, `VIRTIO_FS` and
  `VSOCKETS`
- Mesa built with virtio native context support for your GPU vendor
- No proprietary guest drivers, and no host GPU driver in the guest at all

---

## Getting started

```bash
cargo build --release
sudo ./scripts/nestri-net-setup.sh

# once per host: IP forwarding and NAT so guests can reach the network.
# Explains each change and asks before making it.

./target/release/nesbox my-vm.json
```

See `examples/vm.json` for the configuration format.

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

- **No jailer, no uid per guest, no mount namespace.** The VMM is confined by a
  seccomp-bpf allowlist (on by default) and can optionally enter a private network
  namespace, but it runs as whoever launched it. Boxes sharing a user account are
  separated by the VM boundary and little else — see [SECURITY.md](docs/SECURITY.md).
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
for what that does and, more usefully, what it does not. There is no jailer
chroot, no uid per guest and no namespaces, so boxes sharing a user account are
separated by the VM boundary and not much else.

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


- **Overlapping block I/O** — requests are served off the vCPU thread but still
  one at a time; io_uring would let a deep queue run concurrently.
- **A uid per guest and a mount namespace** — seccomp landed and bounds what a
  compromised device model can *call*; these bound what it can *reach*, which is
  the larger gap. See [SECURITY.md](docs/SECURITY.md).
- **A management API** — the stats socket already reports GPU health and VRAM
  usage; lifecycle control over a socket is what is still missing.
- **Shader cache mounting** — a persistent host directory for virglrenderer, to
  avoid recompiling shaders every boot.
- **Nvidia GPU support (`virtio-nvgpu`)** — active development; requires
  funding. Contributions and sponsorship welcome.
- **SR-IOV support** — dedicated GPU instances per VM where full hardware
  isolation is required.
- **Multi-GPU support** — select a GPU per VM, or stripe across cards.

---

## Acknowledgements

nesbox began as a fork of [Firecracker](https://firecracker-microvm.github.io/)
and its virtio-gpu work drew on [libkrun](https://github.com/containers/libkrun).
Neither remains in the codebase — the VMM was rewritten from scratch on the
rust-vmm crates — but both shaped it.
