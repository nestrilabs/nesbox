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
| Kernel source | same `linux/` dir | Build with `./scripts/config --enable X && make olddefconfig && make -j12 vmlinux`, ~10 min |
| Reference clones | `cloudhypervisor-for-llm-ref/`, `linux-for-llm-ref/` | Read-only references, untracked, **do not build in them** |

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
  src/netsetup.rs    the host's egress rules, and checking they are there
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
| virtio-blk | in-process, sync | root filesystem mounts |
| virtio-console | in-process | typed commands execute in the guest |
| virtio-vsock | kernel, `/dev/vhost-vsock` | host connect to the guest CID gets ECONNRESET from the guest's own stack; an unused CID gets ENODEV |
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
- **`VIRTIO_GPU_SHM_ID_HOST_VISIBLE` is 1, not 0.** Zero is
  `VIRTIO_GPU_SHM_ID_UNDEFINED`. A guest that finds the shared-memory
  capability under id 0 rejects it silently, and the only symptom is
  `-host_visible` in the DRM feature line.
- **rutabaga needs the `virgl_renderer` cargo feature.** Without it the crate
  still builds and `RutabagaBuilder` still runs, but the component the DRM
  native context needs is not there.
- **The host's virglrenderer supplies the native context, not the guest.** It
  has renderers for `amdgpu`, `asahi`, `i915`, `msm` and `panfrost`; check with
  `strings /usr/lib/libvirglrenderer.so.1 | grep -oE '^[a-z0-9]+_ccmd_[a-z_]+'`.
  The guest needs no GPU driver of its own.
- **Mesa's `i915` gallium driver is not the modern Intel one.** It is the gen3
  driver for i830–i945; anything recent wants `iris`. Picking the wrong one
  builds cleanly and produces a driver that never matches the hardware.

---

## 6. Known gaps

- **Egress works**: guest to 1.1.1.1, 0% loss, 11.5ms, with `nesbox setup` as
  the only privileged step. Verified from a blocked host: setup detected the
  dropping forward chain, asked, added both `DOCKER-USER` rules itself and
  reported the host ready. Declining leaves the host untouched and exits
  non-zero.
- **Nothing setup writes survives a reboot.** `ip_forward`, the nftables table
  and the iptables rules all vanish, and the guest silently loses the network
  until someone runs setup again. The preflight will say why, which is better
  than nothing, but a `--persist` that writes `/etc/sysctl.d` and the
  distribution's nftables config is the missing half.
- **passt versus tap is not settled.** The nessh side raised a risk worth more
  than the CPU argument: iroh's hole punching is developed against conntrack,
  and passt is a second NAT with its own mapping semantics. If direct
  connections degrade, sessions fall back to relays and everything keeps working
  slightly worse forever, which is the hardest kind of failure to notice. Test
  hole punching against a real peer before switching.
- **The GPU has never rendered.** §7.
- No API socket, no jailer, no seccomp, no snapshots, no CPU pinning. All were
  in the Firecracker fork; none exist here.
- `virtio-blk` is synchronous on the vCPU thread. Fine for boot, will need an
  io_uring worker under game load.
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

### Still rough

- `map_blob` always takes the `export_blob` route, and that is correct rather
  than a fault: rutabaga's `map_placed` is compiled out unless the unstable
  `virgl_renderer_resource_map_fixed` API is enabled, so it returns
  `Unsupported` unconditionally. Exporting the blob and mapping it ourselves is
  the supported path and what crosvm does. Measured at **~10.7us per mapping**
  across 26 of 27 mappings during Vulkan init, plus one 9.4ms first-touch
  outlier on a 4 KiB buffer. It is a per-resource setup cost, not per frame —
  once mapped, the guest reaches the memory through the KVM slot without
  trapping. Not worth optimising.
- The guest logs `*ERROR* response 0x1200 (command 0x200)` once at startup.
  That is Mesa probing for a context with no capset before retrying with capset
  6, which succeeds. Harmless, and expected given only `RUTABAGA_CAPSET_DRM` is
  enabled.
- Nothing has drawn to a surface; there is no display path and no compositor.

## 8. Suggested order

1. Decide where the host's NAT rules live (§9), so egress actually works.
2. `virtio-blk` is synchronous on the vCPU thread; it will need a worker
   under game load.
