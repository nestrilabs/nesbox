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
What stops that is a uid per guest, a mount namespace and a network namespace. The
network namespace is implemented and off by default (`unshare-network`, below);
the other two are not implemented at all. If two boxes run under the same user
account, this filter does not separate them; the VM boundary does.

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

111 syscalls in the baseline. The list is broad because it must hold the union of
every thread's needs, and the GPU worker drags in most of Mesa. **The point is not
that the list is short — it is what is absent from it.**

## Why not Firecracker's jailer

It was considered. The jailer is `chroot` + cgroups + rlimits + uid/gid drop, and
**seccomp is not in it** — Firecracker's filters live in a separate crate applied
by the VMM itself.

- **cgroups** are the supervisor's to apply, not ours (see `BENCHMARKS.md` §12 for
  what they actually bound, which is less than it sounds). Doing it in two places
  means two places to be wrong. What nesbox owes in return is *visibility*: it now
  reads its own cgroup at startup and says which limits are in force, walking up
  the hierarchy because a limit on an ancestor bounds us while our own file still
  reads `max`. Earlier versions of this file, `vram.rs` and `STATS.md` all asserted
  that host memory "is bounded for the whole process by cgroups" as though it were
  a property of the system. It was a property of whoever wrote the unit file, and
  nothing checked.
- **`chroot` fights the requirements** *if the jail is built to exclude the
  host.* `tools/jailer` takes the opposite approach: bind-mount in the DRM
  render node, `/dev/kvm`, any vhost device nodes, `/sys`, `/proc` and the
  metrics socket path a box needs, at the same path inside the jail, then
  chroot. Nothing is discovered or guessed — every path is named on the
  jailer's own command line by whatever launches it.

  That list is not only hardware, and pretending it was would have produced a
  jailed nesbox that cannot find its own kernel. nesbox opens its config file,
  `kernel_image_path`, every `drives[].path_on_host` and `/dev/net/tun` after
  the exec, from inside the jail. So the jailer reads the box config and
  derives the whole set from it rather than taking a list of flags a caller
  has to keep in step with that config by hand — one thing naming the paths,
  not two. `--bind` remains as an escape hatch for whatever a config does not
  name.

  virtiofs source directories are the one case the original objection got
  right, for a different reason: virtiofsd runs unsandboxed as a separate
  process (below), so it is not inside anything the jailer is responsible
  for. Its source directories are derived from `shared-directories` like
  anything else; the *binary* is not bound in at all, because nesbox spawns
  it from inside the jail and the jail image ships it.

  **The image is read-only, and that is what makes sharing it safe.** It is
  the lower half of an overlay whose upper half is a `tmpfs` private to the
  box, so nothing inside a jail can write to the tree the next box will load
  its Mesa, its virglrenderer and its nesbox out of. Without that, one
  guest compromise would mean planting a library every other box on the host
  `dlopen`s — worse than the no-jail baseline, where each box loads from a
  host tree no guest uid can touch. `writing_inside_the_jail_never_reaches_the_image`
  is the test.

  It bounds the image, not the jail. `/proc`, `/sys`, the metrics directory
  and every path derived from the config stay writable exactly as their own
  permissions on the host allow — the overlay says nothing about them.
- It assumes Firecracker's API-socket contract, which is not ours.
  `tools/jailer` has none — it is `chroot` + bind-mount + uid/gid drop +
  `execve`, and nothing else.

Its **uid/gid dropping is worth taking**, and `tools/jailer` now does it.
`neslet` allocates the uid — one per box, out of a range the operator states,
held for the life of the box — and runs the jailer for every box when it is
started with `--jail-root`. The README's *Running it under the jailer* shows
the same command line for driving one by hand. What is left is that it is
opt-in: a box started by running nesbox directly is not jailed at all.

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

## Limits that only exist if something enforces them

Two bounds in this codebase are applied by something other than nesbox, and both
used to be assumed rather than checked. A limit that silently does not apply is
worse than no limit, because it is a limit you have stopped thinking about.

