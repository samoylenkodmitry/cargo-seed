# cargo-seed

Each new git worktree of a Rust project builds all crates from nothing and
keeps its own full `target/`. cargo-seed gives a new worktree a copy of the
target of the closest worktree of the same repository, and removes build
output that no build uses. Nothing is built ahead of time.

## New worktrees

`git worktree add` copies the target of the worktree whose files differ
least from the new one and that has no build running. On APFS, btrfs, XFS
and ReFS the copy shares disk blocks with it; elsewhere it is a plain copy.
Cargo then rebuilds only what differs:

- A file equal to the donor's committed file gets the donor's mtime. Other
  files keep the time git wrote them with, so their crates rebuild.
- Records that list a donor file by absolute path move to the new worktree.
- A build script that watches a donor file by absolute path runs again.
- A crate that reads `CARGO_MANIFEST_DIR` with `env!` rebuilds: its code holds
  the path, and cargo does not track that variable
  ([cargo#8693](https://github.com/rust-lang/cargo/issues/8693)).

```sh
cargo install cargo-seed
cargo seed install-hook   # in each repository
```

`install-hook` writes only `.git/hooks/post-checkout`. If `core.hooksPath` is
set, it prints the line to add there.

## Unused build output

```sh
cargo seed maintain --root ~/projects   # e.g. hourly
```

runs these three, which also work alone (without `--apply` they only report):

- `gc`: removes whole targets, least recently built first: every target
  unused for 3 days (`--max-idle-days`), then more until 50 GB are free
  (`--min-free-gb`). A repository nobody works on costs no disk.
- `prune`: removes incremental state no build touched for an hour, and builds
  that a newer build of the same unit replaced, such as the old build left by
  a dependency update.
- `dedupe`: makes identical build files across targets share disk blocks.

Targets with a build running or finished recently are left alone. A removed
target or build costs a rebuild if it is needed again.

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

| | copied target | cold |
|---|---|---|
| workspace clippy, loaded Mac | 6.8 s | 13 min 49 s |
| app and tests build, Linux | 40 s | 123 s |

- Binaries from a copied target and from a cold build were byte-identical
  without debug sections.
- `rustc-threads`: one large crate's check took 2.1 s instead of 3.5 s, and
  its build 3.2 s instead of 5.9 s; the final binary was identical.
- `prune` removed 12.3 GB from one 42 GB target.

Tested on macOS, Linux and Windows.
