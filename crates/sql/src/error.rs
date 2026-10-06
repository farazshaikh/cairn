//! The error type shared by the tokenizer and the parser.

use std::fmt;

use crate::span::Span;

/// A tokenizer or parser error at a position in the source.
///
/// `Display` prints three lines: `line L, column C: message`, the source
/// line containing the error, and a caret under the column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlError {
    pub message: String,
    pub span: Span,
    /// The source line containing `span.start`, without its line break.
    pub source_line: String,
}

impl SqlError {
    pub(crate) fn new(message: impl Into<String>, span: Span, src: &str) -> SqlError {
        SqlError {
            message: message.into(),
            span,
            source_line: line_at(src, span.start).to_string(),
        }
    }
}

fn line_at(src: &str, offset: usize) -> &str {
    let before = src.get(..offset).unwrap_or(src);
    let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
    let rest = src.get(line_start..).unwrap_or("");
    let line = rest.split('\n').next().unwrap_or("");
    line.strip_suffix('\r').unwrap_or(line)
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "line {}, column {}: {}",
            self.span.line, self.span.column, self.message
        )?;
        writeln!(f, "{}", self.source_line)?;
        write!(f, "{}^", caret_padding(&self.source_line, self.span.column))
    }
}

/// Copies tabs from the source line so the caret lines up at any tab width.
fn caret_padding(line: &str, column: usize) -> String {
    let width = column.saturating_sub(1);
    let mut padding: String = line
        .chars()
        .take(width)
        .map(|c| if c == '\t' { '\t' } else { ' ' })
        .collect();
    let missing = width.saturating_sub(padding.chars().count());
    padding.extend(std::iter::repeat_n(' ', missing));
    padding
}

impl std::error::Error for SqlError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: usize, line: usize, column: usize) -> Span {
        Span {
            start,
            end: start + 1,
            line,
            column,
        }
    }

    #[test]
    fn display_shows_line_and_caret() {
        let error = SqlError::new(
            "unexpected character '@'",
            span(14, 2, 5),
            "SELECT 1,\n    @\nFROM t",
        );
        assert_eq!(
            error.to_string(),
            "line 2, column 5: unexpected character '@'\n    @\n    ^"
        );
    }

    #[test]
    fn display_at_end_of_input_points_past_the_line() {
        let error = SqlError::new("expected expression", span(6, 1, 7), "SELECT");
        assert_eq!(
            error.to_string(),
            "line 1, column 7: expected expression\nSELECT\n      ^"
        );
    }

    #[test]
    fn display_copies_tabs_into_the_padding() {
        let error = SqlError::new("bad", span(4, 1, 5), "\tx  @");
        assert_eq!(error.to_string(), "line 1, column 5: bad\n\tx  @\n\t   ^");
    }

    #[test]
    fn crlf_line_breaks_are_not_part_of_the_line() {
        let first = SqlError::new("bad", span(2, 1, 3), "a @\r\nb");
        assert_eq!(first.source_line, "a @");
        let second = SqlError::new("bad", span(7, 2, 3), "a @\r\nb @");
        assert_eq!(second.source_line, "b @");
        assert_eq!(second.to_string(), "line 2, column 3: bad\nb @\n  ^");
    }
}
