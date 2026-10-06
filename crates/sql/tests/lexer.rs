//! Lexical rules (milestone AC1, AC2): token kinds, values and spans.

use cairn_sql::{Keyword, Span, TokenKind, tokenize};

fn kinds(src: &str) -> Vec<TokenKind> {
    let tokens = tokenize(src).unwrap_or_else(|error| panic!("{src:?}: {error}"));
    tokens.into_iter().map(|token| token.kind).collect()
}

/// Kinds without the trailing `Eof`.
fn body(src: &str) -> Vec<TokenKind> {
    let mut kinds = kinds(src);
    assert_eq!(kinds.pop(), Some(TokenKind::Eof));
    kinds
}

fn span(start: usize, end: usize, line: usize, column: usize) -> Span {
    Span {
        start,
        end,
        line,
        column,
    }
}

fn ident(name: &str) -> TokenKind {
    TokenKind::Ident(name.to_string())
}

#[test]
fn keywords_are_case_insensitive() {
    let select = TokenKind::Keyword(Keyword::Select);
    assert_eq!(
        body("select SeLeCt SELECT"),
        vec![select.clone(), select.clone(), select]
    );
}

#[test]
fn there_are_fifty_reserved_words_and_type_names_are_not_among_them() {
    assert_eq!(Keyword::ALL.len(), 50);
    for name in ["integer", "real", "text", "boolean", "count"] {
        assert_eq!(Keyword::lookup(name), None, "{name}");
    }
    assert_eq!(Keyword::lookup("wHeRe"), Some(Keyword::Where));
}

#[test]
fn unquoted_identifiers_are_lowercased() {
    assert_eq!(
        body("Foo _bar x1 ABC"),
        vec![ident("foo"), ident("_bar"), ident("x1"), ident("abc")]
    );
}

#[test]
fn quoted_identifiers_keep_case_and_unescape_quotes() {
    assert_eq!(body(r#""Foo""Bar""#), vec![ident("Foo\"Bar")]);
    assert_eq!(
        body(r#""select" "a b""#),
        vec![ident("select"), ident("a b")]
    );
}

#[test]
fn strings_unescape_doubled_quotes() {
    let string = |text: &str| TokenKind::String(text.to_string());
    assert_eq!(
        body("'it''s' '' 'a\nb'"),
        vec![string("it's"), string(""), string("a\nb")]
    );
}

#[test]
fn integers_fit_i64() {
    assert_eq!(
        body("0 42 9223372036854775807"),
        vec![
            TokenKind::Integer(0),
            TokenKind::Integer(42),
            TokenKind::Integer(i64::MAX)
        ]
    );
}

#[test]
fn reals_need_a_decimal_point_or_exponent() {
    let reals = body("1.5 .5 1. 1e3 2.5E-2 3E+2 1.e2 1e-400");
    let expected = [1.5, 0.5, 1.0, 1000.0, 0.025, 300.0, 100.0, 0.0];
    assert_eq!(reals.len(), expected.len());
    for (kind, value) in reals.iter().zip(expected) {
        assert_eq!(*kind, TokenKind::Real(value));
    }
}

#[test]
fn null_true_and_false_are_keywords() {
    assert_eq!(
        body("NULL true False"),
        vec![
            TokenKind::Keyword(Keyword::Null),
            TokenKind::Keyword(Keyword::True),
            TokenKind::Keyword(Keyword::False)
        ]
    );
}

#[test]
fn every_operator_is_recognised() {
    use TokenKind::*;
    assert_eq!(
        body("= <> != < <= > >= + - * / % || ( ) , . ;"),
        vec![
            Eq, NotEq, NotEq, Lt, LtEq, Gt, GtEq, Plus, Minus, Star, Slash, Percent, Concat,
            LParen, RParen, Comma, Dot, Semicolon
        ]
    );
    assert_eq!(body("a<=b"), vec![ident("a"), LtEq, ident("b")]);
    assert_eq!(body("1-2"), vec![Integer(1), Minus, Integer(2)]);
    assert_eq!(body("a||b"), vec![ident("a"), Concat, ident("b")]);
    assert_eq!(body("t.c"), vec![ident("t"), Dot, ident("c")]);
}

#[test]
fn comments_are_skipped() {
    assert_eq!(
        body("a -- line comment\nb /* block\ncomment */ c /**/ d --"),
        vec![ident("a"), ident("b"), ident("c"), ident("d")]
    );
}

#[test]
fn spans_track_offset_line_and_column_across_comments_and_crlf() {
    let src = "SELECT a,\r\n  b -- c\n/* x\n */ FROM t";
    let spans: Vec<Span> = tokenize(src)
        .expect("valid input")
        .into_iter()
        .map(|token| token.span)
        .collect();
    assert_eq!(
        spans,
        vec![
            span(0, 6, 1, 1),
            span(7, 8, 1, 8),
            span(8, 9, 1, 9),
            span(13, 14, 2, 3),
            span(29, 33, 4, 5),
            span(34, 35, 4, 10),
            span(35, 35, 4, 11),
        ]
    );
}

#[test]
fn columns_count_characters_not_bytes() {
    let tokens = tokenize("'é' x").expect("valid input");
    assert_eq!(tokens[1].span, span(5, 6, 1, 5));
}

#[test]
fn empty_input_is_just_eof() {
    let tokens = tokenize("").expect("valid input");
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].kind, TokenKind::Eof);
    assert_eq!(tokens[0].span, span(0, 0, 1, 1));
}

#[test]
fn tokenize_reports_lexical_errors_at_their_span() {
    let error = tokenize("SELECT 'abc").expect_err("unterminated");
    assert_eq!(error.message, "unterminated string literal");
    assert_eq!(error.span, span(7, 11, 1, 8));
    let error = tokenize("x @").expect_err("unknown character");
    assert_eq!(error.span, span(2, 3, 1, 3));
}
