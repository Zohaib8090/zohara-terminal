//! `zohara`: a small CLI front end over `ut-safety`.
//!
//! This is a stand-in for the "beginner assistance" and "command safety"
//! UI described in the spec, exposed as a scriptable command so the safety
//! engine can be exercised, tested, and wired into shell hooks (e.g. a
//! zsh `preexec` that pipes the about-to-run command through `zohara check`)
//! ahead of a full GUI terminal being built on top of `zohara-term`.

use std::env;
use std::io::{self, Read};
use std::process::ExitCode;

use ut_safety::{analyze, analyze_paste, PrivilegeContext, Risk};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("check") => cmd_check(&args[1..]),
        Some("paste") => cmd_paste(&args[1..]),
        Some("--help") | Some("-h") | None => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("zohara: unknown subcommand `{other}`\n");
            print_help();
            ExitCode::from(2)
        }
    }
}

fn print_help() {
    println!(
        "zohara — command-safety checks for Zohara Terminal\n\
         \n\
         USAGE:\n\
         \x20   zohara check <command...>   Analyze a command line for destructive/risky behavior\n\
         \x20   zohara paste                Analyze piped-in text as a pasted block (reads stdin)\n\
         \n\
         Exit status: 0 if nothing above 'low' risk was found, 1 otherwise."
    );
}

fn read_command_arg(rest: &[String]) -> String {
    rest.join(" ")
}

fn cmd_check(rest: &[String]) -> ExitCode {
    if rest.is_empty() {
        eprintln!("zohara check: expected a command, e.g. `zohara check rm -rf /`");
        return ExitCode::from(2);
    }
    let line = read_command_arg(rest);
    let ctx = PrivilegeContext::local_normal_user();
    let report = analyze(&line, &ctx);

    if report.is_empty() {
        println!("No risky patterns detected in: {line}");
        return ExitCode::SUCCESS;
    }

    println!("Checked: {line}\n");
    for f in &report.findings {
        print_finding(f.risk, f.category, &f.explanation, f.suggestion.as_deref(), f.type_to_confirm);
    }

    if report.highest_risk().unwrap_or(Risk::Low) >= Risk::Medium {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn cmd_paste(_rest: &[String]) -> ExitCode {
    let mut input = String::new();
    if io::stdin().read_to_string(&mut input).is_err() {
        eprintln!("zohara paste: failed to read stdin");
        return ExitCode::from(2);
    }
    let ctx = PrivilegeContext::local_normal_user();
    let report = analyze_paste(&input, &ctx);

    if !report.is_suspicious() {
        println!("Paste looks clean ({} bytes).", input.len());
        return ExitCode::SUCCESS;
    }

    println!("Paste preview:\n{}\n", report.visible_preview);
    for f in &report.paste_findings {
        print_finding(f.risk, f.category, &f.explanation, None, false);
    }
    for f in &report.command_findings {
        print_finding(f.risk, f.category, &f.explanation, f.suggestion.as_deref(), f.type_to_confirm);
    }

    if report.highest_risk().unwrap_or(Risk::Low) >= Risk::Medium {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn print_finding(risk: Risk, category: &str, explanation: &str, suggestion: Option<&str>, type_to_confirm: bool) {
    let tag = match risk {
        Risk::Low => "LOW",
        Risk::Medium => "MEDIUM",
        Risk::High => "HIGH",
        Risk::Critical => "CRITICAL",
    };
    println!("[{tag}] {category}: {explanation}");
    if let Some(s) = suggestion {
        println!("  suggestion: {s}");
    }
    if type_to_confirm {
        println!("  (this would require type-to-confirm before running)");
    }
    println!();
}
