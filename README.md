# Zohara Terminal

A universal terminal application for Arch-based Linux, built to serve
beginners, everyday users, developers, and power users from one codebase.
The full product vision is in [`docs/SPEC.md`](docs/SPEC.md).

## Architecture

The workspace is layered, and dependencies only ever flow one direction:
**core → services → plugins → app**. Nothing in `core` knows about a
service; nothing in a service knows about a plugin. This is what keeps a
25-crate workspace navigable instead of a dependency tangle.

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
│   ├── ssh                 # HostProfile + config import + SshClient (connect/exec), SFTP, port forwarding
│   ├── package-manager        # pacman/checkupdates output parsing (update previews, tx size)
│   ├── workflow                   # command-sequence engine + JSON/YAML round-trip + Bash/Zsh/Fish/PowerShell export
│   └── ai                            # offline explainer + optional Ollama-backed NetworkAssistant
│
└── app/             # toolkit-facing
    ├── ut-input       # keyboard event → PTY byte sequence (xterm-style encoding)
    ├── ut-renderer      # Grid → ANSI text reproduction + damage/diff tracking
    ├── ut-ui              # pane split-tree layout (splits, resize, zoom) — pure data
    ├── ut-tui               # a real interactive multiplexer UI (ratatui) over ut-session + ut-ui
    ├── ut-gui                 # a real native OS window (eframe/egui) over ut-tui's own App
    ├── ut-palette                # fuzzy command palette (search/recent/favorites/aliases) — pure data
    ├── ut-cli                      # `zohara check`/`zohara paste` — safety-engine CLI
    └── ut-app                      # `run` — spawns a real command through the whole core stack
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
- **Port forwarding gets one thread per tunnel, not one per connection.**
  libssh2 requires every channel on one `Session` to be accessed from a
  single thread (concurrent access from different threads is undefined
  behavior) — so `ut-plugin-ssh::forward` gives each active `local_forward`/
  `remote_forward` a dedicated, non-blocking session and cooperatively
  multiplexes every tunnelled connection on that one thread, rather than
  reaching for a `Mutex<Session>` and fighting libssh2's threading model.
- **A workflow exporter is only trustworthy if the output actually runs.**
  `ut-plugin-workflow::export`'s Bash/Zsh/Fish generators aren't just
  string templates checked by eye — their tests write the generated
  script to disk and run it through the real `bash`/`zsh`/`fish` binary,
  asserting on exit codes and a scratch-file counter to prove retry, stop-
  on-failure, continue-on-failure, timeout, and `{{VAR}}` substitution all
  behave the way the doc comment claims. PowerShell's generator is
  structural-only (no `pwsh` in this environment) and documented as such.
- **`app/*` is the only layer allowed to assume a runtime/toolkit shape,
  and `ut-gui` is the payoff of that boundary.** `ut-tui`'s `App` (panes,
  split layout, the safety-gated input line) has zero ratatui-specific
  code in it — it's just state. `ut-gui` depends on `ut-tui` as a library
  and reuses that exact `App` unmodified, writing only a new renderer
  (`Grid` → `egui::text::LayoutJob`, in `ut-gui::render`) and a new input
  translator (`egui::Event` → `ut_input::Key`, in `main.rs`). Getting a
  second, real, native-windowed front end cost two files, not a rewrite.
  `ut-palette` is wired in the same way: `ut-gui` owns one `Palette`
  populated with real actions (split/close/zoom/quit) and a `PaletteState`
  driven by Ctrl+P, but the fuzzy matching, ranking, and recent/favorite/
  alias logic all live in `ut-palette` untouched by any rendering code —
  confirming a result calls the exact same `app.split_active`/
  `close_active`/`zoom_active` the Ctrl+B prefix already calls.

### See it work end-to-end

```
cargo run -p ut-app -- run -- /bin/sh -c 'echo hello from ut-app'
cargo run -p ut-app -- run -- rm -rf /        # refused: ut-safety flags it, exits 1
cargo run -p ut-app -- run --force -- ls -la  # --force bypasses the gate

cargo run -p ut-tui   # a real interactive multiplexer: splits, zoom, close, destructive-command
                       # confirmation prompts, all over real shells. Ctrl+B is the prefix key
                       # (", -, x, z, q) — see crates/app/ut-tui/src/main.rs for the full list.

cargo run -p ut-gui   # the same, in a real native OS window (eframe/egui). Same Ctrl+B prefix.
```

