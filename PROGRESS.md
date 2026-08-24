# nesbox — working state and handover

Written 2026-08-05. Everything below was verified by running it, not inferred.
If you are picking this up cold, read §1 and §3, then you can work.

Branch: `feat/rewrite`. Nothing is pushed. `dev` and `main` hold the **old
Firecracker fork**, which this branch replaced with a from-scratch VMM in
commit `28f5e45`. The fork is still useful for exactly one thing: it contains
the virtio-gpu work (§7).

---

## 1. Build, run, and drive a guest

```bash
cargo build                                   # ~5s incremental, no special setup
./target/debug/nesbox examples/vm.json        # needs the paths in §2 filled in
```

The VM boots to an Alpine login prompt in about a second. `RUST_LOG=debug`
shows interrupt routing and vhost handshakes; `RUST_LOG=trace` adds every
unhandled PIO/MMIO access and every ECAM read.

**Driving a guest from a script.** Console input works, so the guest can be
told what to do. Set `init=/bin/sh` in `boot_args` and pipe commands in.
`script` is needed because the VMM wants a pty for the console:

```bash
(sleep 14; printf 'mount -t virtiofs hostshare /mnt\ncat /mnt/greeting.txt\n'; sleep 4) \
  | script -qec "./target/debug/nesbox /path/to/config.json" /dev/null
```

Allow ~12–14s before typing: that is `script` and the guest reaching userspace,
not the VM being slow. Everything after `init=/bin/sh` ends with a kernel panic
when the shell exits — that is expected, and `exitcode=0x0` in the panic line
means the last command succeeded.

Without a tty (a launcher spawning us with pipes) everything still works; raw
mode is skipped. Do not reintroduce a hard `tcgetattr` failure there.

Raw mode is entered **only for the `run` path**, after the subcommands have had
their turn. It belongs to the guest console: entering it earlier leaves `nesbox
setup` impossible to answer, because raw mode turns off echo, line buffering and
the signal characters all at once — the prompt appears, keystrokes vanish, and
Ctrl-C cannot interrupt.

**Stopping a VM.** `poweroff -f` in the guest, or SIGTERM to the VMM. Exit
codes: 0 clean or signalled, 1 VMM error, 2 guest reset, 3 guest fault.

---

## 2. Things that live outside this repo

| What | Where | Notes |
|---|---|---|
| Guest kernel | `/mnt/nekopool/PROJEKT/NestriWork/linux/vmlinux` | 7.1.5, ELF. Has PCI, `PCI_MMCONFIG`, virtio-pci/blk/console/net, `DRM_VIRTIO_GPU`, vsock, `VIRTIO_FS`, 8250, ext4, x2APIC. **No native GPU driver is needed in the guest** — native context means the guest only ever binds virtio-gpu |
| Rootfs | `/mnt/nekopool/PROJEKT/NestriWork/rootfs-builder/output/rootfs.ext4` | 2 GiB Alpine + Nestri stack, built from `rootfs-builder/Containerfile`. Root-owned mode 644, so run it `"is_read_only": true`. Mesa needs `-Dvulkan-drivers=…,intel`, `-Dgallium-drivers=…,iris` and `-Dintel-virtio-experimental=true` for the GPU to work |
| Kernel source | same `linux/` dir | Build with `./scripts/config --enable X && make olddefconfig && make -j12 vmlinux`, ~2 min. Slimmed 2026-08-06: see below |
| Reference clones | `cloudhypervisor-for-llm-ref/`, `linux-for-llm-ref/` | Read-only references, untracked, **do not build in them** |

### Guest kernel configuration

Slimmed on 2026-08-06, taking `.text` from 12.9 to 7.5 MiB and the whole
`vmlinux` from 26 to 16 MiB. Removed: `DRM_AMDGPU` (with `DRM_AMD_DC`, by far
the largest single item), `WLAN`, `ETHERNET` — the vendor NIC drivers, which
`VIRTIO_NET` does not live under — `SCSI`, which had no disk drivers under it
at all, and `ACPI_VIDEO`.

Added, and worth keeping:

