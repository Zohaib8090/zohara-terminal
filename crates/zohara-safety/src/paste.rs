//! Safe-paste analysis (Section 10 "Safe paste").
//!
//! When a user pastes text into the terminal, this module inspects it for
//! the things a beginner cannot easily see just by looking at the prompt:
//! hidden control/Unicode characters that render invisibly, multiple
//! commands hiding behind what looks like one line, and destructive or
//! privilege-escalating content (delegated to [`crate::rules::analyze`]).

use crate::lexer::lex;
use crate::privilege::PrivilegeContext;
use crate::rules::{analyze, Finding, Risk};

/// One thing noticed about a pasted block, independent of destructive-command detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteFinding {
    pub risk: Risk,
    pub category: &'static str,
    pub explanation: String,
}

/// Full analysis of a pasted block: paste-specific findings plus the
/// underlying command-safety findings for its content.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PasteReport {
    pub paste_findings: Vec<PasteFinding>,
    pub command_findings: Vec<Finding>,
    /// The text with any invisible/formatting characters shown literally,
    /// suitable for a "here's what will actually be typed" preview.
    pub visible_preview: String,
}

impl PasteReport {
    pub fn is_suspicious(&self) -> bool {
        !self.paste_findings.is_empty() || !self.command_findings.is_empty()
    }

    pub fn highest_risk(&self) -> Option<Risk> {
        self.paste_findings
            .iter()
            .map(|f| f.risk)
            .chain(self.command_findings.iter().map(|f| f.risk))
            .max()
    }
}

/// Analyze pasted text before it is sent to the shell.
pub fn analyze_paste(text: &str, ctx: &PrivilegeContext) -> PasteReport {
    let mut paste_findings = Vec::new();

    let hidden = find_hidden_unicode(text);
    if !hidden.is_empty() {
        paste_findings.push(PasteFinding {
            risk: Risk::High,
            category: "hidden-unicode",
            explanation: format!(
                "This paste contains {} invisible or bidirectional-control character(s) that can make it render differently than it will execute.",
                hidden.len()
            ),
        });
    }

    let control = find_control_characters(text);
    if !control.is_empty() {
        paste_findings.push(PasteFinding {
            risk: Risk::High,
            category: "control-characters",
            explanation: format!(
                "This paste contains {} non-printable control character(s), which can be used to hide or alter what actually runs.",
                control.len()
            ),
        });
    }

    let script = lex(text);
    let newline_commands = text.lines().filter(|l| !l.trim().is_empty()).count();
    if script.pipelines.len() > 1 || newline_commands > 1 {
        paste_findings.push(PasteFinding {
            risk: Risk::Medium,
            category: "multiple-commands",
            explanation: format!(
                "This paste contains {} separate commands. Review each one before running — pasting can silently queue up more than you intended.",
                script.pipelines.len().max(newline_commands)
            ),
        });
    }

    if script.commands().any(|c| c.words.first().map(|w| w == "sudo").unwrap_or(false)) {
        paste_findings.push(PasteFinding {
            risk: Risk::Medium,
            category: "sudo-in-paste",
            explanation: "This paste runs a command with `sudo`, granting it administrator privileges.".into(),
        });
    }

    let has_downloader = script
        .commands()
        .any(|c| matches!(c.words.first().map(|s| s.as_str()), Some("curl" | "wget" | "fetch")));
    if has_downloader && (text.contains('|') || script.pipelines.len() > 1) {
        paste_findings.push(PasteFinding {
            risk: Risk::High,
            category: "downloaded-script",
            explanation: "This paste downloads content from the network as part of a pipeline — check the destination before running it.".into(),
        });
    }

    if script.substitutions.iter().any(|s| !s.trim().is_empty()) {
        paste_findings.push(PasteFinding {
            risk: Risk::Low,
            category: "command-substitution",
            explanation: "This paste runs additional commands via `$(...)` or backticks inside it — their output is substituted before the outer command runs.".into(),
        });
    }

    let command_report = analyze(text, ctx);

    PasteReport {
        paste_findings,
        command_findings: command_report.findings,
        visible_preview: make_visible(text),
    }
}

