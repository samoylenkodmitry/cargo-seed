# cargo-seed

Each new git worktree of a Rust project builds all crates from nothing and
keeps its own full `target/`. cargo-seed gives a new worktree a copy of a
warm target instead, and removes build output that no build uses.

## Seeds

A seed is a locked worktree at the remote's default branch, built with the
commands you run. `git worktree add` copies the seed's `target/` into the new
worktree. On APFS, btrfs, XFS and ReFS the copy shares disk blocks with the
seed; elsewhere it is a plain copy.

Cargo then rebuilds only what differs from the seed:

- A file equal to the seed's committed file gets the seed's mtime. Other
  files keep the time git wrote them with, so their crates rebuild.
- Records that list a seed file by absolute path move to the new worktree.
- A build script that watches a seed file by absolute path runs again.
- A crate that reads `CARGO_MANIFEST_DIR` with `env!` rebuilds: its code holds
  the path, and cargo does not track that variable
  ([cargo#8693](https://github.com/rust-lang/cargo/issues/8693)).

```sh
cargo install cargo-seed
git config --add seed.warm "cargo build --workspace --all-targets"
git config --add seed.warm "cargo clippy --workspace --all-targets"
cargo seed refresh        # make the first seed
cargo seed install-hook   # seed every new worktree
```

When the remote branch moves past the seed, the hook starts
`cargo seed refresh` in the background. A refresh copies the current seed to
a new one at the new commit, builds it, removes unused output from it, and
then removes the old seed.

| git config | |
|---|---|
| `seed.warm` | a command a seed is built with; repeatable. Default `cargo build --workspace --all-targets` |
| `seed.ref` | the commit seeds follow. Default `origin/HEAD` |
| `seed.autoRefresh` | `false`: the hook does not refresh |
| `seed.path` | the current seed; `refresh` writes it |

`install-hook` writes only `.git/hooks/post-checkout`. If `core.hooksPath` is
set, it prints the line to add there.

## Unused build output

`cargo seed prune --root ~/projects --apply` removes from every cargo target
under the folder:

- incremental state that no build touched for an hour (`--idle-minutes`)
- builds that a newer build of the same unit replaced: after a dependency
  update or a new compiler, cargo builds the unit again under a new hash and
  keeps the old one

It skips seeds and targets built in the last 15 minutes. A removed build
costs a rebuild if it is needed again. Without `--apply` it only reports.

## Parallel front end

`rustc-threads` runs rustc's parallel front end on the stable compiler for
workspace crates in unoptimized builds. Release builds and dependencies run
unchanged. `RUSTC_BOOTSTRAP` is set for the one crate being compiled.

```toml
# ~/.cargo/config.toml
[build]
rustc-wrapper = "/path/to/.cargo/bin/rustc-threads"
```

## Measured

On a 41-crate workspace with 698 packages:

| | seeded | cold |
|---|---|---|
| workspace clippy, loaded Mac | 6.8 s | 13 min 49 s |
| app and tests build, Linux | 40 s | 123 s |

- Seeded and cold binaries were byte-identical without debug sections.
- `rustc-threads`: one large crate's check took 2.1 s instead of 3.5 s, and
  its build 3.2 s instead of 5.9 s; the final binary was identical.
- `prune` removed 12.3 GB from one 42 GB target.

Tested on macOS and Linux. Windows builds, but is not tested.