- `INPUT_EVDEV`, `INPUT_MISC`, `INPUT_UINPUT` — **the guest previously had no
  way to receive input of any kind.** Nothing could read a keyboard, mouse or
  gamepad, and no agent could synthesise one. `/dev/uinput` now exists.
  `INPUT_UINPUT` needs `INPUT_MISC`, which is not obvious from the name.
- `VIRTIO_INPUT` — for when nesbox grows an input device; nothing provides one
  yet.
- `PARAVIRT_SPINLOCKS` — a guest vCPU spinning on a lock held by a vCPU the host
  has descheduled wastes its whole slice. Matters now that SMP works.
- `IP_PNP` — required before the `ip=` command-line plan in §8 can work.

`/usr/lib/libkrunfw.so.5` may still be installed on this host. It is unused and
unusable: its embedded kernel is **virtio-mmio only** — no `virtio_pci`, no
MMCONFIG — so it cannot boot this branch. nesbox builds its own kernel instead,
which is also how it gets a recent one.

virtiofsd 1.14 is installed at `/usr/bin/virtiofsd`. `/dev/vhost-vsock` and
`/dev/vhost-net` both exist and are world-accessible.

---

## 3. Layout of the code

About 5900 lines across three crates.

```
pci/                 config space, CAM1 + ECAM decode, BAR allocation, MSI types
  src/lib.rs         the bus: PIO 0xCF8/0xCFC, ECAM at 0xE0000000, BAR windows
  src/config.rs      PciConfig builder: capabilities, MSI-X, PCIe
  src/msi.rs         MsiVector + MsiRouter, the seam between devices and the VMM
virtio-devices/      one file per device, all virtio 1.0 over PCI
  src/common.rs      shared: queue state, MSI-X table, config-space helpers
  src/blk.rs         virtio-blk, synchronous, single queue
  src/console.rs     virtio-console, hvc0, host stdin <-> guest
  src/vsock.rs       virtio-vsock, queues handed to /dev/vhost-vsock
  src/fs.rs          virtio-fs, queues handed to virtiofsd over vhost-user
  src/net.rs         virtio-net, queues handed to /dev/vhost-net
  src/tap.rs         the tap interface the net device owns
  src/gpu/           virtio-gpu over rutabaga; see §7
vmm/                 the VMM proper
  src/vm.rs          KVM setup, memory, vCPU loop
  src/layout.rs      the guest physical address map — read this first
  src/interrupt.rs   IrqManager: GSI allocation and KVM_SET_GSI_ROUTING
  src/acpi.rs        RSDP/XSDT/FADT/MADT/MCFG/DSDT
  src/boot.rs        ELF kernel load, boot_params, e820
  src/regs.rs        GDT, page tables, long mode entry
  src/serial.rs      16550A at 0x3F8, for earlyprintk
  src/power.rs       ACPI sleep/reset ports
  src/lifecycle.rs   ExitReason and the shared stop signal
  src/virtiofsd.rs   spawns and supervises virtiofsd
  src/memslot.rs     registers host memory as guest RAM, for the GPU window
  src/bin/nesbox.rs  wiring: devices, interrupts, signals, vCPU threads
```

### Guest physical address map (`vmm/src/layout.rs`)

```
0x0000_0000  low RAM (ACPI tables in the top 64 KiB)
0xC000_0000  PCI BAR window, must match PCI0._CRS in the DSDT
0xE000_0000  ECAM, 1 MiB, must match MCFG and be reserved in e820
0xFEC0_0000  IOAPIC
0xFEE0_0000  LAPIC
0x1_0000_0000  high RAM
```

Guest RAM is split around the 3–4 GiB hole and is **memfd-backed and mapped
shared** — vhost-user backends map it themselves, so anonymous memory breaks
virtio-fs.

### Interrupts

GSIs 0–23 keep KVM's default legacy routing (IOAPIC, plus PIC below 16). MSI
vectors are allocated from GSI 24 up; a device's MSI-X table write reprograms
its GSI through `MsiRouter`. INTx still exists as a fallback per the DSDT
`_PRT`, but a healthy boot uses it zero times — that is a useful invariant to
check when something regresses:

```bash
RUST_LOG=trace ./target/debug/nesbox cfg.json 2>&1 | grep -c 'INTx fallback'   # expect 0
```

---