3. Reduce the dead code the GPU port brought with it — `cargo build` names
   several unused fields and methods in `gpu/`.

## 9. Running virtio-net

Creating a tap needs `CAP_NET_ADMIN`. Grant it:

```bash
sudo setcap cap_net_admin+ep ./target/debug/nesbox
```

`cargo build` replaces the binary and drops the capability, so **this has to be
redone after every rebuild** — a VM that suddenly cannot start a tap has
usually just been recompiled. Then boot with a `network` section; the tap is
created and addressed automatically. Inside the guest:

```bash
ip addr add 172.30.0.2/24 dev eth0 && ip link set eth0 up && ping -c2 172.30.0.1
```

That is the check that passes today, and it exercises the tap, vhost-net and
both queues. It does **not** cover egress, which additionally needs IP
forwarding and a masquerade rule for the guest's subnet.

Those rules are host-global state, so a running VM does not install them and
neither does the launcher — nessh is network-facing and anonymous by design, and
giving it `CAP_NET_ADMIN` would turn a compromise of it into a compromise of the
host firewall. They live behind a separate privileged one-off instead:

```bash
sudo ./target/debug/nesbox setup examples/vm.json      # idempotent
sudo ./target/debug/nesbox setup --yes examples/vm.json  # for install scripts
sudo ./target/debug/nesbox teardown examples/vm.json   # removes the table
```

Every change to the host is explained and confirmed before it happens — these
are firewall rules and kernel settings the whole machine shares, and somebody
self-hosting a game server should see what is about to change. `--yes` skips the
prompts for an install script that has already asked in its own words. With
neither a terminal nor `--yes`, setup refuses rather than assuming consent.

`setup` enables `ip_forward` and adds a masquerade rule for the guest's subnet
in its own `ip nesbox` nftables table, so re-running it touches nothing else and
an existing rule from libvirt or Docker is left alone. The rule matches on
destination rather than the tap's name, because the name carries a
kernel-assigned number no one knows until a VM starts. Neither change survives a
reboot; persist them the way your distribution expects.

`setup` also clears a third-party firewall out of the way, because one will
otherwise block everything above while everything above is correct. Docker sets
the forward chain's policy to drop the moment it starts, and ufw ships with the
same; in nftables a drop is final, so an accept in our own table cannot rescue a
packet another chain has already refused. The rule has to go in a chain that
firewall honours — `DOCKER-USER` if Docker is present, `FORWARD` otherwise —
and in **both directions**, because conntrack has already reversed the
masquerade by the time a reply arrives, so replies are addressed *to* the
subnet, not from it:

```bash
iptables -I DOCKER-USER -s 172.30.0.0/24 -j ACCEPT
iptables -I DOCKER-USER -d 172.30.0.0/24 -j ACCEPT
```

`setup` does both itself. With only the first the guest's packets leave and
every reply is dropped — measured on this host, not inferred, and it is what the
preflight's two-direction check is built from.

`setup` finishes by re-running the preflight and fails loudly if the host still
is not ready, rather than reporting success for having written correct rules
that something else ignores.

Every VM start with a `network` section runs a preflight and says which piece is
missing — "IP forwarding is disabled" and "no masquerade rule for 172.30.0.0/24"
are reported separately, because otherwise both present as "the game never
connects". It warns and continues rather than refusing to boot: a guest with no
need for egress is a perfectly good guest.

Reading the nftables ruleset needs `CAP_NET_ADMIN`, and file capabilities do not
survive `exec`, so `netsetup::nft` lends the capability to the `nft` child
through the ambient set. Without that the check degrades to "could not tell" —
which still names a remedy, but is the answer the whole module exists to avoid.

If traffic stops flowing after a change, `RUST_LOG=debug` prints the negotiated
feature set, the vnet header size and the tap offload flags at the moment the
device starts. Those three are what usually disagree: a header size the guest
did not expect misparses every frame silently, and a feature bit vhost-net does
not know makes `VHOST_SET_FEATURES` fail outright (it is masked with
`get_features` to prevent exactly that).

Note that the guest names the interface `eth0` even though nothing here sets a
name — that is the guest kernel's own naming, not something nesbox controls.
