# copybera

copybera copies a repository, or part of one, into another repository under a rules file. It is a
[Copybara](https://github.com/google/copybara)-style transformer written in Rust: a workflow names an origin, a
destination and an ordered list of transformations, and copybera walks the origin's history, applies the rules and
writes the result as one squashed commit or commit for commit.

copybera is an independent project; it is not affiliated with Google or with the Copybara project.

Its first job was publishing a public tree out of a private monorepo: keep the parts that ship, strip the internal
planning files and ticket references, rename what moved, and refuse to write anything that still carries a private
name, path or address.

## What it does

- **Workflows are TOML files.** One file is one workflow: `[workflow]` (name, `squash` or `iterative` mode, dry run,
  receipt), `[origin]` and `[destination]` (a local path or a git URL, a ref, credentials by reference only),
  `[paths.origin]` and `[paths.destination]` include and exclude globs, `[authoring]` (a default author and an
  author map) and `[[transformations]]`. `copybera validate` checks a file without touching git.
- **The transformation set is closed.** `strip_paths`, `strip_references` (ticket ids and the parenthetical or list
  separator around them), `rewrite_messages` (keeps each message's first line with references stripped; a model
  hook is reserved in the schema, `hook = "low_cost_model"`, but not wired, so the fallback always runs),
  `replace_text` and `replace_pairs` (scoped by paths, with lines to leave alone), `move_paths` and
  `rename_paths`, `strip_matching`, `strip_comments` (per language, keeping doc comments by default) and
  `drop_empty_commits`. There is no scripting language: a rule is data, so a workflow reads the same in review as
  it runs.
- **Every run leaves a receipt**: commits read, written and dropped, files kept and stripped, the origin range, the
  last origin commit, and each drop with its reason. The destination commit carries the origin commit it came from,
  so the next run continues from there.
- **It checks what it wrote.** `copybera lint` walks the destination's history and refuses ticket ids (the
  workflow's `strip_references` patterns, such as `TICKET-[0-9]+` in the example, or the patterns `[lint] ids`
  names), excluded paths and any match of a private-patterns file (one regex per line, kept outside git; a finding
  names the pattern's number, never the text). `copybera gate` scans the exported tree by class (home and volume paths, emails, LAN
  addresses, tokens, secret references, private terms and paths, leftover names, licences and notices), and every
  allowance must name a reason. `[verify]` runs commands such as `cargo check` on the exported tree before anything
  is written, and `[cascade] rust = "auto"` repairs a Cargo workspace whose members a strip removed.
- **The rest of the toolbox**: `copybera subjects` reads a range's commit subjects against the private patterns
  before a squash carries them, `copybera closure` prints the files a set of Cargo or JavaScript packages is built
  from (so an include list can be exactly that), `copybera doctor` and `copybera status` report on workflows and
  their receipts, and `copybera migrate --schedule` repeats a migration.

## The memory cap: copybera refuses to run uncapped

copybera holds a window of origin trees in memory while it walks a history. A large history walked without a limit
can take a workstation's memory faster than anyone can stop it, and a watchdog that samples memory use reacts too
late. So copybera runs only where the kernel enforces a hard limit: before it does anything, `--help` included, it
reads the memory limit of its own cgroup (`memory.max`, or cgroup v1's `memory.limit_in_bytes`) and the host's RAM.
With no limit, or a limit that is not below the RAM, it refuses with exit code 78. macOS has no cgroups, so on a Mac
it runs inside a Linux container. The `Dockerfile` in this repository builds one (a Rust build stage, then a slim
runtime with `git` and the binary as its entry point):

```sh
docker build -t copybera .
docker run --rm --memory=1536m --memory-swap=1536m --user "$(id -u):$(id -g)" -e HOME=/tmp \
    -v "$PWD":/work copybera migrate publish/publish-subtree.toml --dry-run
```

The container runs as your user (`--user`) so git accepts the mounted repositories as yours and the files it writes
are yours; `HOME=/tmp` gives git a writable home. Everything a workflow names must sit under the mounted folder.

`--memory` and `--memory-swap` set to the same value make the cap hard (no swap past it). 1.5 GiB has been enough for
a squash of several thousand commits with the default window of 32 commits (`COPYBERA_TREE_WINDOW` changes it).
`COPYBERA_ALLOW_UNCAPPED=1` starts it without a cap and prints a warning; it is meant for debugging, not for runs.

## Building

copybera builds with Rust 1.91 (pinned in `rust-toolchain.toml`) and needs `git` on the PATH at run time.

```sh
cargo build --release --locked
```

The binary is `target/release/copybera`. The test suite runs that binary, so it obeys the cap too: run the tests in
a capped Linux container, for example the official Rust image with the source mounted (`rust-toolchain.toml` pulls
clippy and rustfmt in on first use):

```sh
docker run --rm --memory=8g --memory-swap=8g -v "$PWD":/src -w /src rust:1.91.0-bookworm \
    sh -c 'cargo fmt --all --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked'
```

The workspace has two crates: `copybera-rules` (the parser and the transformation engine, pure functions with no git,
network or model calls) and `copybera` (the command line, git I/O, receipts, lint and gate).

## Quickstart

Work in the folder that holds your private checkout (here `private-monorepo/`), with the image built as above and a
shell function that runs each command in it under the cap:

```sh
copybera() {
    docker run --rm --memory=1536m --memory-swap=1536m --user "$(id -u):$(id -g)" -e HOME=/tmp \
        -v "$PWD":/work copybera "$@"
}
```

1. Make the destination and the workflow folder: `git init --bare public-mirror.git`, `mkdir publish`, then copy
   `examples/publish-subtree.toml` and `examples/gate.toml` into `publish/`. The workflow's paths are read from its
   own folder: `[origin] path = "../private-monorepo"` and `[destination] path = "../public-mirror.git"` already
   point at the layout above. Set `ref` to the release to publish, `since` to the previous one (or drop it to read the
   whole history), and the include list to what ships.
2. Check the file: `copybera validate publish/publish-subtree.toml`.
3. See what would move: `copybera migrate publish/publish-subtree.toml --dry-run`, then read the receipt it wrote
   (`publish/receipts/publish-subtree.json`).
4. Run it: `copybera migrate publish/publish-subtree.toml`. The destination gets one commit; nothing is pushed
   anywhere.
5. Check it: `copybera lint public-mirror.git --workflow publish/publish-subtree.toml --count` and
   `copybera gate publish/publish-subtree.toml --config publish/gate.toml --files`.
6. Read the mirror (`git --git-dir=public-mirror.git log -p`), then push it to the public repository yourself.

`examples/mirror-iterative.toml` shows the other mode: a documentation folder mirrored commit for commit.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as
defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