## 4. Devices, and how each is verified

| Device | Backend | Verified by |
|---|---|---|
| virtio-blk | in-process, worker thread | root filesystem mounts; 400 MiB read off `/dev/vda` is byte-identical to the host image at 516 MB/s |
| virtio-console | in-process | typed commands execute in the guest |
| virtio-vsock | kernel, `/dev/vhost-vsock` | host connect to the guest CID gets ECONNRESET from the guest's own stack; an unused CID gets ENODEV. `/dev/vsock` is present in the guest. **No application-level exchange has been done** — the test rootfs has no vsock-capable tool |
| virtio-fs | virtiofsd, vhost-user | `mount -t virtiofs`, reading a host file, EROFS on a read-only export |
| virtio-net | kernel, `/dev/vhost-net` | guest reaches 1.1.1.1 through the host's NAT, 0% loss; no INTx fallback; tap gone after exit. Needs CAP_NET_ADMIN and the host rules in §9 |
| 16550A serial | in-process, TX only | earlyprintk output |
| SMP | KVM | 8 vCPUs, `smpboot: Total of 8 processors activated`, all online, clean poweroff and SIGTERM |
| virtio-gpu | rutabaga, in-process | `vulkaninfo` in the guest reports the host's real GPU — `Intel(R) Arc(tm) A310 Graphics (DG2)`, `0x8086:0x56a6`, discrete — through ANV over native context |

The vsock check, with a VM running at CID 42:

```python
import socket
s = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM); s.settimeout(5)
try: s.connect((42, 1234))
except OSError as e: print(e.errno)   # 104 ECONNRESET = guest replied; 19 ENODEV = no guest
```

---

## 5. Traps already paid for

Do not re-derive these.

- **vhost-kernel and vhost-user disagree about vring addresses.** The kernel
  backend takes *guest physical* addresses and translates them itself.
  vhost-user takes addresses in the *frontend's* address space. Getting it
  backwards surfaces far downstream as `MissingMemoryMapping` and a broken
  pipe.
- **vhost-vsock owns exactly two vrings**, rx and tx. The event queue is the
  device's own. Offering a third gets ENOBUFS.
- **The guest TSS descriptor must be type 11**, busy 64-bit TSS. Type 9 makes
  every `KVM_RUN` fail VM entry with `FailEntry(0x80000021)` before a single
  instruction runs, silently. This cost the project months.
- **Only the bootstrap processor gets boot register state.** Application
  processors must be left in KVM's reset state so they sit in
  `KVM_MP_STATE_UNINITIALIZED` until the guest sends INIT/SIPI. Giving them all
  the BSP's long-mode entry state starts several CPUs racing through the boot
  path. `KVM_RUN` on a vCPU in that state sleeps in the kernel and then returns
  **EAGAIN**, which the vCPU loop must retry rather than treat as fatal.
- **Linux ignores ECAM unless the window is reserved**, no matter what MCFG
  says. It is reserved twice here: an `E820_RESERVED` entry and a `PNP0C02`
  motherboard device.
- **`\_S5` is required in the DSDT** or Linux will not power off, however
  correctly the FADT describes the sleep registers.
- **The console must flush stdout after every batch.** Rust's stdout is
  line-buffered on a terminal, so without it the guest's echo of a keystroke
  waits in our buffer until the guest happens to emit a newline: typing appears
  dead, and anything with no trailing newline — a shell prompt, `clear` — shows
  up only when the *next* command produces output. `serial.rs` already got this
  right; `console.rs` did not.
- **The virtio ISR register is INTx-only.** An MSI-X driver never reads it, so
  anything gated on it stops working after the first interrupt.
- **Capability offsets are not derivable from capability contents.** Only
  virtio vendor capabilities store their length at offset+2; MSI-X has
  Message Control there.
- The DSDT `_CRS` window, `pci::MMIO_WINDOW_*` and `layout::PCI_MMIO_*` must
  agree. Same for the ECAM base across MCFG, `layout`, and the reservation.
- **The 64-bit MMIO window must fit the CPU's physical address width.** Linux
  discards a host bridge window it cannot address and then declines to assign
  the BAR, saying nothing about why. This machine reports 39 bits, so a window
  at 1 TiB vanished; `vm.rs` reads the width from CPUID and places it at the top
  of what is addressable.