`ut-app run` spawns the given program behind a real `ut-pty`, feeds every
byte of its output through `ut-ansi`/`ut-grid`, and prints the resulting
screen once the program exits — proving the parser, the grid, and the PTY
layer are wired together correctly, not just unit-tested in isolation.
`ut-tui`/`ut-gui` prove the same for the full interactive stack, including
`ut-daemon`'s session model when driven through its socket protocol.

`ut-gui` was verified with real rendered screenshots, not just "it
compiles": a software-rendered `Xvfb` display + `eframe`'s own
`ViewportCommand::Screenshot` captured a running shell prompt, a live
horizontal split with two independent shells, and the destructive-command
confirmation banner correctly holding back `rm -rf /`. Re-run that
yourself with `ut-gui`'s verification flags:

```
xvfb-run -a cargo run -p ut-gui -- --screenshot /tmp/out.png \
    --exit-after-frames 60 --auto-type 'echo hello'

# The command palette (Ctrl+P), verified the same way: a query that filters
# and ranks results, and --palette-confirm proving the selected action is
# actually dispatched (here: split: 1 pane -> 2).
xvfb-run -a cargo run -p ut-gui -- --screenshot /tmp/palette.png \
    --exit-after-frames 20 --open-palette 'split h' --palette-confirm
```

## Building

```
cargo build --workspace
cargo test --workspace     # 308 tests across 25 crates, all offline/hermetic
cargo clippy --workspace --all-targets
```

Requires a recent stable Rust toolchain (edition 2021), a C toolchain +
`libssl-dev` (for `ut-plugin-ssh`'s `ssh2`/libssh2 binding), and is
Unix-only (`ut-pty` uses `nix`/`libc` directly; Arch Linux is the target
platform). `ut-gui` additionally needs a windowing system at *run* time
(X11/Wayland — a real desktop, or `Xvfb`/similar for a headless box) but
not at *build* time; `cargo build`/`test`/`clippy` for it need no display.
`ut-plugin-workflow`'s export tests run the scripts they generate through
real `bash`/`zsh`/`fish` binaries when available, for a genuine behavioral
check rather than just inspecting the generated text — but each one skips
cleanly (not a failure) if that particular shell isn't installed, so
`cargo test --workspace` still passes on a machine with only `bash`.

`ut-plugin-ssh`'s live-connection tests (`SshClient::connect`/`exec`, the
SFTP round trip, and both `local_forward`/`remote_forward`, against a real
server) are `#[ignore]`d by default so `cargo test --workspace` never
depends on network access or a running `sshd` — they were verified in
development against a real local test `sshd` (with `AllowTcpForwarding
yes` for the forwarding tests). Run them yourself against your own test
server with:

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

`ut-gui` gives the project a real native window, but it's still one plain
window — no multi-window/tray/dropdown/PiP modes, no themes-as-settings
(colors are hardcoded), no mouse-driven resize/drag for panes (only the
Ctrl+B keyboard prefix), no font/appearance picker. The command palette
(`ut-palette`) covers actions, but not yet "search tabs/SSH hosts/saved
workflows/files/settings/documentation/history" — those need the app
layer to actually feed items from those sources into the registry it
already accepts, and natural-language action search needs an AI backend
wired to it, neither built yet. SSH now has a real
`SshClient`, SFTP browser, and local/remote port forwarding — still
missing: dynamic (SOCKS) forwarding and ProxyJump chaining (see
`ut-plugin-ssh::forward`'s module docs for why dynamic forwarding
specifically was scoped out). Workflow editors/exporters, actual dynamic
plugin loading (signing/verification exists in `ut-plugin-signing`;
loading a `.so` behind that check is still future work), and
llama.cpp/remote-provider AI backends (Ollama is implemented; the
`AiProvider` trait is what a second backend implements) round out the
list — all described in [`docs/SPEC.md`](docs/SPEC.md). The crate layout
above is built so each of those is an addition to one crate, not a
restructuring.
