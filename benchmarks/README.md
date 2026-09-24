# Committed benchmark results

One file per **GPU**, written by `scripts/bench.sh`. **Commit them.** A result in
a terminal is an anecdote; a result in the repository is something the next
change can be compared against.

Named for the card, not the machine: a result belongs to a GPU, and a hostname
in a public repository names a box that is often somebody else's. `bench.sh`
derives a default name from the card (`navi-44.json`); the files here are named
the way a reader would recognise the card, which is worth the small
inconsistency. The machine's CPU, governor and power state are in each file's
`provenance`, because a figure cannot be read without them.

```sh
scripts/bench.sh          # writes benchmarks/<gpu>.json
```

## Reading these

- **`completed` first.** A run cut short still emits numbers, and they are a
  partial run's numbers.
- **Ratios, not absolutes.** A guest-to-host ratio on one machine is a property of
  the software. A millisecond figure is a property of that machine — its governor,
  its power profile, whether it was on battery. All of that is in `provenance`
  precisely so the numbers are not read without it.
- **`not_measured` is part of the result.** Network, random I/O, boot time and any
  comparison against another hypervisor are absent, and the file says so rather
  than leaving a reader to assume coverage.

## Adding a machine

Nothing to configure. `bench.sh` finds the render node itself, and every
provenance field degrades to `null` on a machine that does not have it, so a new
machine needs no code change — which is the point, since these numbers have to
be re-taken on every new GPU.

See `../docs/BENCHMARKS.md` for what each number means, and §8 there for the
measurement rules that produced it.
