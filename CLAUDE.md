# nesbox

A microVM that boots in about a second and renders on a real GPU. **This
repository is public**, and that is the single most important fact about it.

## THE RULE THAT COMES BEFORE EVERY OTHER RULE

**This repo is public. Write nothing that only makes sense to us.**

Read this before writing a single comment, docstring, script message or commit
message. It has been violated twice across our public repos, both times by
someone who knew the repo was public, both times about ten occurrences deep
before anyone noticed — once in *this* tree, in `cpuid.rs`, `config.rs`,
`PROGRESS.md` and a setup script's operator output. Being careful is
demonstrably not a mechanism, so what follows is mechanical.

**Never, anywhere in this repo:**

| ✗ | why |
|---|---|
| **Any relative path that escapes this tree** into an internal repo, or the filename of an internal document | A filename plus a title tells a reader exactly what to ask for |
| A quotation from an internal document, even one phrase | Restate the requirement in this repo's own words |
| The name of a component with no public surface | It discloses the shape of the system, which is the part deliberately kept closed |
| Anything of the above in a **commit message** | History here is permanent and is deliberately never rewritten — a message cannot be fixed by a later commit |
| Anything of the above in **published output** — a script's console text, an error message, a README, `PROGRESS.md` | It reaches people who never open the source. Check where a string *goes*, not what file it is in |

**The one sanctioned exception**, and the only way to cite internal reasoning:

```rust
// A guest is placed inside one L3 domain, so its cores share a cache. ref(d-0024)
```

`ref(d-NNNN)` · `todo(d-NNNN)` · `fixme(d-NNNN)`, in **source comments only**.
Note `d-NNNN` and never `d/NNNN` — a slash reads as a path, and a path is the
thing this rule exists to stop.

**The test that makes it decidable — apply it to every sentence:**

> Delete the marker. Does the comment still say something true and useful about
> *this* code?

If yes, it belongs. If the sentence collapses without the reference, it was
describing our topology rather than this component, and the fix is to state the
**requirement** instead of who set it.

In practice a category noun does it — *the caller*, *a supervising agent*, *the
allocator*, *an orchestrator*, *the control plane*. The result is a better
sentence every time, because it says what is required rather than who currently
satisfies it. `cpuid.rs` now says a guest is placed inside one L3 domain, which
is the actual constraint, instead of naming who places it. **The leak buys
nothing.**

**A name a user types is not a leak.** `nessh` is a product surface — the
product *is* `ssh nestri.io` — so it appears in public artefacts on purpose. The
test narrows to: does this name appear because a **user** encounters it, or
because a **component** does?

If you are unsure whether a name is internal, do not guess and do not go looking
for permission — write the category noun. It is never wrong.

**Nothing closed may enter this repo**, either: not source, not a dependency,
not a directory that looked convenient.

## What this is

A VMM built on `rutabaga_gfx` for GPU by native context, `io_uring` multiqueue
block, vhost-net, vsock and virtio-fs. It boots an artifact already on disk and
starts a payload it is not allowed to understand.

**It does not branch on what it runs.** The box starts a workload the open
components are deliberately ignorant of, so no code here may test which one it
is. Comments may name a real case that motivated a workaround — that is a name
in prose, never a dependency in code.

| | |
|---|---|
| `cargo build --release` | build |
| `cargo test --workspace` | tests |
| `make materialize` in `build/` | a jail image for `tools/jailer` |
| `scripts/bench.sh` | the public benchmark harness |

`docs/SECURITY.md` is the honest account of what the jail bounds and what it
does not. Read it before changing anything under `tools/jailer`.

## Conventions

Conventional commits. Explain *why* in the body — the diff already shows what.
Comments earn their place by saying something the code cannot.

**A commit message here is public and permanent.** No internal component names,
no decision numbers, no `ref(d-…)` markers — those are for source comments,
where a later commit can fix a mistake. Describe the change in this repo's own
terms. See the rule at the top.
