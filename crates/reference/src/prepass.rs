//! `EXPLAIN` and `PRAGMA`, which the README lets cairn recognise before
//! parsing: an unquoted `explain` at the start of a statement marks the
//! statement after it, and `pragma name` followed by `;` or the end of
//! input is a statement of its own. Both are blanked out of the text (line
//! breaks kept) so the parser sees only ordinary SQL.

use cairn_sql::{Span, TokenKind, tokenize};

use crate::RErrorKind;

/// The text with directives blanked, and where they were.
pub(crate) struct Directives {
    pub text: String,
    pub explains: Vec<Span>,
    /// Each pragma's span and lowercased name.
    pub pragmas: Vec<(Span, String)>,
    /// Whether anything besides directives remains.
    pub has_sql: bool,
}

pub(crate) fn scan(sql: &str) -> Result<Directives, RErrorKind> {
    let tokens = tokenize(sql).map_err(|_| RErrorKind::Syntax)?;
    let source = |span: Span| sql.get(span.start..span.end).unwrap_or("");
    let is_word = |i: usize, word: &str| {
        tokens.get(i).is_some_and(|t| {
            matches!(&t.kind, TokenKind::Ident(w) if w == word)
                && source(t.span).eq_ignore_ascii_case(word)
        })
    };
    let mut out = Directives {
        text: sql.to_string(),
        explains: Vec::new(),
        pragmas: Vec::new(),
        has_sql: false,
    };
    let mut at_start = true;
    let mut i = 0;
    while let Some(token) = tokens.get(i) {
        if token.kind == TokenKind::Eof {
            break;
        }
        if at_start && is_word(i, "explain") {
            blank(&mut out.text, token.span.start, token.span.end);
            out.explains.push(token.span);
            i += 1;
            continue;
        }
        if at_start && is_word(i, "pragma") {
            let name_token = tokens.get(i + 1).ok_or(RErrorKind::Syntax)?;
            let name = match &name_token.kind {
                TokenKind::Ident(name) if !source(name_token.span).starts_with('"') => {
                    name.to_ascii_lowercase()
                }
                _ => return Err(RErrorKind::Syntax),
            };
            let (end, used) = match tokens.get(i + 2).map(|t| &t.kind) {
                Some(TokenKind::Semicolon) => (tokens.get(i + 2).map_or(0, |t| t.span.end), 3),
                Some(TokenKind::Eof) | None => (name_token.span.end, 2),
                Some(_) => return Err(RErrorKind::Syntax),
            };
            blank(&mut out.text, token.span.start, end);
            out.pragmas.push((token.span.until(name_token.span), name));
            i += used;
            continue;
        }
        out.has_sql = true;
        at_start = token.kind == TokenKind::Semicolon;
        i += 1;
    }
    Ok(out)
}

fn blank(text: &mut String, start: usize, end: usize) {
    let Some(original) = text.get(start..end) else {
        return;
    };
    let spaces: String = original
        .chars()
        .map(|c| if c == '\n' || c == '\r' { c } else { ' ' })
        .collect();
    if spaces.len() == original.len() {
        text.replace_range(start..end, &spaces);
    }
}