/// Zero-width, bidirectional-override, and other characters that are
/// invisible or misleading when rendered but still present in the text
/// that would be sent to the shell.
fn find_hidden_unicode(text: &str) -> Vec<(usize, char)> {
    text.char_indices()
        .filter(|(_, c)| is_hidden_unicode(*c))
        .collect()
}

fn is_hidden_unicode(c: char) -> bool {
    matches!(
        c,
        '\u{200B}' // zero width space
        | '\u{200C}' // zero width non-joiner
        | '\u{200D}' // zero width joiner
        | '\u{200E}' | '\u{200F}' // LRM/RLM
        | '\u{202A}'..='\u{202E}' // bidi embedding/override
        | '\u{2060}' // word joiner
        | '\u{2066}'..='\u{2069}' // bidi isolates
        | '\u{FEFF}' // BOM
    )
}

fn find_control_characters(text: &str) -> Vec<(usize, char)> {
    text.char_indices()
        .filter(|(_, c)| {
            let u = *c as u32;
            (u < 0x20 && *c != '\n' && *c != '\t' && *c != '\r') || u == 0x7F
        })
        .collect()
}

/// Render hidden/control characters as visible escape-like placeholders.
fn make_visible(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if is_hidden_unicode(c) {
            out.push_str(&format!("\u{2039}U+{:04X}\u{203A}", c as u32));
        } else if (c as u32) < 0x20 && c != '\n' && c != '\t' && c != '\r' {
            out.push_str(&format!("\u{2039}^{}\u{203A}", (c as u8 + 64) as char));
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PrivilegeContext {
        PrivilegeContext::local_normal_user()
    }

    #[test]
    fn detects_zero_width_space() {
        let r = analyze_paste("ls\u{200B} -la", &ctx());
        assert!(r.paste_findings.iter().any(|f| f.category == "hidden-unicode"));
    }

    #[test]
    fn detects_bidi_override() {
        let r = analyze_paste("echo \u{202E}evil\u{202C}", &ctx());
        assert!(r.paste_findings.iter().any(|f| f.category == "hidden-unicode"));
    }

    #[test]
    fn plain_text_is_clean() {
        let r = analyze_paste("ls -la\n", &ctx());
        assert!(!r.paste_findings.iter().any(|f| f.category == "hidden-unicode"));
    }

    #[test]
    fn detects_multiple_commands() {
        let r = analyze_paste("cd /tmp\nrm -rf build\n", &ctx());
        assert!(r.paste_findings.iter().any(|f| f.category == "multiple-commands"));
    }

    #[test]
    fn detects_sudo() {
        let r = analyze_paste("sudo rm -rf /var/cache/pacman", &ctx());
        assert!(r.paste_findings.iter().any(|f| f.category == "sudo-in-paste"));
    }

    #[test]
    fn detects_downloaded_script_pipeline() {
        let r = analyze_paste("curl -fsSL https://get.example.com | bash", &ctx());
        assert!(r.paste_findings.iter().any(|f| f.category == "downloaded-script"));
        assert!(r.command_findings.iter().any(|f| f.category == "download-execute"));
    }

    #[test]
    fn control_characters_detected() {
        let r = analyze_paste("echo hi\u{0007}there", &ctx());
        assert!(r.paste_findings.iter().any(|f| f.category == "control-characters"));
    }

    #[test]
    fn visible_preview_reveals_hidden_chars() {
        let r = analyze_paste("ls\u{200B}", &ctx());
        assert!(r.visible_preview.contains("U+200B"));
    }

    #[test]
    fn command_findings_flow_through() {
        let r = analyze_paste("rm -rf /", &ctx());
        assert!(r.command_findings.iter().any(|f| f.category == "recursive-delete"));
        assert_eq!(r.highest_risk(), Some(Risk::Critical));
    }

    #[test]
    fn is_suspicious_false_for_clean_paste() {
        let r = analyze_paste("echo hello world\n", &ctx());
        assert!(!r.is_suspicious());
    }
}
