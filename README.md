# Zohara Terminal

A universal terminal application for Arch-based Linux, built to serve
beginners, everyday users, developers, and power users from one codebase.
The full product vision is in [`docs/SPEC.md`](docs/SPEC.md).

## Architecture

The workspace is layered, and dependencies only ever flow one direction:
**core → services → plugins → app**. Nothing in `core` knows about a
service; nothing in a service knows about a plugin. This is what keeps a
24-crate workspace navigable instead of a dependency tangle.

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
│   ├── ut-daemon        # named sessions that outlive a client — detach/reattach over a Unix socket
│   ├── ut-history          # command history store + search (text/dir/exit-status/date/favorites)
│   ├── ut-search              # scrollback/output search — literal or regex
│   ├── ut-profile                # loads/saves ut-config atomically, resolves the active profile
│   └── ut-notify                    # notification dispatch, suppression rules, history
│
├── plugins/         # permission-scoped, depend only on ut-plugin-api + core
│   ├── ut-plugin-api  # the Plugin trait + PluginContext — the enforcement point for permissions
│   ├── ut-plugin-signing # ed25519 manifest signing/verification against a trusted keyring
│   ├── ssh                 # HostProfile + ~/.ssh/config import + a real ssh2-backed SshClient
│   ├── package-manager        # pacman/checkupdates output parsing (update previews, tx size)
│   ├── workflow                   # command-sequence engine: variables, retry, timeout, continue/stop
│   └── ai                            # offline explainer + optional Ollama-backed NetworkAssistant
│
└── app/             # toolkit-facing
    ├── ut-input       # keyboard event → PTY byte sequence (xterm-style encoding)
    ├── ut-renderer      # Grid → ANSI text reproduction + damage/diff tracking
    ├── ut-ui              # pane split-tree layout (splits, resize, zoom) — pure data
    ├── ut-tui               # a real interactive multiplexer UI (ratatui) over ut-session + ut-ui
    ├── ut-cli                 # `zohara check`/`zohara paste` — safety-engine CLI
    └── ut-app                    # `run` — spawns a real command through the whole core stack
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
  `ut-tui` builds on this with a best-effort local line tracker so the same
  gate applies to keystrokes typed into the interactive UI (see that
  crate's module docs for the real, documented limits of that approach on
  a raw PTY).
- **Plugins only see a `PluginContext`, never a service directly.** A
  plugin's `Permissions` (read-only by default) gate every method on that
  context; widening what a plugin can do means widening `PluginContext`,
  in one reviewable place, not auditing every plugin. `ut-plugin-ai`'s
  `NetworkAssistant` is the concrete example: it requests `network`
  explicitly and checks it again at the call site before ever reaching the
  network.
- **`ut-daemon` is what makes "the multiplexer" real, not just a data
  model.** Each named session is a genuine background thread pumping a
  real PTY, independent of any attached client — detach/reattach is a
  socket disconnect/reconnect, not a special mode.
- **`app/*` is the only layer allowed to assume a runtime/toolkit shape.**
  `ut-tui` is that layer's first real occupant: an interactive terminal-
  hosted UI (not yet a native windowed GUI — still future work) built on
  `ratatui`, wired straight into `ut-session`/`ut-ui`/`ut-input`.

### See it work end-to-end

```
cargo run -p ut-app -- run -- /bin/sh -c 'echo hello from ut-app'
cargo run -p ut-app -- run -- rm -rf /        # refused: ut-safety flags it, exits 1
cargo run -p ut-app -- run --force -- ls -la  # --force bypasses the gate

cargo run -p ut-tui   # a real interactive multiplexer: splits, zoom, close, destructive-command
                       # confirmation prompts, all over real shells. Ctrl+B is the prefix key
                       # (", -, x, z, q) — see crates/app/ut-tui/src/main.rs for the full list.
```

`ut-app run` spawns the given program behind a real `ut-pty`, feeds every
byte of its output through `ut-ansi`/`ut-grid`, and prints the resulting
screen once the program exits — proving the parser, the grid, and the PTY
layer are wired together correctly, not just unit-tested in isolation.
`ut-tui` proves the same for the full interactive stack, including
`ut-daemon`'s session model when driven through its socket protocol.

## Building

```
cargo build --workspace
cargo test --workspace     # 246 tests across 24 crates, all offline/hermetic
cargo clippy --workspace --all-targets
```

Requires a recent stable Rust toolchain (edition 2021), a C toolchain +
`libssl-dev` (for `ut-plugin-ssh`'s `ssh2`/libssh2 binding), and is
Unix-only (`ut-pty` uses `nix`/`libc` directly; Arch Linux is the target
platform).

`ut-plugin-ssh`'s live-connection tests (`SshClient::connect`/`exec`
against a real server) are `#[ignore]`d by default so `cargo test
--workspace` never depends on network access or a running `sshd` — they
were verified in development against a real local test `sshd`. Run them
yourself against your own test server with:

```
cargo test -p ut-plugin-ssh -- --ignored
# (reads UT_SSH_TEST_HOST/_USER/_KEY/_PORT env vars, defaulting to
#  127.0.0.1:2222 / root / /tmp/sshd_test/client_key)
```

## Design principles carried into the code

- **Local-first, no network calls in the default test suite.** `ut-pty`'s
  and `ut-session`'s tests spawn real local processes (`/bin/sh`,
  `/bin/cat`); `ut-plugin-ai`'s Ollama-client tests spin up a one-shot
  local mock HTTP server rather than needing a real Ollama install;
  `ut-plugin-ssh`'s only tests that touch a network are `#[ignore]`d, per
  above.
- **Conservative, static safety analysis.** `ut-safety` never executes
  anything — it's a lexer plus a rule engine, aiming for zero false
  negatives on well-known footguns (`rm -rf /`, `dd of=/dev/sdX`, `curl|sh`)
  while keeping false positives low.
- **Never execute silently.** `ut-plugin-api::PluginContext` structurally
  prevents a plugin from running a command without going through the same
  `ut-safety` check and explicit user action as typed input — see
  `PluginAction` and `PluginContext::suggest_input`. `ut-plugin-ai`'s
  network-backed assistant is held to the same rule: it can only
  `ShowMessage`/`SuggestCommand`, never run something on its own.
- **Trust is never self-declared.** `ut-plugin-signing::Keyring::verify`
  checks a plugin's signature against publisher keys the *plugin manager*
  already trusts — never a key embedded in the package itself, which an
  attacker could just as easily generate.

## What's not here yet

A native windowed GUI (`ut-tui` is the terminal-hosted UI standing in for
it today), SSH's SFTP browser and port forwarding (the `SshClient`
foundation they'd extend already exists in `ut-plugin-ssh::client`),
workflow editors/exporters, actual dynamic plugin loading (signing/
verification exists in `ut-plugin-signing`; loading a `.so` behind that
check is still future work), and llama.cpp/remote-provider AI backends
(Ollama is implemented; the `AiProvider` trait is what a second backend
implements) are all future work described in
[`docs/SPEC.md`](docs/SPEC.md). The crate layout above is built so each of
those is an addition to one crate, not a restructuring.
