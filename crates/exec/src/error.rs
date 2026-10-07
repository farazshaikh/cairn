//! The error type returned by every `cairn-exec` operation.

use std::fmt;

use cairn_sql::{Span, SqlError};
use cairn_storage::StorageError;

/// The class of an [`ExecError`], for matching without comparing messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The SQL text did not tokenize or parse.
    Syntax,
    /// The storage layer failed (I/O, full database, pool exhausted).
    Storage,
    /// The file, catalog or a record is malformed.
    Corrupt,
    /// A table, column, alias or function does not exist.
    NotFound,
    /// A table, index or column name is already in use.
    AlreadyExists,
    /// A type mismatch or otherwise invalid statement.
    Type,
    /// NOT NULL, PRIMARY KEY or UNIQUE failed, or row ids ran out.
    Constraint,
    /// Integer overflow, real overflow or division by zero.
    Arithmetic,
    /// A statement this version does not run (non-SELECT EXPLAIN, an unknown
    /// pragma).
    Unsupported,
    /// A row or index key exceeds the storage size limits.
    TooLarge,
    /// The shared database file is poisoned after a failed sync; close every
    /// handle and reopen it.
    Unusable,
    /// Another handle holds the write lock, or readers block a checkpoint.
    Busy,
    /// BEGIN, COMMIT, ROLLBACK or a checkpoint used in the wrong transaction
    /// state.
    Transaction,
}

/// An execution error. When it relates to SQL text it carries the span and
/// the source line, and `Display` prints the same three-line caret form as
/// [`SqlError`]; otherwise `Display` prints the message alone.
#[derive(Debug)]
pub struct ExecError(Box<Inner>);

/// Boxed so `Result<_, ExecError>` stays small in deeply recursive code.
#[derive(Debug)]
struct Inner {
    kind: ErrorKind,
    message: String,
    span: Option<Span>,
    source_line: Option<String>,
    storage: Option<StorageError>,
}

impl ExecError {
    pub(crate) fn new(
        kind: ErrorKind,
        message: impl Into<String>,
        span: Option<Span>,
    ) -> ExecError {
        ExecError(Box::new(Inner {
            kind,
            message: message.into(),
            span,
            source_line: None,
            storage: None,
        }))
    }

    pub(crate) fn at(kind: ErrorKind, message: impl Into<String>, span: Span) -> ExecError {
        ExecError::new(kind, message, Some(span))
    }

    pub(crate) fn from_sql(error: SqlError, sql: &str) -> ExecError {
        let mut converted = ExecError::at(ErrorKind::Syntax, error.message, error.span);
        converted.0.source_line = Some(line_at(sql, error.span.start).to_string());
        converted
    }

    pub(crate) fn corrupt(message: impl Into<String>) -> ExecError {
        ExecError::new(ErrorKind::Corrupt, message, None)
    }

    /// Fills in the source line from the statement text, once.
    pub(crate) fn with_source(mut self, sql: &str) -> ExecError {
        if let (Some(span), None) = (self.0.span, &self.0.source_line) {
            self.0.source_line = Some(line_at(sql, span.start).to_string());
        }
        self
    }

    pub fn kind(&self) -> ErrorKind {
        self.0.kind
    }

    pub fn message(&self) -> &str {
        &self.0.message
    }

    pub fn span(&self) -> Option<Span> {
        self.0.span
    }

    pub fn source_line(&self) -> Option<&str> {
        self.0.source_line.as_deref()
    }
}

impl From<StorageError> for ExecError {
    fn from(error: StorageError) -> ExecError {
        let kind = match error {
            StorageError::Corrupt { .. }
            | StorageError::BadMagic { .. }
            | StorageError::FileTooShort { .. }
            | StorageError::NotPageMultiple { .. }
            | StorageError::UnsupportedVersion { .. } => ErrorKind::Corrupt,
            StorageError::Busy { .. } => ErrorKind::Busy,
            StorageError::NoTransaction
            | StorageError::TransactionOpen
            | StorageError::InvalidSavepoint => ErrorKind::Transaction,
            StorageError::Unusable => ErrorKind::Unusable,
            _ => ErrorKind::Storage,
        };
        ExecError(Box::new(Inner {
            kind,
            message: error.to_string(),
            span: None,
            source_line: None,
            storage: Some(error),
        }))
    }
}

fn line_at(src: &str, offset: usize) -> &str {
    let before = src.get(..offset).unwrap_or(src);
    let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
    let rest = src.get(line_start..).unwrap_or("");
    let line = rest.split('\n').next().unwrap_or("");
    line.strip_suffix('\r').unwrap_or(line)
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(span) = self.0.span else {
            return f.write_str(&self.0.message);
        };
        write!(
            f,
            "line {}, column {}: {}",
            span.line, span.column, self.0.message
        )?;
        let Some(line) = &self.0.source_line else {
            return Ok(());
        };
        write!(f, "\n{line}\n{}^", caret_padding(line, span.column))
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

impl std::error::Error for ExecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0
            .storage
            .as_ref()
            .map(|error| error as &(dyn std::error::Error + 'static))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_errors_keep_span_and_display_like_sql_error() {
        let sql = "SELECT 1;\nSELECT FROM";
        let error = cairn_sql::parse(sql).expect_err("parse error");
        let expected = error.to_string();
        let converted = ExecError::from_sql(error.clone(), sql);
        assert_eq!(converted.kind(), ErrorKind::Syntax);
        assert_eq!(converted.span(), Some(error.span));
        assert_eq!(converted.to_string(), expected);
    }

    #[test]
    fn display_without_span_is_the_message() {
        let error = ExecError::corrupt("bad page");
        assert_eq!(error.to_string(), "bad page");
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn transaction_storage_errors_get_their_own_kinds() {
        let busy = ExecError::from(StorageError::Busy { reason: "x" });
        assert_eq!(busy.kind(), ErrorKind::Busy);
        assert_eq!(
            ExecError::from(StorageError::NoTransaction).kind(),
            ErrorKind::Transaction
        );
        assert_eq!(
            ExecError::from(StorageError::Unusable).kind(),
            ErrorKind::Unusable
        );
    }

    #[test]
    fn storage_errors_expose_their_source() {
        let error = ExecError::from(StorageError::DatabaseFull);
        assert_eq!(error.kind(), ErrorKind::Storage);
        assert!(std::error::Error::source(&error).is_some());
    }
}