- **amdgpu cannot export RADV's buffers as dmabufs, ever.**
  `amdgpu_gem_prime_export` returns `EPERM` for any BO carrying
  `AMDGPU_GEM_CREATE_VM_ALWAYS_VALID`, which is how RADV allocates most of them,
  and no capability or privilege changes that. The first version of `map_blob`
  exported a dmabuf and mapped the fd; it worked on Intel and could never have
  worked on AMD. Resources are now published through
  `virgl_renderer_resource_map` and a per-resource memory slot instead.
- **KVM numbers memory slots and the numbers must be recycled.** Deleting a slot
  frees the region but not the number. The GPU maps and unmaps a blob per
  resource — Vulkan initialisation alone did 27 — so handing out a fresh number
  each time climbs until the VM can map nothing more.
- **`VIRTIO_GPU_SHM_ID_HOST_VISIBLE` is 1, not 0.** Zero is
  `VIRTIO_GPU_SHM_ID_UNDEFINED`. A guest that finds the shared-memory
  capability under id 0 rejects it silently, and the only symptom is
  `-host_visible` in the DRM feature line.
- **rutabaga needs the `virgl_renderer` cargo feature.** Without it the crate
  still builds and `RutabagaBuilder` still runs, but the component the DRM
  native context needs is not there.
- **The host's virglrenderer supplies the native context, not the guest.** Check
  which renderers a given build actually has, rather than assuming — they are a
  build option and distributions differ:
  ```
  strings -a /usr/lib/libvirglrenderer.so.1 \
    | grep -oE '^[a-z0-9]+_ccmd_[a-z_]+' | sed 's/_ccmd_.*//' | sort -u
  ```
  Upstream carries `amdgpu`, `asahi`, `i915`, `msm` and `panfrost`. **The AMD dev
  box returns `amdgpu` alone** — so that host cannot reproduce the A310 result, and
  the only symptom is a guest that finds no GPU. The guest needs no GPU driver of
  its own either way.
- **The virglrenderer on the AMD dev box has no provenance, and that is a
  problem.** `pacman -Qo /usr/lib/libvirglrenderer.so.1.11.0` → *no package owns
  it*: hand-installed, no version string in the binary, unknown commit. It does
  export what the `map_blob` rewrite needs (`virgl_renderer_resource_map` **and**
  `virgl_renderer_resource_map_fixed`), so it works — but a GPU measurement taken
  against it cannot be attributed or bisected later. Build from a recorded commit
  into its own prefix and select it with `LD_LIBRARY_PATH`; never install over the
  distro path, because A/B-ing two renderers is exactly what you want the first
  time a guest fails to render.
- **There is no ccmd protocol version handshake.** `virgl_renderer_capset_drm`
  carries `version_major/minor/patchlevel` straight from the *amdgpu kernel driver*
  (`drm_renderer.c:123`), not a protocol version. So a guest Mesa and a host
  virglrenderer built months apart fail *silently* rather than cleanly. The check
  that matters is whether `amdgpu_virtio_proto.h` agrees between the two trees —
  it is copied between Mesa and virglrenderer by hand, and diffing the two
  checkouts is a five-second answer.
- **Mesa's `i915` gallium driver is not the modern Intel one.** It is the gen3
  driver for i830–i945; anything recent wants `iris`. Picking the wrong one
  builds cleanly and produces a driver that never matches the hardware.

---

## 6. Known gaps

- **Egress works**: guest to 1.1.1.1, 0% loss, 11.5ms, with the host set up as
  the only privileged step. Verified from a blocked host: setup detected the
  dropping forward chain, asked, added both `DOCKER-USER` rules itself and
  reported the host ready. Declining leaves the host untouched and exits
  non-zero.
- **`--persist` has never been run.** The unit it writes is unit-tested for
  content and ordering, but installing it needs root and no host has rebooted
  with it in place.
- **passt versus tap is not settled.** The nessh side raised a risk worth more
  than the CPU argument: iroh's hole punching is developed against conntrack,
  and passt is a second NAT with its own mapping semantics. If direct
  connections degrade, sessions fall back to relays and everything keeps working
  slightly worse forever, which is the hardest kind of failure to notice. Test
  hole punching against a real peer before switching.
