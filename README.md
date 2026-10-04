# AI Session Manager

A terminal browser for Claude Code session logs.

Claude Code keeps every conversation as a `.jsonl` file under `~/.claude/projects/`, one
directory per project. Those files pile up, are named by uuid, and are effectively opaque.
This indexes them all at startup and puts them in a tree you can search, read and prune.

## Install

Grab the latest static Linux x86-64 binary. This URL always resolves to the
newest release:

```sh
wget https://github.com/cpp-serg/ai-session-manager/releases/latest/download/ai-session-manager
chmod +x ai-session-manager
./ai-session-manager
```

Or as a one-liner into `~/.local/bin`:

```sh
wget -qO ~/.local/bin/ai-session-manager \
  https://github.com/cpp-serg/ai-session-manager/releases/latest/download/ai-session-manager \
  && chmod +x ~/.local/bin/ai-session-manager
```

The binary is statically linked against musl, so it runs on any distribution
regardless of glibc version and has no runtime dependencies. A `.sha256` is
published next to it, along with a versioned `.tar.gz` that also carries the
README and LICENSE.

For any other platform, build from source with the steps below — the dependency
tree is pure Rust, so nothing beyond a Rust toolchain is required.

## Build and run

```sh
cargo build --release
./target/release/ai-session-manager          # sessions for the current directory
./target/release/ai-session-manager --all    # every session on the host
./target/release/ai-session-manager --help   # full key list
```

It starts scoped to the directory you launch it from, and falls back to showing everything
if that directory has no sessions. `-a`/`--all` only picks the starting scope; `ctrl-a`
switches at any time.

## What you see

```
  last started         title                             msgs branch
  ▾ /home/dev/webapp ─────────────── 8 sessions + 3 subagents · 4.2k msgs · 2h
    2h 2026-01-14  ├─  Refactor the auth middleware       231 main
    1d 2026-01-13  ├▾  Investigate flaky checkout tests   412 fix/checkout
    1d 2026-01-13   ├─ Explore how the test fixtures b…    47 fix/checkout
    1d 2026-01-13   └─ Find every retry helper in the …    33 fix/checkout
    6d 2026-01-08  └─  Port the CLI to the new config …   180 main
  ▾ /home/dev/notes ──────────────────────────────── 2 sessions · 96 msgs · 3w
```

Sessions group under their project directory, newest first, and directories sort by their
most recent session. Subagent transcripts nest under the session that spawned them and
start folded. The age column is colored: green within a week, cyan within a month, orange
within three, red beyond.

## Keys

Navigation is vim-style, and arrow keys work everywhere too.

| | |
|---|---|
| `j` `k` `h` `l` | move, fold, unfold |
| `gg` `G` | first / last row |
| `ctrl-f` `ctrl-b` | page down / up |
| `ctrl-d` `ctrl-u` | half page down / up |
| `-` | fold or unfold the node under the cursor |
| `enter` | open the session, or fold the directory |
| `/` | filter; `enter` accepts, `esc` cancels |
| `esc` | clear marks, then the filter |
| `space` | mark for deletion; a session takes its subagents, a directory inverts its children |
| `d` `del` | delete what is marked, else the selected session (`y`/`enter` confirms) |
| `c` | resume it: replaces this app with `claude -r <id>`, run from the project's directory |
| `r` | rename the selected session |
| `ctrl-e` | fold or unfold everything |
| `ctrl-r` | fold or unfold every session node |
| `ctrl-a` | current directory / all sessions |
| `ctrl-t` | match titles / full text |
| `f1` | key help |
| `q` `ctrl-c` | quit |

In the transcript viewer: `j`/`k` scroll, `ctrl-f`/`ctrl-b` page, `gg`/`G` jump, `t` shows
thinking blocks, `o` expands truncated tool output, `esc` or `q` returns to the tree.

## Notes

**Search.** Filtering matches titles by default and the entire transcript with `ctrl-t`.
It is fuzzy, with smart case. A renamed session still matches the name it used to have.

**Renaming** appends the same record `/rename` writes, so it is compatible with Claude Code
and never rewrites existing history. It does update the file's timestamp, so the row moves
to the top of its group.

**Deleting** removes the session file and its subagent directory, and always asks first.
Marks are remembered by path, so a session marked before you filter or fold stays marked.

**Performance.** Everything is read once at startup and held in memory, so filtering,
full-text search and opening a transcript never touch the disk. A 46 MB corpus of 45 logs
loads in well under a second.

## Releasing

`ci.yml` builds, lints and tests every push and pull request. `release.yml`
fires on a version tag, builds the static Linux binary and publishes a GitHub
release with the archive attached:

```sh
git tag -a v0.1.0 -m "v0.1.0"
git push origin v0.1.0
```

Keep the tag and the `version` in `Cargo.toml` in step; the tag name is what
ends up in the archive name.

## License

MIT © 2026 Sergey Pastukhov <sergey.pastukhov@gmail.com>. See [LICENSE](LICENSE).
