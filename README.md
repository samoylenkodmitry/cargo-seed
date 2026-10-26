# cargo-seed

Every new git worktree of a Rust project normally builds the whole dependency
graph from nothing and keeps a full `target/` of its own. With several agents
or people each working in their own worktree, that is most of the machine's
CPU and disk.

`cargo seed` keeps one *seed*: a locked worktree at the remote's default
branch whose `target/` holds what you build. A new worktree gets a
copy-on-write clone of that target (one `clonefile` call on APFS, reflinks on
btrfs and XFS, block cloning on ReFS, a plain copy elsewhere), and the clone
is corrected so cargo rebuilds exactly what differs from the seed:

- A file or directory whose committed content equals the seed's, and which
  neither tree has edited, gets the seed's mtime. Every other path keeps the
  time git wrote it with, so its units rebuild.
- Cargo's fingerprint records that list a seed file by absolute path are
  pointed at the new tree, with their own mtimes kept.
- A build script that watches a seed source by absolute path reruns.
- A unit whose code holds a seed path rebuilds. rustc reports every variable a
  crate read with `env!`, and cargo leaves the ones it sets itself, such as
  `CARGO_MANIFEST_DIR`, out of its fingerprint
  ([cargo#8693](https://github.com/rust-lang/cargo/issues/8693)).

## Use

```sh
cargo install cargo-seed
cd my-repo
git config --add seed.warm "cargo build --workspace --all-targets"
git config --add seed.warm "cargo clippy --workspace --all-targets"
cargo seed refresh        # makes the first seed and warms it
cargo seed install-hook   # `git worktree add` now seeds every new worktree
```

The hook also starts a background `cargo seed refresh` when the followed
branch has moved past the seed. A refresh clones the current seed into a new
one at the new commit, warms it, and removes the old seed once no clone reads
it.

| git config | meaning |
|---|---|
| `seed.warm` | a command run in a new seed; repeat the key for more (default `cargo build --workspace --all-targets`) |
| `seed.ref` | the commit seeds follow (default `origin/HEAD`) |
| `seed.autoRefresh` | `false` stops the hook from refreshing a seed that fell behind |
| `seed.path` | the current seed, written by `refresh` |

`install-hook` writes only the repository's own `.git/hooks/post-checkout`.
When `core.hooksPath` is set, it prints the line to add there.

## Measured

On a 41-crate, 698-package workspace (Cranpose):

| | seeded | cold |
|---|---|---|
| workspace clippy, loaded M-series Mac | 6.8 s | 13 min 49 s |
| app and test build, Linux, same commit as the seed | 40 s | 123 s |
| same build, tree 6 merges behind the seed | 50 s | 174 s |

The seeded and cold binaries were byte-identical with debug sections and the
build id removed. A seeded worktree shares the seed's disk blocks until it
rebuilds something.