- ~~**The GPU has never rendered, and the AMD path is unverified.**~~ **Both done,
  2026-08-24, on a Barcelo iGPU (Ryzen 5 7530U, Vega/gfx90c).** `vulkaninfo` in the
  guest reports `AMD Radeon Graphics (RADV RENOIR)`, `0x1002:0x15e7` — the host's
  card, exact PCI id match — through RADV over native context, and `vkcube` renders
  through `nescope`'s Wayland compositor. The blob-mapping rewrite is correct.

  **It needed a virglrenderer patch that did not exist.** `vulkaninfo` passing was
  necessary and not sufficient: it creates contexts and queries the device, and
  never shares a buffer. The first thing that does failed with

  ```
  amdgpu_renderer_export_opaque_handle:303: failed to get dmabuf fd: Operation not permitted
  ```

  which is **the same `EPERM` as §5's dmabuf note, at a second site.** `map_blob`
  was rewritten to avoid dmabuf export; `export_opaque_handle` was not, and that is
  the path RADV's Wayland WSI takes to hand a frame to a compositor.

  Why the host cannot know to avoid it: RADV marks shareable buffers with
  `AMDGPU_GEM_CREATE_VIRTIO_SHARED`, a **Mesa-private bit** (`sid.h`, `1u << 31`,
  not kernel uapi). The guest converts it to `VIRTGPU_BLOB_FLAG_USE_SHAREABLE` on
  the *blob* and strips it from the ccmd (`amdgpu_virtio_bo.c:176`). So `GEM_NEW`
  arrives with clean flags **and before `RESOURCE_CREATE_BLOB`** — the allocation
  happens before shareability is known. `grep VIRTIO_SHARED` across virglrenderer
  returns nothing.

  Clearing the capset's `has_vm_always_valid` is **not** a fix:
  `radv_device.c:1533` makes it mandatory and RADV fails device creation without it.

  Fix in `patches/0001-virglrenderer-amdgpu-strip-VM_ALWAYS_VALID.patch` against
  virglrenderer `7fcfce4` — strip the flag in `amdgpu_ccmd_gem_new`. Four failures
  before, zero after. Cost: per-submit validation work, since amdgpu must carry the
  BO in the validation list rather than assume residency.

- **`gpu.width`/`gpu.height` set the scanout geometry, not the application
  surface.** Raising them changes nothing a workload does. Measured: solo GPU
  occupancy was 8.9% at a 1080p scanout and 8.9% at 3840x2160, with VRAM
  byte-identical at 36.7 MiB — and identical again with `vkcube --width 3840
  --height 2160`, since the compositor sizes the surface. Anyone treating this
  field as a load knob will measure noise.

- **`nescope` does not offer the IMMEDIATE present mode.** `vkcube --present_mode 0`
  fails with `Present mode specified is not supported`, so a guest cannot render
  uncapped through it — the compositor holds the frame clock. Useful, and note it
  is guest-side, so it bounds a cooperative workload and not a hostile one.

- **`vkcube` is a fixed, overhead-dominated load and cannot calibrate anything.**
  Roughly 1.5 ms of GPU time per frame that is the cost of pushing *any* frame
  through this path rather than the cost of its content, invariant across 2 and 4
  vCPUs and every geometry above. Two guests rendering at once divided a stable
  14.2% occupancy sum against 8.9% solo, and with no frame count that could not be
  resolved into dropped frames versus cheaper frames — it read as a serialization
  ceiling and was not one. `tools/nesprobe` exists because of this; see
  [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md).

- **`VK_KHR_display` cannot work in a native-context guest, and `--wsi display` is
  a dead end.** The extension is advertised and `vkcube --wsi display` still says
  `Cannot find any display!` — because the *display* belongs to `virtio_gpu`
  (`/dev/dri/card0`) while the *renderer* is the host's GPU reached through
  `renderD128`. RADV cannot enumerate another driver's connectors. A compositor is
  the only route to a surface, which is what `nescope` is for.

