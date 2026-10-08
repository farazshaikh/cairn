//! Splits interactive input into complete statements.
//!
//! A statement is complete at a `;` outside string literals, quoted
//! identifiers and comments. The scanner mirrors the trivia and quoting
//! rules of `cairn_sql::tokenize`: `'...'` and `"..."` with a doubled quote
//! standing for one, `--` to the end of the line, and `/* ... */` without
//! nesting. Every delimiter is ASCII, so scanning bytes never splits a
//! UTF-8 character.

/// Where the scanner is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    Str,
    Quoted,
    LineComment,
    BlockComment,
}

/// What one scanner step consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Whitespace or comment text.
    Trivia,
    /// Part of a statement.
    Content,
    /// A statement-ending `;`.
    Terminator,
}

/// Classifies the byte at `at` (with one byte of lookahead), updates the
/// state and returns how many bytes it consumed.
fn step(state: &mut State, bytes: &[u8], at: usize) -> (Class, usize) {
    let current = bytes.get(at).copied().unwrap_or(b'\n');
    let next = bytes.get(at + 1).copied();
    match *state {
        State::Normal => match (current, next) {
            (b' ' | b'\t' | b'\n' | b'\r', _) => (Class::Trivia, 1),
            (b'-', Some(b'-')) => {
                *state = State::LineComment;
                (Class::Trivia, 2)
            }
            (b'/', Some(b'*')) => {
                *state = State::BlockComment;
                (Class::Trivia, 2)
            }
            (b'\'', _) => {
                *state = State::Str;
                (Class::Content, 1)
            }
            (b'"', _) => {
                *state = State::Quoted;
                (Class::Content, 1)
            }
            (b';', _) => (Class::Terminator, 1),
            _ => (Class::Content, 1),
        },
        State::Str => quoted(state, b'\'', current, next),
        State::Quoted => quoted(state, b'"', current, next),
        State::LineComment => {
            if current == b'\n' {
                *state = State::Normal;
            }
            (Class::Trivia, 1)
        }
        State::BlockComment => {
            if (current, next) == (b'*', Some(b'/')) {
                *state = State::Normal;
                return (Class::Trivia, 2);
            }
            (Class::Trivia, 1)
        }
    }
}

fn quoted(state: &mut State, quote: u8, current: u8, next: Option<u8>) -> (Class, usize) {
    if current != quote {
        return (Class::Content, 1);
    }
    if next == Some(quote) {
        return (Class::Content, 2);
    }
    *state = State::Normal;
    (Class::Content, 1)
}

/// Buffers input lines and hands out each complete statement once.
#[derive(Debug)]
pub(crate) struct Splitter {
    /// Unconsumed text: the pending statement, if any, and nothing before it.
    buf: String,
    /// Scan position in `buf`.
    pos: usize,
    state: State,
    /// Offset in `buf` of the pending statement's first non-trivia byte.
    start: Option<usize>,
}

impl Splitter {
    pub(crate) fn new() -> Splitter {
        Splitter {
            buf: String::new(),
            pos: 0,
            state: State::Normal,
            start: None,
        }
    }

    /// Appends one input line (a missing final `\n` is added) and returns
    /// every statement it completes, in order. A statement starts at its
    /// first non-trivia character and ends with its `;`; statements with
    /// nothing before the `;` are dropped.
    pub(crate) fn push_line(&mut self, line: &str) -> Vec<String> {
        self.buf.push_str(line);
        if !line.ends_with('\n') {
            self.buf.push('\n');
        }
        let mut statements = Vec::new();
        while self.pos < self.buf.len() {
            let (class, len) = step(&mut self.state, self.buf.as_bytes(), self.pos);
            match class {
                Class::Trivia => {}
                Class::Content => {
                    self.start.get_or_insert(self.pos);
                }
                Class::Terminator => {
                    if let Some(start) = self.start.take() {
                        let end = self.pos + 1;
                        statements.push(self.buf.get(start..end).unwrap_or("").to_string());
                    }
                }
            }
            self.pos += len;
        }
        self.compact();
        statements
    }

    /// Drops consumed text so memory is bounded by the pending statement.
    fn compact(&mut self) {
        let keep_from = self.start.unwrap_or(self.pos);
        self.buf.drain(..keep_from);
        self.pos -= keep_from;
        if self.start.is_some() {
            self.start = Some(0);
        }
    }

    /// True while a statement has started but not ended, or a block comment
    /// is open.
    pub(crate) fn is_pending(&self) -> bool {
        self.start.is_some() || self.state == State::BlockComment
    }

    /// At end of input: the incomplete text, if any, leaving the splitter
    /// empty.
    pub(crate) fn take_pending(&mut self) -> Option<String> {
        let pending = self.is_pending().then(|| {
            let start = self.start.unwrap_or(0);
            self.buf.get(start..).unwrap_or("").to_string()
        });
        *self = Splitter::new();
        pending
    }
}

