# HANDOFF.md

Session handoff notes — written so a fresh session (human or AI) can pick
this up without re-deriving context. If you're compacting/starting a new
conversation, read this file first, then `README.md`, then `ROADMAP.md`,
then `GOTCHAS.md` before writing any code.

## Where things stand right now

- **Branch**: `claude/upbeat-sagan-bfhhua` (this is the active development
  branch — push here unless told otherwise).
- **Latest commit**: `5c1d246` — "Add ut-palette: a real fuzzy command
  palette, wired into ut-gui (Ctrl+P)".
- **Tree is clean**: no uncommitted changes as of this handoff.
- **25 crates, 308 tests, all passing, `cargo clippy --workspace
  --all-targets` clean.** This has been true after every commit in this
  branch's history — keep it true.
- **No PR has been opened.** All work has been pushed directly to the
  branch; the user hasn't asked for a PR yet.

## Git identity note (important, don't lose this)

Partway through this work, the user asked to commit under their own name
instead of Claude's attribution. Current config in this checkout:

```
user.name = Zohaib
user.email = zohaibbaig144@gmail.com
```

This is set via `git config` in the working directory (not global), so it
persists for this checkout but **won't automatically carry over to a
fresh clone/container**. If you're starting a new session against this
repo, check `git config user.name`/`user.email` and, if unset, ask the
user whether they still want commits attributed to them rather than
defaulting to Claude attribution lines.

## How this session worked (so you can match the pattern)

The user has been saying some variant of "keep going, pick the next one"
repeatedly. Each round followed the same shape:

1. Pick the highest-leverage remaining item from `ROADMAP.md` (or propose
   one if the roadmap needs re-prioritizing).
2. Build it for real — no stubs, no "TODO: implement later". If a feature
   needs live infrastructure to prove it works (a shell, a server, a
   display), **set that infrastructure up in this sandbox** (apt-get
   install, spin up a local sshd, install Xvfb) rather than settling for
   "trust me, the code looks right." See `GOTCHAS.md` for the exact
   recipes and the traps hit doing this.
3. Write tests that exercise the real thing, run them, fix what's broken
   (several rounds caught genuine bugs this way — a deadlock, a sort
   tie-break, a verification flag that silently no-opped).
4. Run `cargo test --workspace` and `cargo clippy --workspace --all-targets`
   clean before committing.
5. Update `README.md` (crate tree, a "why" bullet if the feature taught a
   non-obvious lesson, the test count, "what's not here yet").
6. Commit with a detailed message (what was built, how it was verified,
   test counts) under the user's identity, push, confirm local/remote
   `git rev-parse HEAD` match.
7. Report back concisely: what got built, how it was verified (screenshots
   sent via `SendUserFile` for GUI work), what's still missing, and ask
   if they want to keep going.

If the user says "keep going" again, repeat this loop. Don't ask which
item to pick unless the obvious next choice is genuinely ambiguous — the
user has consistently delegated that choice.

## What just happened, in build order (newest first)

1. **`ut-palette`** — fuzzy command palette (fzf-style scoring), wired
   into `ut-gui` as Ctrl+P. Verified with screenshots showing the overlay
   filtering/ranking results and a `--palette-confirm` flag proving a
   selected action actually dispatches (pane count 1→2).
2. **Workflow export** — `ut-plugin-workflow::export` renders a workflow
   as a Bash/Zsh/Fish/PowerShell script; `text_format` round-trips
   JSON/YAML. Bash/Zsh/Fish generators are verified by actually running
   the generated script through the real interpreter (installed `zsh`
   and `fish` in this sandbox specifically for this).
3. **SFTP + port forwarding** — `ut-plugin-ssh::sftp`/`forward`. Verified
   against a real local `sshd` (upload/download/mkdir/rename/rmdir, and
   both `local_forward`/`remote_forward` tunneling real TCP traffic).
4. **`ut-gui`** — the native windowed GUI (eframe/egui), reusing
   `ut-tui::App` unmodified. Verified with real screenshots under Xvfb
   (Mesa/llvmpipe software rendering, no GPU needed).
5. **Five parallel builds in one round**: `ut-plugin-signing` (ed25519
   manifest signing), `ut-daemon` (the real multiplexer — named sessions
   surviving detach/reattach over a Unix socket), `ut-tui` (the first real
   interactive front end), `ut-plugin-ai`'s `NetworkAssistant`
   (Ollama-backed, network-permission-gated), `ut-plugin-ssh::client`
   (the base SSH client, pre-SFTP/forwarding).
6. Before all of that: the initial architecture restructure into
   `core/services/plugins/app`, and before *that*, the very first
   bootstrap (`ut-ansi`/`ut-grid`/`ut-safety`/`ut-pty` and friends).

Each of these has a detailed commit message with exactly what was built
and how it was verified — `git log` is a legitimate second source of
truth alongside this file and `ROADMAP.md`.

## If you're about to continue this work

- Read `ROADMAP.md`'s "remaining" section and pick the next item, or ask
  the user if they have a preference.
- Follow the pattern above — especially step 2 (real infra, not stubs)
  and step 3 (real tests against the real thing).
- Check `GOTCHAS.md` before writing threading code, PTY code, or anything
  touching `ssh2`/libssh2 — several of those traps are easy to
  reintroduce if you're not aware of them going in.
- Don't re-litigate the architecture (`core → services → plugins → app`,
  one crate per concern) — it's been load-bearing through 25 crates and
  several large features without needing to change.