- **The full guest stack powers the box off if it has no session, and it looks like
  a crash.** With `nestri-guest-hub` running and no vsock and no network, the box
  reaches a login prompt and powers off a few seconds later — reproducibly, with
  stdin held open and nothing typed, so it is not a console bug. The hub is an iroh
  agent with `respawn_max="2"` and an idle timeout; it cannot dial anything, so it
  gives up. For GPU work that needs none of that, boot
  `init=/bin/bash` and mount by hand — `examples/` has the pattern, and
  `test-gpu-bare.json` is a working config. Expect the documented
  `Attempted to kill init!` panic with `exitcode=0x0` when the shell exits.

- **Do not `debugfs -w` into these images.** It does not maintain `metadata_csum`
  consistently: a file written that way came back as mode `120777`, a broken
  symlink, and `e2fsck -fn` reported a wrong inode refcount and an
  `orphan_present` inconsistency after the guest's own boot-time fsck "repaired"
  it. Loop-mount as root, or `mkfs` a fresh image.
- No API socket, no jailer, no seccomp, no snapshots, no CPU pinning. All were
  in the Firecracker fork; none exist here.
- `virtio-blk` serves requests on a worker thread, but serially and with
  blocking reads. Requests do not overlap, so a deep queue is drained one at a
  time; io_uring would let them run concurrently. Measured over a 400 MiB read:
  242 batches, 275ms of I/O in total, 1.14ms mean, 8.5ms worst — that worst case
  is half a frame at 60fps, and it used to happen on the vCPU thread.
- Only one root drive is honoured; extra `drives` entries are ignored.

---

## 7. The GPU

Ported from the old fork and living in `virtio-devices/src/gpu/`. rutabaga comes
from crates.io — `rutabaga_gfx = { version = "0.1.80", features =
["virgl_renderer"] }` — not the git revision the old workspace pinned. Every
symbol the old code used still exists at that version.

```
gpu/protocol.rs         command and response wire format, ported unchanged
gpu/virtio_gpu.rs       the rutabaga bridge: resources, blobs, fences
gpu/worker.rs           command dispatch, on its own thread
gpu/descriptor_utils.rs Reader/Writer over a descriptor chain
gpu/display.rs, edid.rs scanout description and generated EDID
gpu/device.rs           the PCI transport — the only part written from scratch
```

