//! Source positions.

/// A region of SQL source text.
///
/// `start` and `end` are byte offsets, with `end` exclusive. `line` and
/// `column` are 1-based and describe `start`; the column counts characters,
/// not bytes. Only `\n` starts a new line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    /// Byte offset of the first byte.
    pub start: usize,
    /// Byte offset just past the last byte.
    pub end: usize,
    /// 1-based line of `start`.
    pub line: usize,
    /// 1-based column of `start`, in characters.
    pub column: usize,
}

impl Span {
    /// Returns the span from the start of `self` to the end of `last`.
    pub fn until(self, last: Span) -> Span {
        Span {
            end: last.end,
            ..self
        }
    }
}