/// Byte offsets of every `;` the scanner treats as a terminator, including
/// those of empty statements. Used to check the scanner against the
/// tokenizer.
#[cfg(test)]
pub(crate) fn terminator_offsets(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut state = State::Normal;
    let mut offsets = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let (class, len) = step(&mut state, bytes, at);
        if class == Class::Terminator {
            offsets.push(at);
        }
        at += len;
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_sql::{TokenKind, tokenize};

    /// Feeds `input` line by line and returns every completed statement.
    fn split(input: &str) -> (Vec<String>, Splitter) {
        let mut splitter = Splitter::new();
        let mut statements = Vec::new();
        for line in input.split_inclusive('\n') {
            statements.extend(splitter.push_line(line));
        }
        (statements, splitter)
    }

    #[test]
    fn a_simple_statement_completes() {
        let (statements, splitter) = split("SELECT 1;\n");
        assert_eq!(statements, ["SELECT 1;"]);
        assert!(!splitter.is_pending());
    }

    #[test]
    fn semicolons_in_strings_identifiers_and_comments_do_not_terminate() {
        for (input, expected) in [
            ("SELECT 'a;b';", "SELECT 'a;b';"),
            ("SELECT \"a;b\" FROM t;", "SELECT \"a;b\" FROM t;"),
            (
                "SELECT 1 -- not ; here\n+ 2;",
                "SELECT 1 -- not ; here\n+ 2;",
            ),
            ("SELECT /* ; */ 1;", "SELECT /* ; */ 1;"),
        ] {
            let (statements, splitter) = split(input);
            assert_eq!(statements, [expected], "input {input:?}");
            assert!(!splitter.is_pending(), "input {input:?}");
        }
    }

    #[test]
    fn doubled_quotes_stay_inside() {
        let (statements, _) = split("SELECT 'it''s;';\nSELECT \"a\"\"b;\" FROM t;\n");
        assert_eq!(
            statements,
            ["SELECT 'it''s;';", "SELECT \"a\"\"b;\" FROM t;"]
        );
    }

    #[test]
    fn several_statements_on_one_line_come_in_order() {
        let (statements, splitter) = split("SELECT 1; SELECT 2;\n");
        assert_eq!(statements, ["SELECT 1;", "SELECT 2;"]);
        assert!(!splitter.is_pending());
    }

    #[test]
    fn a_statement_spanning_lines_waits_for_its_semicolon() {
        let mut splitter = Splitter::new();
        assert!(splitter.push_line("SELECT a\n").is_empty());
        assert!(splitter.is_pending());
        assert!(splitter.push_line("  FROM t\n").is_empty());
        assert!(splitter.is_pending());
        assert_eq!(
            splitter.push_line("  WHERE a = 1; SELECT"),
            ["SELECT a\n  FROM t\n  WHERE a = 1;"]
        );
        assert!(splitter.is_pending(), "the trailing SELECT is pending");
    }

    #[test]
    fn unterminated_strings_identifiers_and_comments_are_pending() {
        for input in [
            "SELECT 'abc;\n",
            "SELECT \"abc;\n",
            "/* open;\n",
            "SELECT 1 /* open;\n",
        ] {
            let (statements, mut splitter) = split(input);
            assert!(statements.is_empty(), "input {input:?}");
            assert!(splitter.is_pending(), "input {input:?}");
            assert!(splitter.take_pending().is_some(), "input {input:?}");
            assert!(!splitter.is_pending());
        }
        let (_, mut splitter) = split("SELECT 'abc;\nmore';\n");
        assert!(!splitter.is_pending());
        assert_eq!(splitter.take_pending(), None);
    }

    #[test]
    fn whitespace_comments_and_empty_statements_produce_nothing() {
        for input in ["\n", "   \t\n", "-- note\n", "/* c */\n", ";\n", "  ; ;\n"] {
            let (statements, splitter) = split(input);
            assert!(statements.is_empty(), "input {input:?}");
            assert!(!splitter.is_pending(), "input {input:?}");
        }
    }

    #[test]
    fn directives_pass_through_unchanged() {
        let (statements, _) = split("EXPLAIN SELECT 1; PRAGMA checkpoint;\n");
        assert_eq!(statements, ["EXPLAIN SELECT 1;", "PRAGMA checkpoint;"]);
    }

    #[test]
    fn leading_comments_are_dropped_and_utf8_survives() {
        let (statements, _) = split("/* c */ -- d\n  SELECT 'héllo; ✓';\n");
        assert_eq!(statements, ["SELECT 'héllo; ✓';"]);
    }

    /// Hypothesis H1 of the M5 design: on every input the tokenizer
    /// accepts, the scanner's terminators are exactly its `;` tokens.
    #[test]
    fn terminators_match_the_tokenizer() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/sql");
        let mut inputs: Vec<String> = std::fs::read_dir(dir)
            .expect("read tests/sql")
            .map(|entry| entry.expect("entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
            .map(|path| std::fs::read_to_string(path).expect("read script"))
            .collect();
        assert!(inputs.len() >= 40, "golden scripts found: {}", inputs.len());
        inputs.extend(
            [
                "SELECT 'a;b'; SELECT \"x;y\" FROM t;",
                "SELECT 'it''s;'; SELECT \"a\"\"b;\" FROM t;",
                "SELECT 1 -- ; \n; /* ; */ SELECT 2;",
                "  ; ;",
                "SELECT 1-/*x*/2; SELECT 5--3\n;",
            ]
            .map(String::from),
        );
        let mut checked = 0;
        for input in &inputs {
            let Ok(tokens) = tokenize(input) else {
                continue;
            };
            let expected: Vec<usize> = tokens
                .iter()
                .filter(|token| token.kind == TokenKind::Semicolon)
                .map(|token| token.span.start)
                .collect();
            assert_eq!(terminator_offsets(input), expected, "input {input:?}");
            checked += 1;
        }
        assert!(checked >= 40, "inputs checked: {checked}");
    }
}
