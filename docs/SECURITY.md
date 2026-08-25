# Confining the VMM

A guest reaches the GPU through the host's own driver. That is what makes the
native-context design fast, and it means a guest that finds a driver bug can reach
the card and through it other guests on the same card. **That risk is structural
and a syscall filter does not remove it.**

What a filter buys is a bound on what a *compromised device model* can do
afterwards. No `execve`. No `ptrace`, `mount`, `bpf`, `init_module`, `kexec_load`,
`pivot_root`, `setuid`. It cannot start a program, load code into the kernel,
inspect another process, or change how the host is put together, and it costs
nothing measurable at runtime.

**It can still open files and open sockets.** `openat`, `socket`, `connect` and
`sendto` are all on the baseline — the GPU worker needs the render node and Mesa's
shader cache, and the metrics socket needs the rest. So a compromised device model
keeps read and write access to every path this process's uid can reach, and it
keeps network egress. Read that plainly: **seccomp does not stop data leaving.**
What stops that is a uid per guest, a mount namespace and a network namespace, and
none of the three is implemented here yet — see *What this does not do yet*. If two
boxes run under the same user account, this filter does not separate them; the VM
boundary does.

```json
"seccomp": "enforce"    // default. A syscall outside the policy kills the process
"seccomp": "audit"      // report which syscall it was, then exit
"seccomp": "off"        // unconfined
```

`enforce` is the default because a security control that is off by default is not
a control.

## Where the policy comes from

**Not from reading this source**, which is how you miss things. Two upstreams,
both maintained against running VMMs:

- **crosvm's `jail/seccomp/x86_64/gpu_common.policy`** for the GPU half —
  maintained by the people who wrote rutabaga and virglrenderer, so it covers what
  Mesa actually does. It contributed things nobody would derive from our code:
  `sched_setscheduler` and `kcmp` are needed *specifically on AMD*, and Mesa's
  shader cache needs the `inotify` family plus `flock`, `rename` and `fallocate`.
- **Firecracker's `resources/seccomp/x86_64-*.json`** for the VMM and vCPU halves,
  and for the shape of the installer.

110 syscalls in the baseline. The list is broad because it must hold the union of
every thread's needs, and the GPU worker drags in most of Mesa. **The point is not
that the list is short — it is what is absent from it.**

## Why not Firecracker's jailer

It was considered. The jailer is `chroot` + cgroups + rlimits + uid/gid drop, and
**seccomp is not in it** — Firecracker's filters live in a separate crate applied
by the VMM itself.

- **cgroups** are already applied from outside, by whatever supervises the process
  (see `BENCHMARKS.md` §12). Doing it twice means two places to be wrong.
- **`chroot` fights the requirements**: the DRM render node, virtiofs source
  directories, and the metrics socket path all live outside any plausible jail.
- It assumes Firecracker's API-socket contract, which is not ours.

Its **uid/gid dropping is worth taking** and is not implemented here yet.

No dependency was added either. The runtime install is `prctl(PR_SET_NO_NEW_PRIVS)`
plus one `seccomp` syscall against a flat array of 8-byte instructions — the whole
mechanism is `vmm/src/seccomp.rs`, and security-critical code short enough to read
in one sitting is worth more than security-critical code someone else audited.

## Verified

Full stack under `enforce`: boot, virtio-fs, virtio-gpu rendering through the
native context, VRAM accounting, the metrics socket, `nesprobe` completing with
`EXIT=0`. No refusals.

| `seccomp` | fps | p50 | p99 |
|---|---|---|---|
| `off` | 76.26 | 10.159 | 28.579 |
| `enforce` | 75.41 | 10.269 | 29.326 |

**No measurable cost** — the 1.1% gap on the median is smaller than run-to-run
variation on this host, and both p99s are dominated by GPU clock ramp at a 2 s
warm-up (`BENCHMARKS.md` §8.2), not by the filter.

