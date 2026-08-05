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

**Stopping a VM.** `poweroff -f` in the guest, or SIGTERM to the VMM. Exit
codes: 0 clean or signalled, 1 VMM error, 2 guest reset, 3 guest fault.

---

## 2. Things that live outside this repo

| What | Where | Notes |
|---|---|---|
| Guest kernel | `/mnt/nekopool/PROJEKT/NestriWork/linux/vmlinux` | 7.1.5, ELF. Has PCI, `PCI_MMCONFIG`, virtio-pci/blk/console/net, `DRM_VIRTIO_GPU`, vsock, `VIRTIO_FS`, 8250, ext4, x2APIC |
| Rootfs | `/mnt/nekopool/PROJEKT/NestriWork/rootfs-builder/output/rootfs.ext4` | 2 GiB Alpine + Nestri stack. Root-owned mode 644, so run it `"is_read_only": true` |
| Kernel source | same `linux/` dir | Build with `./scripts/config --enable X && make olddefconfig && make -j12 vmlinux`, ~10 min |
| Reference clones | `cloudhypervisor-for-llm-ref/`, `linux-for-llm-ref/` | Read-only references, untracked, **do not build in them** |

`/usr/lib/libkrunfw.so.5` is installed and the README talks it up, but its
kernel is **virtio-mmio only** — no `virtio_pci`, no MMCONFIG — so it cannot
boot this branch. Nothing loads it. Ignore it or delete the README claim.

virtiofsd 1.14 is installed at `/usr/bin/virtiofsd`. `/dev/vhost-vsock` and
`/dev/vhost-net` both exist and are world-accessible.

---

## 3. Layout of the code

4270 lines across four crates.

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
  src/bin/nesbox.rs  wiring: devices, interrupts, signals, vCPU threads
vm-core/             a vestigial event-manager vCPU subscriber, unused
```

`vm-core` is dead code. Nothing depends on its behaviour; delete it when
convenient.

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
| virtio-net | kernel, `/dev/vhost-net` | guest pings the host end of the tap both ways, no INTx fallback, tap gone after exit. Needs CAP_NET_ADMIN, see §9 |
| 16550A serial | in-process, TX only | earlyprintk output |
| SMP | KVM | 8 vCPUs, `smpboot: Total of 8 processors activated`, all online, clean poweroff and SIGTERM |

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

---

## 6. Known gaps

- **The net link works, egress does not.** The guest can reach the host end of
  its tap and nothing further, because routing the subnet outward is host-global
  state nesbox does not install. §9 has the rules; where they belong is open.
- **No GPU.** §7.
- No API socket, no jailer, no seccomp, no snapshots, no CPU pinning. All were
  in the Firecracker fork; none exist here.
- `virtio-blk` is synchronous on the vCPU thread. Fine for boot, will need an
  io_uring worker under game load.
- Only one root drive is honoured; extra `drives` entries are ignored.
- `vm-core` is dead code.

---

## 7. Next: the GPU

The old Firecracker fork has a working-ish DRM native-context virtio-gpu, about
3800 lines, at commit `caade9c` (branch `main`, also `wip/rutabaga_gfx-update-junk`):

```
src/vmm/src/devices/virtio/gpu/{device,virtio_gpu,protocol,worker,edid,
                                display,descriptor_utils,event_handler,mod}.rs
```

`git show main:src/vmm/src/devices/virtio/gpu/virtio_gpu.rs` to read one
without checking anything out. `protocol.rs` (870 lines) and `virtio_gpu.rs`
(909 lines) are transport-agnostic and should port nearly unchanged;
`device.rs`, `event_handler.rs` and `worker.rs` are tied to Firecracker's
virtio-MMIO transport and its event manager, and are the real work.

The rutabaga dependency is pinned but commented out in the workspace
`Cargo.toml`:

```toml
#rutabaga_gfx = { git = "https://github.com/magma-gpu/rutabaga_gfx.git", rev = "2f0c6a55fd36e61b22a1b3679f69d4273c056602" }
```

`caade9c` also vendored the whole crate under `src/rutabaga_gfx/`, which is why
that commit is 577 files. Decide early whether to depend on the fork or vendor
it again.

Host GPU is at `/dev/dri/renderD128` (card1). The guest kernel already has
`DRM_VIRTIO_GPU=y`. Expect to need a large, likely 64-bit prefetchable BAR for
the GPU's host-visible memory, which the current BAR allocator does not support
— it only does 32-bit non-prefetchable BARs inside a 512 MiB window. Plan for
that before porting device code.

## 8. Suggested order

1. 64-bit prefetchable BAR support in `pci`, which the GPU needs anyway.
2. The GPU port.
3. Decide where the host's NAT rules live (§9), so egress actually works.
4. Delete `vm-core`; make the README describe what is actually true.

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
both queues. It does **not** check egress: reaching the internet additionally
needs, on the host,

```bash
sudo sysctl -w net.ipv4.ip_forward=1
sudo nft add table ip nesbox
sudo nft 'add chain ip nesbox postrouting { type nat hook postrouting priority 100 ; }'
sudo nft add rule ip nesbox postrouting ip saddr 172.30.0.0/24 oifname != "nesbox0" masquerade
```

That is host-global state, which is why nesbox does not install it. Where it
should live — an install step, a systemd unit, or nessh — is still open.

If traffic stops flowing after a change, `RUST_LOG=debug` prints the negotiated
feature set, the vnet header size and the tap offload flags at the moment the
device starts. Those three are what usually disagree: a header size the guest
did not expect misparses every frame silently, and a feature bit vhost-net does
not know makes `VHOST_SET_FEATURES` fail outright (it is masked with
`get_features` to prevent exactly that).

Note that the guest names the interface `eth0` even though nothing here sets a
name — that is the guest kernel's own naming, not something nesbox controls.
