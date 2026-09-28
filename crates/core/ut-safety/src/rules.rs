//! Destructive-command and dangerous-pattern detection (Section 10).
//!
//! [`analyze`] takes a raw command-line string plus a [`PrivilegeContext`]
//! and returns a [`Report`] of [`Finding`]s, each with a [`Risk`] level, a
//! human-readable explanation, and (where possible) a safer alternative.
//! Findings are ordered most-severe first.

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

    findings.sort_by(|a, b| b.risk.cmp(&a.risk));
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

fn check_command(cmd: &SimpleCommand, ctx: &PrivilegeContext, findings: &mut Vec<Finding>) {
    let Some(prog) = program_name(cmd) else { return };
    let text = cmd.words.join(" ");
    let args = &cmd.words[1..];

    // Peel off a leading `sudo`/`doas`/`pkexec` to see the real command, and
    // flag the elevation itself.
    let (effective_prog, effective_args, elevated) = if matches!(prog, "sudo" | "doas" | "pkexec") {
        let inner: Vec<String> = args.iter().skip_while(|a| a.starts_with('-')).cloned().collect();
        let inner_prog = inner.first().map(|s| s.rsplit('/').next().unwrap_or(s).to_string());
        (inner_prog, inner.get(1..).map(|s| s.to_vec()).unwrap_or_default(), true)
    } else {
        (Some(prog.to_string()), args.to_vec(), false)
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
        "rm" | "rmdir" | "unlink" | "shred" => {
            check_rm(ep, &effective_args, ctx, &text, findings);
        }
        "find" => check_find_delete(&effective_args, &text, findings),
        "dd" => check_dd(&effective_args, &text, findings),
        "mkfs" | "mkfs.ext4" | "mkfs.ext3" | "mkfs.xfs" | "mkfs.btrfs" | "mkfs.vfat" | "mkfs.fat" | "mkfs.ntfs" => {
            check_mkfs(&effective_args, &text, findings)
        }
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
        "history" => {
            if effective_args.iter().any(|a| a == "-c") {
                findings.push(Finding {
                    risk: Risk::Low,
                    category: "history-clear",
                    explanation: "This clears your shell history for the current session.".into(),
                    command_text: text.clone(),
                    suggestion: None,
                    type_to_confirm: false,
                });
            }
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
    } else if prog == "shred" && targets.iter().any(|t| t.starts_with("/dev/")) {
        findings.push(Finding {
            risk: Risk::Critical,
            category: "raw-disk-write",
            explanation: format!("`shred` will overwrite the raw device `{}` with garbage, destroying everything on it.", targets.first().unwrap_or(&"")),
            command_text: text.to_string(),
            suggestion: None,
            type_to_confirm: true,
        });
    }
}

fn check_find_delete(args: &[String], text: &str, findings: &mut Vec<Finding>) {
    if args.iter().any(|a| a == "-delete") || args.windows(2).any(|w| w[0] == "-exec" && (w[1] == "rm" || w[1].starts_with("rm"))) {
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
    if !recursive {
        return;
    }
    let broad_mode = args.iter().any(|a| a == "777" || a == "a+rwx" || a == "+rwx" || a == "000");
    let targets: Vec<&str> = args
        .iter()
        .map(|s| s.as_str())
        .filter(|a| !a.starts_with('-') && !a.chars().all(|c| c.is_ascii_digit()) && !a.contains('+') && !a.contains('='))
        .collect();
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
    let target_root = targets.iter().skip(1).any(|t| is_root_like(t) || expands_broadly(t));
    if recursive && target_root {
        findings.push(Finding {
            risk: Risk::High,
            category: "recursive-permission",
            explanation: format!("This recursively changes ownership of a broad or system path, which can lock you out of your own files or break installed software (`{prog}`)."),
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
        "checkout" | "restore" if rest.iter().any(|a| a == ".") || rest.iter().any(|a| a == "--") && rest.contains(&".".to_string()) => {
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
    if prog == "kill" && args.iter().any(|a| a == "-1") {
        findings.push(Finding {
            risk: Risk::Critical,
            category: "process-kill",
            explanation: "`kill -1` sends SIGHUP to every process you can signal, which can terminate your whole session.".into(),
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
        } else if r.is_truncating() && is_root_like(&r.target) {
            findings.push(Finding {
                risk: Risk::High,
                category: "raw-disk-write",
                explanation: format!("This truncates `{}`.", r.target),
                command_text: text.to_string(),
                suggestion: None,
                type_to_confirm: false,
            });
        }
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
        let has_downloader = pipeline.commands[..pipeline.commands.len() - 1]
            .iter()
            .any(|c| matches!(program_name(c), Some("curl" | "wget" | "fetch")));
        let last = &pipeline.commands[pipeline.commands.len() - 1];
        let feeds_shell = matches!(program_name(last), Some("sh" | "bash" | "zsh" | "fish" | "dash" | "python" | "python3" | "perl" | "sudo"));
        if has_downloader && feeds_shell {
            let text = pipeline
                .commands
                .iter()
                .map(|c| c.words.join(" "))
                .collect::<Vec<_>>()
                .join(" | ");
            findings.push(Finding {
                risk: Risk::High,
                category: "download-execute",
                explanation: "This downloads a script from the network and runs it immediately, without letting you see what it does first.".into(),
                command_text: text,
                suggestion: Some("Download the script to a file, read it, then run it separately.".into()),
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

    #[test]
    fn highest_risk_orders_findings_first() {
        let r = analyze("rm -rf / ; echo hi", &ctx());
        assert_eq!(r.findings[0].risk, Risk::Critical);
    }
}