**What that run did not cover: shutdown.** It was measured with a config that has
no `shared-directories`, so it never reached `Virtiofsd::drop`, which reaps each
daemon with `wait4` — a syscall the first version of the policy did not allow. Any
VM with a shared directory therefore died of `SIGSYS` on every clean exit: sockets
left behind, the exit code a signal instead of the reason the VM stopped, and the
one signal that is supposed to mean *a device model was compromised* firing on
ordinary teardown. `wait4` is on the list now and
`a_child_can_still_be_reaped_under_the_baseline` covers it, but the shape of the
mistake is worth keeping: **a policy verified only against the paths a benchmark
takes is verified against the paths a benchmark takes.** Teardown, error paths and
the guest's own bad behaviour are where the remaining gaps will be.

## What this does not do yet

### One uid per guest, and namespaces — the largest gap, and it is not seccomp

Every box currently runs as whatever user launched it. Because the baseline allows
`openat` and `connect`, a compromised device model inherits that uid's entire
filesystem reach and its network. Two guests under one account are not isolated
from each other by anything in this file.

Three things fix it, and none of them is a syscall filter:

- **A uid per guest.** The jailer's uid/gid drop, noted below as "worth taking";
  it is the single highest-value unfinished item in this document.
- **A mount namespace**, so the paths a guest's VMM can name are the ones it was
  given.
- **A network namespace** with no route out, so egress is not a policy decision.

Narrowing `ioctl` is worth doing and buys less than any one of these. Ranking the
work honestly matters more than doing the tractable part first.

### No argument filtering, and `ioctl` is the one that matters

Both upstreams constrain arguments: `ioctl` to specific request numbers,
`mmap`/`mprotect` to specific protection flags, `prctl` to specific options.
This filter allows those syscalls outright.

`ioctl` is the significant gap. It is how every KVM operation, every DRM call and
every DMA-BUF operation is made, so allowing it wholesale means a compromised
thread can issue *any* ioctl on *any* descriptor it holds. crosvm narrows this
with `arg1 & 0x6400` (the DRM ioctl base) and an explicit list; that is the obvious
next tightening. A syscall-number allowlist is the floor, not the ceiling.

Note the limit of what seccomp can ever do here: it cannot see which *file* a
descriptor refers to, only the request number. "ioctl on the DRM fd only" is not
expressible.

### The per-thread vCPU filter exists and is not enabled

A vCPU thread needs almost nothing — it runs the guest and talks to KVM. A filter
denying it `openat`, `socket` and `clone` would be the single largest hardening
available, and `seccomp::vcpu()` is written and tested.

**It cannot be installed yet, for a structural reason.** Device workers are
spawned *lazily, by whichever vCPU thread services the guest's activation write*,
and a thread inherits its creator's filters. So the GPU worker would come into
existence already forbidden from opening the render node — measured, it dies at
virtio-gpu probe and the guest never gets a display.

The fix is to stop vCPU threads being the parents of long-lived workers: have
activation hand the work to a thread that already exists under the baseline. Until
then this is left off rather than shipped broken.

That investigation also found a **limitation of `audit` mode**: a refusal in a
freshly-cloned thread dumped core without the handler reporting, so audit cannot
name the syscall in every case. It works for the common case, and a run that dies
with no message means the failure was in a thread too young to take a signal.

### Everything else

- The filter is **per-process, one process per box**, so it is per-tenant by
  construction. It is *not* per-device: crosvm gets that by running each device in
  its own process, and a single-process VMM cannot match it.
- **virtiofsd is a separate process, is not covered by this filter, and is not
  sandboxed at all.** `Virtiofsd::spawn` passes `--sandbox none`, which turns off
  virtiofsd's own namespace sandbox — it needs privileges an unprivileged VMM does
  not have. So the process holding a FUSE channel to the guest *and* the shared
  directories open is unconfined. An earlier version of this file claimed it
  "applies its own sandbox"; that was wrong, and it was the sentence most likely to
  stop someone asking. Running it under its own uid, or giving the VMM enough
  privilege to let virtiofsd sandbox itself, is unfinished work.
- `seccomp` itself is on the allowlist. That looks wrong and is not: filters are
  monotonic — every filter on a thread must allow a syscall, so an added filter can
  only remove, and with `NO_NEW_PRIVS` set there is no way to relax one. The worst a
  compromised thread achieves is confining itself further.
