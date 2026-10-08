# GOTCHAS.md

Specific traps hit while building this repo, each of which cost real
debugging time. Read the relevant section before touching that area —
these aren't theoretical, they're bugs that actually shipped briefly and
got caught by tests or live verification.

## Threading + PTYs/sessions: never hold a lock through a blocking read

**The pattern that bit us twice:** a background thread does
`mutex.lock()` then calls something that blocks indefinitely (a PTY read,
a libssh2 channel read) *while still holding the lock*. Any other thread
that needs the same lock — e.g. to write input — is now stuck until the
blocking call happens to return, which might be never.

- **`ut-daemon`** (`crates/services/ut-daemon/src/lib.rs`): the session
  reader thread locked `Mutex<Pane>` and called a blocking
  `pump_output_captured`, so `attach`'s `write_input` could never get the
  lock. Fixed by putting the pane's fd in non-blocking mode
  (`Pane::set_nonblocking`) and having the reader thread poll with a short
  sleep between attempts — it still only holds the lock for the duration
  of one syscall, never while waiting for data.
- **`ut-gui`** (`crates/app/ut-gui/src/main.rs`): a pane created by a
  mid-session split started in blocking mode (only the *initial* panes
  were set non-blocking at construction), so the single-threaded egui
  `update()` loop froze solid the moment it tried to pump that pane. Fixed
  by re-asserting `set_nonblocking(true)` on every pane, every frame — the
  comment at that call site spells out why it has to be unconditional
  rather than "once, when a pane is created".

**The fix pattern, every time:** make the fd/session non-blocking, and
poll with a short sleep — never a bare blocking call — anywhere a lock
(or sole ownership needed elsewhere) is involved.

## `Pty::read` on Linux: EIO means EOF, not a real error

Once every fd referencing a PTY's slave side closes (the child exited),
`read()` on the **master** returns `EIO`, not `Ok(0)`. `ut_pty::Pty::read`
(`crates/core/ut-pty/src/lib.rs`) translates `EIO` into `Ok(0)` so every
caller gets the normal "0 bytes read = EOF" convention instead of having
to special-case this everywhere. If you ever reimplement PTY reading from
scratch, remember this — it's a genuine POSIX/Linux quirk, not a bug.

## `ut_session::Pane::pump_output_captured` returns `Option`, not bytes

Its signature is `-> io::Result<Option<&[u8]>>`, where `None` means
"non-blocking fd, no data right now" and `Some(&[])` means real EOF.
Collapsing these into one "empty means nothing happened" case (an earlier
version of this code did exactly that) makes a poller unable to tell "try
again in a moment" from "the shell exited" — which in `ut-daemon` meant
sessions were incorrectly marked dead the first time their reader thread
hit `WouldBlock`. Keep the distinction if you touch this.

## libssh2 (`ssh2` crate) threading and non-blocking error shapes

- **One `Session` = one thread, full stop.** libssh2 multiplexes every
  channel and SFTP request over one connection internally; concurrent
  access to the same `Session`'s channels from two threads is undefined
  behavior, not just a race to avoid. `ut-plugin-ssh::forward` sidesteps
  this entirely by giving each active port-forward its *own* dedicated
  session and running one cooperative single-threaded event loop per
  forward, rather than reaching for `Arc<Mutex<Session>>` (which would
  still be wrong — a mutex doesn't fix "two threads must never touch this
  even sequentially-interleaved without protocol-level coordination" the
  way it fixes plain data races).
- **The actual `WouldBlock`/EAGAIN shapes**, confirmed against a real
  session rather than assumed from docs:
  - `Channel::read`/`write` in non-blocking mode (`session.set_blocking(false)`)
    return a plain `io::Error` with `.kind() == io::ErrorKind::WouldBlock` —
    behaves like a normal non-blocking socket.
  - `Listener::accept()` (remote port forwarding) does **not** go through
    `io::Error` — it returns `ssh2::Error` with
    `.code() == ssh2::ErrorCode::Session(-37)` when nothing's pending.
  - If you add more non-blocking ssh2 code, verify the actual error shape
    against a real connection before writing retry logic around it — don't
    guess the error enum/code from the crate's doc comments alone.

## apt in this sandbox: `*-security` packages 404, pin to `noble/main` instead

Several packages (`openssh-server`, `bsdextrautils`, others) fail with a
404 when apt tries to pull the `*-security`/`*-updates` candidate version.
The fix every time: `apt-cache policy <pkg>` to see the plain `noble/main`
version still available, then pin it explicitly:

```sh
apt-get install -y --no-install-recommends <pkg>=<the-plain-noble-main-version>
```

