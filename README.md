# Zohara Terminal

A universal terminal application for Arch-based Linux, built to serve
beginners, everyday users, developers, and power users from one codebase.
The full product vision is in [`docs/SPEC.md`](docs/SPEC.md).

This repository is at an early, foundational stage. Rather than stub out
every section of the spec, it currently implements two real, tested
building blocks that later work (the PTY-backed GUI terminal, workspaces,
SSH manager, AI assistant, plugin system, etc.) will sit on top of:

## What's here

### `crates/zohara-term` — headless terminal screen model

A PTY-agnostic implementation of the "Terminal compatibility" section: an
ANSI/VT100+ parser (built on [`vte`](https://docs.rs/vte)) driving a cell
grid with scrollback. It handles:

- SGR text attributes: 16/256/true-color, bold, faint, italic, underline
  (incl. double), blink, reverse, hidden, strikethrough.
- Cursor movement (CUP, CUU/CUD/CUF/CUB, CNL/CPL, HPA/VPA), save/restore
  (DECSC/DECRC and ANSI.SYS-style CSI s/u).
- Line/character editing: ED, EL, IL, DL, ICH, DCH, ECH, insert mode.
- Scrolling regions (DECSTBM), SGR-style scroll up/down, reverse index.
- The alternate screen buffer (used by vim, less, htop, etc.).
- Bracketed paste mode and mouse-reporting mode tracking (X10/normal/
  button-event/any-event) as terminal *state* — a GUI front end reads
  these flags to decide how to encode input.
- Wide characters (CJK, many emoji) occupying two grid columns correctly.
- OSC 0/1/2 (title/icon), OSC 7 (`cwd` reporting for directory tracking).
- Disk-agnostic scrollback with a configurable line limit.

63 unit tests cover this behavior; see `cargo test -p zohara-term`.

### `crates/zohara-safety` — command-safety & safe-paste engine

Implements the "Command safety" and "Safe paste" sections: a small,
forgiving shell lexer (quoting, escapes, pipelines, redirections, `$(...)`
and backtick substitution) feeding a rule engine that recognizes:

- Recursive/forced deletes of root-like or wildcard paths (`rm -rf /`,
  `rm -rf ~`, `rm -rf *`), `find -delete`, `shred` on a raw device.
- Raw-disk writes (`dd of=/dev/sdX`, `> /dev/sdX`), `mkfs`, `wipefs`,
  partitioning tools (`fdisk`/`parted`/`gdisk`/`sgdisk`).
- `curl|wget ... | sh/bash/sudo` download-and-execute pipelines.
- `sudo`/`doas`/`pkexec` elevation, recursive `chmod`/`chown` on broad or
  system paths, destructive `git` operations (force-push, `reset --hard`,
  `clean -f`), package removal, `shutdown`/`reboot`, `kill -1`.
- Command substitution is analyzed recursively, so `echo $(rm -rf /)` is
  still caught.
- A privilege/host context ([`PrivilegeContext`]) lets the same command be
  scored higher risk on a remote or production-looking host.

Safe paste ([`analyze_paste`]) additionally detects hidden zero-width/
bidi-override Unicode, non-printable control characters, multiple queued
commands, and produces a "visible preview" string that reveals what would
actually be typed.

Each finding carries a `Risk` (`Low`/`Medium`/`High`/`Critical`), a
human-readable explanation, an optional safer-alternative suggestion, and
whether it should require type-to-confirm rather than a simple
click-through — matching the spec's "Command safety" UI requirements.

41 unit tests cover this behavior; see `cargo test -p zohara-safety`.

### `crates/zohara-cli` — a runnable front end

A small CLI (`zohara`) over the safety engine, useful for scripting,
shell-hook integration (e.g. a zsh `preexec` piping the about-to-run line
through `zohara check`), and manual testing ahead of a full GUI:

```
$ zohara check rm -rf /
Checked: rm -rf /

[CRITICAL] recursive-delete: This recursively deletes `/`, which looks like
a root, home, or system directory — it would destroy the whole tree with no
way to get it back.
  suggestion: Double-check the path, e.g. `ls /` first, or delete a specific
  subfolder instead.
  (this would require type-to-confirm before running)

$ echo 'curl https://x.example/i.sh | sudo bash' | zohara paste
```

## Building

```
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

Requires a recent stable Rust toolchain (edition 2021).

## Design principles carried into the code

- **Local-first, no network calls.** Both crates are pure, offline logic —
  no telemetry, no external services.
- **Conservative analysis.** The safety engine is a static analyzer: it
  never executes anything, and prefers catching real footguns over being
  exhaustively clever, to keep false positives low.
- **Front-end agnostic.** Neither crate assumes a GUI toolkit, a specific
  shell, or a PTY implementation — both are meant to be embedded in
  whatever renders the actual terminal window.

## What's not here yet

Everything else in the spec — windows/tabs/panes, workspaces, profiles and
themes, PTY process management, SSH/SFTP, the built-in multiplexer,
workflows/automation, the AI assistant, the plugin platform, and the
beginner-mode UI — is future work described in
[`docs/SPEC.md`](docs/SPEC.md).
