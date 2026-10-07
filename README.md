# nesbox

**A microVM that shares one GPU across many VMs, at bare-metal speed.**

nesbox is a microVM hypervisor built for cloud gaming and GPU-accelerated streaming on consumer cards.
It puts a virtio-gpu device in every virtual machine, boots in milliseconds, and gives each guest near-native GPU performance.

The GPU is not an optional add-on: it is always present, always connected, and always ready.
You get a Firecracker-like security and isolation model, with a GPU that behaves as if
it were wired directly into the guest.

## Why

Cloud streaming wants many small, isolated VMs on one GPU. Most hypervisors make you choose between
VFIO passthrough, which locks the card to one VM; datacenter-only features (vGPU and SR-IOV);
and slow API forwarding.

So instead we built nesbox, which gives each VM near-native access to a shared card.

## How fast

Each guest compared with the same machine's bare metal:

| GPU        | how the guest reaches it                                   | frame time | CPU  |
| ---------- | ---------------------------------------------------------- | ---------- | ---- |
| RTX 3060   | [virtio-nvgpu](https://github.com/nestrilabs/virtio-nvgpu) | within 2%  | same |
| RX 9060 XT | AMD native context                                         | within 2%  | —    |

That holds for any real game frame (2 ms or more; 60 Hz is 16.7 ms).

## How many guests?

- **12 guests on one RTX 3060**, each encoding 720p60 H.264 with no dropped
  frames.
- **4 guests on an AMD Vega iGPU**, with total throughput _rising_ as guests are
  added (98.8 → 114.3 fps), because one guest leaves the GPU idle between frames.

The card is split evenly, without requiring scheduling from us.

> [!NOTE]
> These are what we measured, not caps. Like containers, you're limited by:
>
> 1. **VRAM.** A game needs gigabytes, so this usually runs out first. Cap each
>    guest with `--vram-limit-mib`.
> 2. **Host CPU and RAM** for each guest.

[Numbers and method →](docs/BENCHMARKS.md)

## Is it for you?

**Yes**, if you want lots of short-lived Linux VMs sharing a GPU, and you're
fine building your own guest kernel.

**No**, if you want Windows guests, a desktop VM with a monitor, whole-card
passthrough, or a general-purpose hypervisor.

## How it works

- **Boots a `vmlinux` directly.** No BIOS, no bootloader, no initrd.
- **Everything is virtio over PCI:** GPU, network, vsock, shared folders, disk
  and console.
- **NVIDIA GPUs** use [virtio-nvgpu](https://github.com/nestrilabs/virtio-nvgpu):
  the guest runs NVIDIA's own drivers (Vulkan, NVENC, and CUDA).
- **Intel and AMD GPUs** work too, through DRM native context.

## Quick start

```bash
cargo build --release --no-default-features   # NVIDIA host; drop the flag for Intel/AMD
sudo ./scripts/nestri-net-setup.sh   # once per host: bridge, taps, NAT
./target/release/nesbox examples/vm.json
```

You'll need

1. Linux with KVM
2. A GPU (NVIDIA, Intel or AMD)
   - NVIDIA guests need the [virtio-nvgpu](https://github.com/nestrilabs/virtio-nvgpu) backend, and NVIDIA's drivers on the host.
   - Intel and AMD need `libvirglrenderer` our [patches](patches/) applied, and Mesa built with `-Dintel-virtio-experimental=true` and `-Damdgpu-virtio=true`.
     `0002` (the per-guest VRAM budget) is for every AMD host. `0001` is only for pre-RDNA GPUs such as Vega, and costs a race on every other one: see its header.
3. `virtiofsd`
4. The guest kernel needs `VIRTIO_PCI`, `PCI_MMCONFIG`, `DRM_VIRTIO_GPU`, `VIRTIO_FS` and `VSOCKETS`.

For more isolation, run each box under the jailer (`tools/jailer`), which gives
nesbox its own chroot and uid. See [`build/README.md`](build/README.md).

## Before you use it

- **It's early.** Breaking changes may land without notice.
- **Guests share the host's GPU driver.** A guest that exploits a driver bug
  could affect other VMs on that card. If you need hard isolation, use a GPU
  per tenant. [Details →](docs/SECURITY.md)
- **No management API, snapshots, or live migration yet.** You configure it with
  a JSON file, and pass it as the only argument. There's a read-only
  [stats socket](docs/STATS.md).

## Learn more

- [Benchmarks](docs/BENCHMARKS.md)
- [Security](docs/SECURITY.md)
- [Stats socket](docs/STATS.md)
- [Config format](examples/vm.json)

nesbox started as a fork of [Firecracker](https://firecracker-microvm.github.io/).
Firecracker's virtio-MMIO devices can't give a GPU the large memory windows it
needs, so nesbox was rewritten on the rust-vmm crates with virtio over PCIe.
Its GPU work also borrowed from [libkrun](https://github.com/containers/libkrun).
Apache-2.0.
