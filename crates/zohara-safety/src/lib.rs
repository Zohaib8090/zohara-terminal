//! `zohara-safety`: command-safety and safe-paste analysis for Zohara Terminal.
//!
//! This crate implements the "Command safety" and "Safe paste" sections of
//! the Zohara Terminal spec: it looks at a shell command line (or a pasted
//! block of them) *before* it runs and reports what is risky about it, so a
//! front end can show a human-readable warning and ask for confirmation.
//!
//! It is intentionally a static, best-effort analyzer: it never executes
//! anything, and it is conservative rather than exhaustive — it aims for
//! zero false negatives on the well-known dangerous patterns (`rm -rf /`,
//! `dd` to a raw disk, `mkfs`, `curl | sh`, etc.) while keeping false
//! positives low enough to stay usable in Everyday and Power-user modes.

mod lexer;
mod paste;
mod privilege;
mod rules;

pub use lexer::{lex, Pipeline, Redirect, Script, SimpleCommand};
pub use paste::{analyze_paste, PasteFinding, PasteReport};
pub use privilege::{Privilege, PrivilegeContext};
pub use rules::{analyze, Finding, Report, Risk};