**The VRAM budget is enforced inside virglrenderer**, by
`patches/0002-virglrenderer-amdgpu-per-guest-VRAM-budget.patch`, which reads
`NESTRI_VRAM_LIMIT_MIB`. Which renderer gets loaded is `LD_LIBRARY_PATH`'s
decision, made outside this program. Point it at a stock virglrenderer and
`vram-limit-mib` becomes a no-op: the config still names a number, the stats
socket still reports `vram_limit_bytes`, and `vram_refusals` sits at zero because
nothing is refusing anything.

nesbox now finds the `libvirglrenderer` it actually mapped — from
`/proc/self/maps`, not the link-time name, since a wrong `LD_LIBRARY_PATH` is the
whole failure — and **refuses to start** when a limit is configured and that
library does not carry the marker. Without a configured limit it is a log line.
The check is evidence rather than proof: it shows the library was built from a
tree that knows the variable, not that the enforcement path is reached. A marker
*symbol* resolved with `dlsym` would be stronger and the patch should grow one.

**Host memory is bounded by a cgroup, or by nothing.** `vram.rs` deliberately does
not enforce a GTT limit because GTT is host system memory and that is supposed to
be capped for the process. Nothing here caps it. nesbox now reports the limits
actually in force at startup — walking up the cgroup hierarchy, because an
ancestor's limit bounds us while our own file still reads `max` — and warns when
there is no `memory.max` above it at all.

## What this does not do yet

### A network namespace: available, and off by default

`"unshare-network": true` puts the box in a private user and network namespace.
Afterwards the process has no interfaces beyond a `lo` left down, so `connect`
remains a syscall it may make and reaches nothing. **The guest keeps its own
link**: the tap descriptor is opened before the unshare, and a descriptor — like a
socket's namespace — is fixed when it is created, not when it is used. The same
reasoning keeps the metrics socket reachable from the host.

It is done in two steps for a reason that is not obvious and was measured rather
than guessed. `unshare(CLONE_NEWUSER)` requires a **single-threaded** process, and
by the time the tap is open the block and console devices have each spawned a
worker — the combined call returns `EINVAL` on a real boot. So the user namespace
is entered at the very top of `main`, purely to acquire `CAP_SYS_ADMIN` over
namespaces, and the network namespace is unshared later, where it belongs.

**Why it is off by default.** Entering a user namespace maps only this uid and
gid, so supplementary groups do not survive, and every subsequent open resolves
against the reduced credentials. Where `/dev/kvm` and the render node are `0666`
this costs nothing. Where they are the more usual `0660 root:kvm` / `0660
root:render` and access comes from group membership, **the box will not start or
the GPU will not work**. Test it on the host it will run on. Opening the render
node before the unshare and handing the worker the descriptor would remove the
objection and is what would let this become a default.

`vsock` is untested in combination with it — the kernel's vsock is
namespace-aware — and nesbox warns when both are configured.

### One uid per guest, and a mount namespace — the largest gap, and it is not seccomp

Every box currently runs as whatever user launched it. Because the baseline allows
`openat` and `connect`, a compromised device model inherits that uid's entire
filesystem reach and its network. Two guests under one account are not isolated
from each other by anything in this file.

Three things fix it, and none of them is a syscall filter:

- **A uid per guest.** *Implemented*, in two halves: `tools/jailer` does the
  drop — nesbox cannot do this to itself, an unprivileged process cannot
  change its own uid — and `neslet` allocates the uid, since only the process
  that owns the host's state knows what it has promised to boxes that are not
  running yet. The jailer still refuses a uid a live host process holds, as a
  guard against a colliding pool rather than as the allocator.
- **A mount namespace**, so the paths a guest's VMM can name are the ones it
  was given. *Implemented*: `tools/jailer` unshares one and bind-mounts in
  what a box needs before chrooting into a read-only image.

Both are opt-in rather than automatic: `neslet` jails boxes when it is given
`--jail-root`, and a box started by running nesbox directly is not jailed.
- **A network namespace** with no route out — *this one is implemented*, as
  `unshare-network` above, and off by default.

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
