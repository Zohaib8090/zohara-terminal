//! `ut-app`: an end-to-end demo binary proving the layered architecture
//! actually works together — `ut-pty` spawns a real process behind a PTY,
//! its output is parsed and rendered into `ut-grid`'s cell model, and the
//! command line is checked by `ut-safety` before anything runs. This is
//! not the eventual GUI terminal (that needs `ut-ui`/`ut-input` plus a
//! real windowing toolkit, still future work) — it's the smallest possible
//! program that runs the whole `core` stack for real.
//!
//! Usage:
//!   ut-app run [--force] [--ansi] -- <program> [args...]
//!
//! `--force` skips the ut-safety confirmation gate (for scripting/CI).
//! `--ansi` reproduces the final screen with color/style escapes via
//! ut-renderer instead of plain text.

use std::io;
use std::process::ExitCode;
use ut_grid::Terminal;
use ut_pty::{Pty, PtySize};
use ut_safety::{PrivilegeContext, Risk};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("run") => run(&args[1..]),
        _ => {
            print_help();
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    eprintln!(
        "ut-app run [--force] [--ansi] -- <program> [args...]\n\
         \n\
         Spawns <program> behind a real PTY, feeds its output through the\n\
         ut-grid terminal screen model, and prints the resulting screen once\n\
         the program exits. The command is checked with ut-safety first,\n\
         unless --force is given."
    );
}

fn run(args: &[String]) -> ExitCode {
    let mut force = false;
    let mut ansi = false;
    let mut rest = args;
    loop {
        match rest.first().map(|s| s.as_str()) {
            Some("--force") => {
                force = true;
                rest = &rest[1..];
            }
            Some("--ansi") => {
                ansi = true;
                rest = &rest[1..];
            }
            Some("--") => {
                rest = &rest[1..];
                break;
            }
            _ => break,
        }
    }

    let Some((program, prog_args)) = rest.split_first() else {
        eprintln!("ut-app run: missing program to run (expected `ut-app run [--force] [--ansi] -- <program> [args...]`)");
        return ExitCode::from(2);
    };

    let privilege = PrivilegeContext::local_normal_user();
    let joined = std::iter::once(program.as_str()).chain(prog_args.iter().map(|s| s.as_str())).collect::<Vec<_>>().join(" ");
    let report = ut_safety::analyze(&joined, &privilege);

    if !force {
        if let Some(risk) = report.highest_risk() {
            if risk >= Risk::Medium {
                eprintln!("ut-app: refusing to run without --force — ut-safety flagged this command:\n");
                for f in &report.findings {
                    eprintln!("[{}] {}: {}", f.risk, f.category, f.explanation);
                    if let Some(s) = &f.suggestion {
                        eprintln!("  suggestion: {s}");
                    }
                }
                return ExitCode::FAILURE;
            }
        }
    }

    let args_ref: Vec<&str> = prog_args.iter().map(|s| s.as_str()).collect();
    let size = PtySize::new(100, 30);
    let mut pty = match Pty::spawn(program, &args_ref, size, None, &[]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("ut-app: failed to spawn '{program}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut terminal = Terminal::new(size.cols as usize, size.rows as usize);
    let mut buf = [0u8; 8192];
    loop {
        match pty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => terminal.feed(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("ut-app: read error: {e}");
                break;
            }
        }
    }

    let status = match pty.wait() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ut-app: wait failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    if ansi {
        println!("{}", ut_renderer::render_screen_ansi(terminal.grid()));
    } else {
        let mut lines = terminal.plain_text();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        for line in lines {
            println!("{line}");
        }
    }

    match status {
        ut_pty::PtyExitStatus::Exited(0) => ExitCode::SUCCESS,
        ut_pty::PtyExitStatus::Exited(code) => {
            eprintln!("ut-app: '{program}' exited with status {code}");
            ExitCode::FAILURE
        }
        ut_pty::PtyExitStatus::Signaled(sig) => {
            eprintln!("ut-app: '{program}' was killed by signal {sig}");
            ExitCode::FAILURE
        }
        ut_pty::PtyExitStatus::StillRunning => ExitCode::SUCCESS,
    }
}