This is an environment quirk (a missing mirror for security updates), not
a real dependency problem — don't spend time trying to fix apt config.

## Test tie-breaks: `Reverse((score, label))` sorts ties backwards from what you'd expect

In `ut-palette`, an early version sorted search results with
`sort_by_key(|(s, item)| Reverse((*s, item.label.clone())))`, intending
"highest score first, alphabetical for ties". `Reverse` on a tuple negates
*every* field, so equal-score ties actually came out in **descending**
alphabetical order (Z before A), not ascending — caught by a test that
assumed a specific tie order. Fixed by sorting score descending and label
ascending explicitly: `sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)))`.
If you want "sort A descending, B ascending" on a composite key, don't
reach for `Reverse((a, b))` — it reverses both.

## `cargo clippy --fix` and sibling tools can touch files outside what you asked

`cargo clippy --fix --allow-dirty --lib --tests -p <crate>` rewrote
`export.rs` while it was mid-edit in one session — not wrong, just a
reminder that these auto-fix commands touch files on disk directly. Re-run
`cargo build`/`test` immediately after any `--fix` invocation rather than
assuming the fix was a no-op; a few of clippy's suggestions here
(`needless_borrow`, `ptr_arg`) required a follow-up manual check that
nothing semantic changed.

## Shell-script generation: verify by actually running the shell

Don't trust a Bash/Zsh/Fish script generator by eyeballing the output
string. `ut-plugin-workflow::export`'s tests write the generated script to
a real file and run it through the real interpreter, asserting on exit
codes and a scratch-file counter — this is what caught that the test
*assertions* (not the generator) were sometimes wrong (e.g. the
`yaml_is_hand_editable_plain_text` test asserted `!yaml.contains("secs")`,
which trivially failed because the field is legitimately named
`timeout_secs` — fixed by checking for `"nanos"` instead, the actual
signal of a raw `Duration` struct leaking through serde).

## Dynamic plugin loading: don't export a `dyn Trait` pointer across a dylib boundary

The tempting design for `ut-plugin-loader` was `extern "C" fn create() ->
*mut dyn Plugin`: simple, and it even compiles and runs fine as long as
host and plugin share the exact rustc build and crate graph. The problem
is what happens the moment they don't — a `dyn Trait` value is a fat
pointer (data pointer + vtable pointer), and the vtable's layout/address
is an unstabilized implementation detail of one specific compiler build.
A mismatch doesn't error cleanly; it's UB (wrong function called through
a stale vtable, or a crash with no useful message). Used a real opaque
`*mut c_void` instance plus five plain `extern "C"` functions and a tiny
tagged-string wire protocol for `PluginAction` instead — more ABI
boilerplate, but every piece of it is actually a stable C ABI, checkable
at load time (`zohara_plugin_abi_version()`) rather than a silent trap.

Also: clippy's `not_unsafe_ptr_arg_deref` (deny-by-default) flags any
`pub extern "C" fn` that dereferences a raw pointer unless it's marked
`pub unsafe extern "C" fn` — doesn't affect the exported symbol (dlsym
doesn't care about Rust's `unsafe` keyword), just the Rust-side safety
contract. Mark every ABI function that touches a pointer `unsafe`, with a
`# Safety` doc comment on what the caller (the host) must guarantee.

## Testing dynamic loading: build the fixture plugin as a cdylib via a real `cargo build` subprocess

`ut-plugin-loader`'s "loads and drives a real compiled plugin" test
shells out to `cargo build -p ut-plugin-loader-fixture --lib` before
`dlopen`-ing the result — proving the ABI macro actually produces a
working `.so`, not just that it expands to plausible-looking code. This
only works because the fixture crate is `publish = false` and lives in
the workspace purely as a test dependency; if you add a second
dynamically-loaded plugin for tests, follow the same pattern rather than
hand-rolling raw bytes to fake a plugin binary.

## GUI verification needs a real render, not just "it compiled"

`eframe`'s `ViewportCommand::Screenshot` + a software-rendered `Xvfb`
display (Mesa/llvmpipe, no GPU needed) is what actually proved `ut-gui`
renders panes, splits, the safety confirmation banner, and the command
palette correctly — several of these were visually wrong on first
pass (e.g. a confirm flow that silently no-opped because a shared "done"
flag gated a second step it shouldn't have) in ways that compiled fine and
"looked plausible" in the code. If you add a new interactive GUI feature,
add a `--auto-*`/verification flag for it (see existing ones in
`ut-gui/src/main.rs`) and actually look at the resulting PNG before
calling it done.
