//! `EXPLAIN <select>` and `PRAGMA <name>` without parser support.
//!
//! `cairn-sql` has neither statement, so [`preprocess`] finds them in the
//! token stream before parsing and replaces their bytes with spaces. The
//! text keeps its length and line breaks, so every span in the parsed
//! statements still points into the original input.
//!
//! - An unquoted `explain` at the start of a statement is a marker for the
//!   statement that follows it.
//! - An unquoted `pragma` at the start of a statement, followed by an
//!   unquoted name and then `;` or the end of input, is a whole statement.
//!   Its `;` is blanked too, because the parser rejects empty statements.
//!
//! [`Prepass::items`] merges the parsed statements and the pragmas back into
//! source order and attaches each EXPLAIN marker to the item after it.

use cairn_sql::{Span, Statement, Token, TokenKind, tokenize};

use crate::error::{ErrorKind, ExecError};

/// A `PRAGMA name` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Pragma {
    /// From `pragma` to the end of the name.
    pub span: Span,
    /// The name, ASCII-lowercased.
    pub name: String,
    pub name_span: Span,
}

/// The input with directives blanked out.
pub struct Prepass {
    pub text: String,
    pub explains: Vec<Span>,
    pub pragmas: Vec<Pragma>,
    /// Whether anything other than directives remains to be parsed.
    pub has_statements: bool,
}

/// One unit of execution, in source order.
#[derive(Clone, Copy)]
pub enum Item<'a> {
    Statement(&'a Statement),
    Pragma(&'a Pragma),
}

impl Item<'_> {
    fn span(&self) -> Span {
        match self {
            Item::Statement(statement) => statement.span,
            Item::Pragma(pragma) => pragma.span,
        }
    }
}

impl Prepass {
    /// The statements and pragmas in source order, each with the EXPLAIN
    /// marker in front of it, if any. A marker that nothing follows
    /// (`SELECT 1; EXPLAIN`) is a syntax error rather than being dropped.
    pub fn items<'a>(
        &'a self,
        statements: &'a [Statement],
    ) -> Result<Vec<(Item<'a>, Option<Span>)>, ExecError> {
        let mut items: Vec<Item<'a>> = statements
            .iter()
            .map(Item::Statement)
            .chain(self.pragmas.iter().map(Item::Pragma))
            .collect();
        items.sort_by_key(|item| item.span().start);
        let mut assigned = Vec::with_capacity(items.len());
        let mut previous_end = 0;
        for item in items {
            let span = item.span();
            let marker = self
                .explains
                .iter()
                .copied()
                .find(|m| m.start >= previous_end && m.start < span.start);
            assigned.push((item, marker));
            previous_end = span.end;
        }
        match self.explains.iter().find(|m| m.start >= previous_end) {
            Some(dangling) => Err(ExecError::at(
                ErrorKind::Syntax,
                "expected SELECT after EXPLAIN",
                *dangling,
            )),
            None => Ok(assigned),
        }
    }
}

pub fn preprocess(sql: &str) -> Result<Prepass, ExecError> {
    let tokens = tokenize(sql).map_err(|e| ExecError::from_sql(e, sql))?;
    let mut pass = Prepass {
        text: sql.to_string(),
        explains: Vec::new(),
        pragmas: Vec::new(),
        has_statements: false,
    };
    let mut at_start = true;
    let mut i = 0;
    while let Some(token) = tokens.get(i) {
        if token.kind == TokenKind::Eof {
            break;
        }
        if at_start && is_word(sql, token, "explain") {
            blank(&mut pass.text, token.span.start, token.span.end);
            pass.explains.push(token.span);
            i += 1;
            continue;
        }
        if at_start && is_word(sql, token, "pragma") {
            i += pragma(sql, &tokens, i, &mut pass)?;
            continue;
        }
        pass.has_statements = true;
        at_start = token.kind == TokenKind::Semicolon;
        i += 1;
    }
    Ok(pass)
}

