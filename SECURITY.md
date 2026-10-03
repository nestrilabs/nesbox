# Security

Found a security hole? Skip to [Reporting](#reporting).

## The short version

Each guest is a KVM virtual machine, and the VMM is locked down with seccomp.
That's a solid boundary between a guest and the host.

The weak spot is the GPU. Guests share the host's GPU driver, so **a guest that
finds a driver bug could reach the card and other guests on it**. No filter can
remove that. If you need hard isolation between tenants, use a GPU per tenant.

## What's on by default

- **KVM isolation.** Each guest runs its own kernel in its own VM.
- **A seccomp filter**, set to `enforce`. nesbox can't start programs, load
  kernel code, inspect other processes, or change mounts. It has no measurable performance cost.
- **No root needed.** nesbox runs without any capabilities.

## What you can turn on for extra security

- **The jailer** (`tools/jailer`). It gives each box its own uid, mount
  namespace and read-only root, so boxes can't see each other's files.
- **`"unshare-network": true`.** It cuts nesbox off from the network, while
  the guest keeps its own link. Test it on your host first: if `/dev/kvm` or
  the render node are only group-accessible, the box won't start.
- **`vram-limit-mib`**, a per-guest VRAM cap. nesbox refuses to start if the
  renderer it loaded can't enforce it.

## What isn't covered yet

- **Without the jailer, boxes run as whoever launched them.** A compromised
  box can read anything that user can, and can reach the network. Seccomp
  doesn't stop data leaving.
- **`ioctl` isn't filtered by argument.** A compromised thread can issue any
  ioctl on any descriptor it holds.
- **virtiofsd isn't sandboxed.** It runs as a separate process with the shared
  folders open.
- **Host memory is only capped if you put nesbox in a cgroup.** nesbox tells
  you at startup which limits apply.
- **For NVIDIA guests, the virtio-nvgpu backend runs outside the jail.** See
  its [security notes](https://github.com/nestrilabs/virtio-nvgpu/blob/dev/SECURITY.md).

The full details, including where the seccomp policy comes from, are in
[`docs/SECURITY.md`](docs/SECURITY.md).

## Reporting

Please report vulnerabilities privately, not in a public issue:

- Email **[security@nestri.io](mailto:security@nestri.io)**, or
- Open a private [GitHub security advisory](../../security/advisories/new) on
  this repository.

If you can, include your host GPU, the config you ran, and the steps to
reproduce. Thank you in advance.