Three seams carry it: `GpuQueues` (pop and complete on the control queue, a
trait because rutabaga's fence callbacks run on its threads), `HostMemoryMapper`
(implemented by `vmm/src/memslot.rs`, registers the shared window with KVM), and
`Reader`/`Writer` taking the descriptor tuples our queues already produce.

BAR0 holds the virtio registers. **BAR2 is a 8 GiB 64-bit prefetchable BAR**
holding the host-visible window blob resources are mapped into, advertised to
the guest through a virtio shared-memory capability and backed by a KVM memory
slot so guest access never traps to us.

### Verifying it

The guest needs `/proc` **and `/sys`** mounted before any of this works: libdrm
enumerates through sysfs, so without it Mesa finds no devices and says only
"Failed to detect any valid GPUs" — which looks exactly like a broken device.
Under `init=/bin/sh` nothing is mounted for you.

```bash
mount -t proc proc /proc; mount -t sysfs sysfs /sys
export XDG_RUNTIME_DIR=/tmp
vulkaninfo --summary
```

That reports the host GPU by name and PCI id if native context is working.
`RUST_LOG=off` makes the guest's own output readable.

`glxinfo`/`eglinfo` still fail, for two separate reasons: there is no display
server in the test rootfs, and `iris_dri.so` is built without virtio support
(`grep -c virtgpu /usr/lib/dri/iris_dri.so` is 0, against 13 for
`libvulkan_intel.so`). GL over native context would need a Mesa rebuild.
Vulkan is what Proton uses, so this has not been chased.

**`grep virtgpu` is an ANV-only test — it false-negatives on RADV.** It returns
**0** for a `libvulkan_radeon.so` that has full native-context support, because
RADV reaches the transport through the shared `vdrm` layer and never contains the
literal string. Judging an AMD rootfs by this test would throw away a working one.
The correct markers come from `src/amd/common/virtio/amdgpu_virtio.c`:

```
for f in MULTIPLE_AMDGPU_CTX VIRTIO_SYNC_CMD 'vdrm_device_connect failed'; do
  printf '%-28s %s\n' "$f" "$(grep -ac "$f" libvulkan_radeon.so)"
done
```

All three present means the driver was built with `-Damdgpu-virtio=true` (the
meson gate, `mesa/meson.build:213`). Both rootfs images on the AMD dev box pass:
the one in `rootfs_and_vmlinux.tar.zst` at Mesa 26.3.0-devel (`git-b78fc73dd8`),
and an older one at 26.1.0-devel (`git-e100ca7c86`).

### Still rough

- `map_blob` publishes each resource as **its own KVM memory slot**, backed by
  the pointer `virgl_renderer_resource_map` returns. There is no host-side
  reservation and no `MAP_FIXED` any more.
- The guest logs `*ERROR* response 0x1200 (command 0x200)` once at startup.
  That is Mesa probing for a context with no capset before retrying with capset
  6, which succeeds. Harmless, and expected given only `RUTABAGA_CAPSET_DRM` is
  enabled.
- Nothing has drawn to a surface; there is no display path and no compositor.

## 8. Suggested order

1. Verify `--persist` survives an actual reboot (§9).
2. **Pass `ip=` on the kernel command line** so the guest configures itself from
   whatever subnet the host chose. Today the guest's address is static, written
   into the rootfs from `rootfs-builder/config/rootfs.conf`, and has to be kept
   in step with the `network` section of the VM's JSON by hand — a mismatch
   there is what made guest networking silently not work. nesbox knows both
   halves and could generate `ip=<guest>::<gateway>:<netmask>::eth0:off`
   itself; the guest kernel needs `CONFIG_IP_PNP`.
3. `virtio-blk` serves requests serially; io_uring would let a deep queue
   overlap.

## 9. Running virtio-net

nesbox **opens** a tap; it never creates one. That is why it needs no
capabilities: `tun_not_capable()` in `drivers/net/tun.c` consults
`CAP_NET_ADMIN` only when creating a device, or when the opener is not that
device's owner. A tap made in advance and handed to the right user is openable
by that user unprivileged.

So the host is prepared once:

```bash
sudo ./scripts/nestri-net-setup.sh
```

It makes the bridge, optionally puts a tagged VLAN uplink on it, and creates
persistent taps (`nesbox0`, `nesbox1`, …) owned by the user nesbox runs as.
A config then names one:

```json
"network": { "tap-name": "nesbox0", "mac": "02:00:00:00:00:01" }
```

### What this replaced, and why

nesbox used to carry `CAP_NET_ADMIN` to create taps, and had `setup`/`teardown`
subcommands that installed the host's forwarding and masquerade rules behind a
consent prompt.

The reasoning behind that split was sound and still is: host-global state is not
a running VM's business, and nessh is network-facing and anonymous by design, so
giving *it* the capability would turn a compromise of it into a compromise of
the host firewall. What changed is the conclusion. A VMM that wants net-admin on
its binary is a thing to ask of everyone who self-hosts, and pre-created taps
make the question unnecessary rather than answering it. Host administration
moved to a shell script, which is what it always was.

It also removed a papercut: `cargo build` replaced the binary and dropped the
capability, so it had to be re-granted after every rebuild — a VM that suddenly
could not start a tap had usually just been recompiled.

### When it does not work

`Tap::open` reports a missing tap, or one owned by somebody else, and names the
remedy. That replaced a preflight which read the nftables ruleset — a check that
needed the very capability this change removes.

The guest's own address arrives on the kernel command line (`nestri.ip=`,
`nestri.gw=`, read by `guest-net` in the rootfs), so two guests on one host no
longer collide on one baked-in address.

If traffic will not flow, `RUST_LOG=debug` prints the negotiated feature set, the
vnet header size and the tap offload flags at the moment the device starts.
Those three are what usually disagree: a header size the guest did not expect
misparses every frame silently, and a feature bit vhost-net does not know makes
`VHOST_SET_FEATURES` fail outright (it is masked with `get_features` to prevent
exactly that).

Note that the guest names the interface `eth0` even though nothing here sets a
name — that is the guest kernel's own naming, not something nesbox controls.