/// Recognises the pragma starting at token `i` and returns how many tokens
/// it consumed.
fn pragma(sql: &str, tokens: &[Token], i: usize, pass: &mut Prepass) -> Result<usize, ExecError> {
    let (Some(keyword), Some(name_token)) = (tokens.get(i), tokens.get(i + 1)) else {
        return Err(ExecError::corrupt("token stream ends without end of input"));
    };
    let name = match &name_token.kind {
        TokenKind::Ident(name) if !is_quoted(sql, name_token) => name.to_ascii_lowercase(),
        _ => {
            return Err(ExecError::at(
                ErrorKind::Syntax,
                "expected pragma name",
                name_token.span,
            ));
        }
    };
    let end = match tokens.get(i + 2) {
        Some(t) if t.kind == TokenKind::Semicolon => (t.span.end, 3),
        Some(t) if t.kind == TokenKind::Eof => (name_token.span.end, 2),
        Some(t) => {
            return Err(ExecError::at(
                ErrorKind::Syntax,
                "expected ';' or end of input after PRAGMA",
                t.span,
            ));
        }
        None => (name_token.span.end, 2),
    };
    blank(&mut pass.text, keyword.span.start, end.0);
    pass.pragmas.push(Pragma {
        span: keyword.span.until(name_token.span),
        name,
        name_span: name_token.span,
    });
    Ok(end.1)
}

/// An unquoted identifier spelled `word` in any ASCII case.
fn is_word(sql: &str, token: &Token, word: &str) -> bool {
    matches!(&token.kind, TokenKind::Ident(w) if w == word)
        && sql
            .get(token.span.start..token.span.end)
            .is_some_and(|source| source.eq_ignore_ascii_case(word))
}

fn is_quoted(sql: &str, token: &Token) -> bool {
    sql.get(token.span.start..token.span.end)
        .is_some_and(|source| source.starts_with('"'))
}

/// Replaces `[start, end)` with spaces, keeping line breaks so that line
/// and column numbers of later tokens do not move.
fn blank(text: &mut String, start: usize, end: usize) {
    let Some(original) = text.get(start..end) else {
        return;
    };
    let replacement: String = original
        .chars()
        .map(|c| if c == '\n' || c == '\r' { c } else { ' ' })
        .collect();
    if replacement.len() == original.len() {
        text.replace_range(start..end, &replacement);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explain_markers_are_blanked_only_at_statement_start() {
        let sql = "EXPLAIN SELECT 1; explain select explain FROM \"explain\";\nExplain\tSELECT 2";
        let pre = preprocess(sql).expect("tokenize");
        assert_eq!(pre.explains.len(), 3);
        assert_eq!(pre.text.len(), sql.len());
        assert!(pre.text.starts_with("        SELECT 1;"));
        assert!(pre.text.contains("select explain FROM \"explain\""));
        assert_eq!(pre.explains.get(2).map(|m| m.line), Some(2));
    }

    #[test]
    fn quoted_words_are_not_directives() {
        let pre = preprocess("\"explain\"; \"pragma\"").expect("tokenize");
        assert!(pre.explains.is_empty() && pre.pragmas.is_empty());
        assert!(pre.has_statements);
    }

    #[test]
    fn pragmas_are_blanked_with_their_semicolon() {
        let sql = "SELECT 1; PRAGMA Checkpoint;\npragma integrity_check";
        let pre = preprocess(sql).expect("prepass");
        assert_eq!(
            pre.text,
            format!("SELECT 1;{}\n{}", " ".repeat(19), " ".repeat(22))
        );
        let names: Vec<&str> = pre.pragmas.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["checkpoint", "integrity_check"]);
        assert_eq!(pre.pragmas[1].span.line, 2);
        assert!(pre.has_statements);
        assert!(
            !preprocess("PRAGMA checkpoint;")
                .expect("prepass")
                .has_statements
        );
    }

    #[test]
    fn malformed_pragmas_are_syntax_errors() {
        for (sql, message) in [
            ("PRAGMA", "expected pragma name"),
            ("PRAGMA;", "expected pragma name"),
            ("PRAGMA \"x\"", "expected pragma name"),
            ("PRAGMA x y", "expected ';' or end of input after PRAGMA"),
        ] {
            let error = preprocess(sql).err().expect(sql);
            assert_eq!(
                (error.kind(), error.message()),
                (ErrorKind::Syntax, message),
                "{sql}"
            );
        }
    }
}
