# Zohara Terminal

A universal terminal application for Arch-based Linux, built to serve
beginners, everyday users, developers, and power users from one codebase.
The full product vision is in [`docs/SPEC.md`](docs/SPEC.md).

## Architecture

The workspace is layered, and dependencies only ever flow one direction:
**core → services → plugins → app**. Nothing in `core` knows about a
service; nothing in a service knows about a plugin. This is what keeps a
20+-crate workspace navigable instead of a dependency tangle.

```
crates/
├── core/            # pure, OS-agnostic-except-ut-pty, no plugin/UI knowledge
│   ├── ut-pty        # spawn a shell behind a real PTY — the only unsafe/OS-process crate
│   ├── ut-ansi        # ANSI/VT byte stream → typed AnsiEvents (vte-based, pure parsing)
│   ├── ut-grid         # AnsiEvents → cell grid, cursor, scrollback, modes (Terminal)
│   ├── ut-config        # typed profile/appearance schema: load, merge (inheritance), validate
│   └── ut-safety          # destructive-command & safe-paste analysis
│
├── services/        # stateful, own a domain, sit on top of core
│   ├── ut-session     # owns PTY-backed panes/tabs/workspaces — THE safety-checked write path
│   ├── ut-history       # command history store + search (text/dir/exit-status/date/favorites)
│   ├── ut-search          # scrollback/output search — literal or regex
│   ├── ut-profile           # loads/saves ut-config atomically, resolves the active profile
│   └── ut-notify               # notification dispatch, suppression rules, history
│
├── plugins/         # permission-scoped, depend only on ut-plugin-api + core
│   ├── ut-plugin-api  # the Plugin trait + PluginContext — the enforcement point for permissions
│   ├── ssh              # HostProfile model + ~/.ssh/config import
│   ├── package-manager     # pacman/checkupdates output parsing (update previews, tx size)
│   ├── workflow               # command-sequence engine: variables, retry, timeout, continue/stop
│   └── ai                        # offline command explainer + ut-safety risk annotation
│
└── app/             # toolkit-facing; a real GUI would live here
    ├── ut-input       # keyboard event → PTY byte sequence (xterm-style encoding)
    ├── ut-renderer      # Grid → ANSI text reproduction + damage/diff tracking
    ├── ut-ui              # pane split-tree layout (splits, resize, zoom) — pure data
    ├── ut-cli               # `zohara check`/`zohara paste` — safety-engine CLI
    └── ut-app                 # `run` — spawns a real command through the whole core stack
```

### Why the split is where it is

- **`ut-ansi` / `ut-grid` are separate crates.** The parser only knows the
  *shape* of escape sequences; the grid only knows what a terminal *is*.
  Either can be tested or reused independently — `ut-grid`'s tests build
  `AnsiEvent`s by hand as well as through `ut-ansi`, proving the split is real.
- **`ut-safety` lives in `core`, not as an optional service.** Command
  safety has to run on every path that can write to a shell, synchronously,
  or it's not a guarantee. `ut-session::Pane::submit_input` is the *only*
  place in the codebase that writes to a PTY, and it always runs input
  through `ut-safety` first — see `crates/services/ut-session/src/lib.rs`.
- **Plugins only see a `PluginContext`, never a service directly.** A
  plugin's `Permissions` (read-only by default) gate every method on that
  context; widening what a plugin can do means widening `PluginContext`,
  in one reviewable place, not auditing every plugin.
- **`app/*` is the only layer allowed to assume a runtime/toolkit shape.**
  `ut-input`/`ut-renderer`/`ut-ui` are still toolkit-agnostic (no GTK/wgpu/
  whatever chosen yet), but they're where that choice will land — `core`
  and `services` never need to change when it does.

### See it work end-to-end

```
cargo run -p ut-app -- run -- /bin/sh -c 'echo hello from ut-app'
cargo run -p ut-app -- run -- rm -rf /        # refused: ut-safety flags it, exits 1
cargo run -p ut-app -- run --force -- ls -la  # --force bypasses the gate
```

`ut-app run` spawns the given program behind a real `ut-pty`, feeds every
byte of its output through `ut-ansi`/`ut-grid`, and prints the resulting
screen once the program exits — proving the parser, the grid, and the PTY
layer are wired together correctly, not just unit-tested in isolation.

## Building

```
cargo build --workspace
cargo test --workspace     # 191 tests across 19 crates, all offline/hermetic
cargo clippy --workspace --all-targets
```

Requires a recent stable Rust toolchain (edition 2021). Unix-only
(`ut-pty` uses `nix`/`libc` directly; Arch Linux is the target platform).

## Design principles carried into the code

- **Local-first, no network calls.** Every crate's test suite is offline;
  `ut-pty`'s tests spawn real local processes (`/bin/sh`, `/bin/cat`), never
  anything networked.
- **Conservative, static safety analysis.** `ut-safety` never executes
  anything — it's a lexer plus a rule engine, aiming for zero false
  negatives on well-known footguns (`rm -rf /`, `dd of=/dev/sdX`, `curl|sh`)
  while keeping false positives low.
- **Never execute silently.** `ut-plugin-api::PluginContext` structurally
  prevents a plugin from running a command without going through the same
  `ut-safety` check and explicit user action as typed input — see
  `PluginAction` and `PluginContext::suggest_input`.

## What's not here yet

A real GUI (windows/tabs/tray, themes rendered on screen, the command
palette, on-screen keyboard toolbar), SSH's actual network layer and SFTP
browser, the built-in multiplexer's persistence, workflow editors/exporters,
model-backed AI (local/remote providers — the offline explainer is the
floor those build on), and the signed-plugin sandbox are all future work
described in [`docs/SPEC.md`](docs/SPEC.md). The crate layout above is
built so each of those is an addition to one crate, not a restructuring.
