# Committed benchmark results

One file per host, written by `scripts/bench.sh`. **Commit them.** A result in a
terminal is an anecdote; a result in the repository is something the next change
can be compared against.

```sh
scripts/bench.sh          # writes benchmarks/<host>.json
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

## Adding a host

Nothing to configure. `bench.sh` finds the render node itself, and every
provenance field degrades to `null` on a machine that does not have it, so a new
host needs no code change — which is the point, since these numbers have to be
re-taken on every new GPU.

See `../docs/BENCHMARKS.md` for what each number means, and §8 there for the
measurement rules that produced it.
