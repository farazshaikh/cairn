//! Tokenizer for the cairn SQL subset.

use crate::error::SqlError;
use crate::span::Span;
use crate::token::{Keyword, Token, TokenKind};

/// Splits SQL source into tokens. The last token is always [`TokenKind::Eof`].
///
/// Comments and whitespace are skipped. Errors point at the start of the
/// offending token: an unterminated string, quoted identifier or block
/// comment, an unknown character, or a number out of range.
pub fn tokenize(src: &str) -> Result<Vec<Token>, SqlError> {
    Lexer {
        src,
        pos: 0,
        line: 1,
        column: 1,
    }
    .run()
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
    line: usize,
    column: usize,
}

impl Lexer<'_> {
    fn run(mut self) -> Result<Vec<Token>, SqlError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_trivia()?;
            let start = self.here();
            let Some(c) = self.peek() else {
                tokens.push(Token {
                    kind: TokenKind::Eof,
                    span: start,
                });
                return Ok(tokens);
            };
            let kind = self.token(c, start)?;
            tokens.push(Token {
                kind,
                span: self.span_from(start),
            });
        }
    }

    fn skip_trivia(&mut self) -> Result<(), SqlError> {
        loop {
            match (self.peek(), self.peek_second()) {
                (Some(' ' | '\t' | '\n' | '\r'), _) => {
                    self.bump();
                }
                (Some('-'), Some('-')) => self.skip_line_comment(),
                (Some('/'), Some('*')) => self.skip_block_comment()?,
                _ => return Ok(()),
            }
        }
    }

    fn skip_line_comment(&mut self) {
        while self.peek().is_some_and(|c| c != '\n') {
            self.bump();
        }
    }

    fn skip_block_comment(&mut self) -> Result<(), SqlError> {
        let start = self.here();
        self.bump();
        self.bump();
        loop {
            match (self.peek(), self.peek_second()) {
                (None, _) => {
                    return Err(self.error("unterminated block comment", self.span_from(start)));
                }
                (Some('*'), Some('/')) => {
                    self.bump();
                    self.bump();
                    return Ok(());
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    fn token(&mut self, c: char, start: Span) -> Result<TokenKind, SqlError> {
        match c {
            'a'..='z' | 'A'..='Z' | '_' => Ok(self.word()),
            '"' => self.quoted_identifier(start),
            '\'' => self.string(start),
            '0'..='9' => self.number(start),
            '.' if self.peek_second().is_some_and(|next| next.is_ascii_digit()) => {
                self.number(start)
            }
            _ => self.operator(c, start),
        }
    }

    fn word(&mut self) -> TokenKind {
        let start = self.pos;
        while self.peek().is_some_and(is_identifier_char) {
            self.bump();
        }
        let word = self
            .src
            .get(start..self.pos)
            .unwrap_or("")
            .to_ascii_lowercase();
        match Keyword::lookup(&word) {
            Some(keyword) => TokenKind::Keyword(keyword),
            None => TokenKind::Ident(word),
        }
    }

    fn quoted_identifier(&mut self, start: Span) -> Result<TokenKind, SqlError> {
        let name = self.quoted('"', start, "unterminated quoted identifier")?;
        if name.is_empty() {
            return Err(self.error("empty quoted identifier", self.span_from(start)));
        }
        Ok(TokenKind::Ident(name))
    }

    fn string(&mut self, start: Span) -> Result<TokenKind, SqlError> {
        let text = self.quoted('\'', start, "unterminated string literal")?;
        Ok(TokenKind::String(text))
    }

    /// Reads the text between two `quote` characters; a doubled quote stands for one.
    fn quoted(&mut self, quote: char, start: Span, unterminated: &str) -> Result<String, SqlError> {
        self.bump();
        let mut text = String::new();
        loop {
            match self.bump() {
                None => return Err(self.error(unterminated, self.span_from(start))),
                Some(c) if c == quote => {
                    if self.peek() != Some(quote) {
                        return Ok(text);
                    }
                    self.bump();
                    text.push(quote);
                }
                Some(c) => text.push(c),
            }
        }
    }

    fn number(&mut self, start: Span) -> Result<TokenKind, SqlError> {
        let mut is_real = false;
        self.skip_digits();
        if self.peek() == Some('.') {
            self.bump();
            self.skip_digits();
            is_real = true;
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            if self.skip_digits() == 0 {
                return Err(self.error(
                    "expected digits after exponent in number literal",
                    self.span_from(start),
                ));
            }
            is_real = true;
        }
        if let Some(c) = self.peek().filter(|&c| is_identifier_char(c) || c == '"') {
            let at = self.here();
            self.bump();
            return Err(self.error(
                format!("unexpected character '{c}' after number"),
                self.span_from(at),
            ));
        }
        let text = self.src.get(start.start..self.pos).unwrap_or("");
        if !is_real {
            return text
                .parse()
                .map(TokenKind::Integer)
                .map_err(|_| self.error("integer literal out of range", self.span_from(start)));
        }
        match normalize_real(text).parse::<f64>() {
            Ok(value) if value.is_finite() => Ok(TokenKind::Real(value)),
            _ => Err(self.error("real literal out of range", self.span_from(start))),
        }
    }

    fn skip_digits(&mut self) -> usize {
        let mut count = 0;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
            count += 1;
        }
        count
    }

    fn operator(&mut self, c: char, start: Span) -> Result<TokenKind, SqlError> {
        let two_char = match (c, self.peek_second()) {
            ('<', Some('=')) => Some(TokenKind::LtEq),
            ('<', Some('>')) | ('!', Some('=')) => Some(TokenKind::NotEq),
            ('>', Some('=')) => Some(TokenKind::GtEq),
            ('|', Some('|')) => Some(TokenKind::Concat),
            _ => None,
        };
        self.bump();
        if let Some(kind) = two_char {
            self.bump();
            return Ok(kind);
        }
        let kind = match c {
            '=' => TokenKind::Eq,
            '<' => TokenKind::Lt,
            '>' => TokenKind::Gt,
            '+' => TokenKind::Plus,
            '-' => TokenKind::Minus,
            '*' => TokenKind::Star,
            '/' => TokenKind::Slash,
            '%' => TokenKind::Percent,
            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            ',' => TokenKind::Comma,
            '.' => TokenKind::Dot,
            ';' => TokenKind::Semicolon,
            _ => {
                return Err(
                    self.error(format!("unexpected character '{c}'"), self.span_from(start))
                );
            }
        };
        Ok(kind)
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos..)?.chars().next()
    }

    fn peek_second(&self) -> Option<char> {
        self.src.get(self.pos..)?.chars().nth(1)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    fn here(&self) -> Span {
        Span {
            start: self.pos,
            end: self.pos,
            line: self.line,
            column: self.column,
        }
    }

    fn span_from(&self, start: Span) -> Span {
        Span {
            end: self.pos,
            ..start
        }
    }

    fn error(&self, message: impl Into<String>, span: Span) -> SqlError {
        SqlError::new(message, span, self.src)
    }
}

fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Adds the zero digits in `.5`, `1.` and `1.e3` so the text has the
/// `int.frac` shape every float parser accepts.
fn normalize_real(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len() + 2);
    if text.starts_with('.') {
        normalized.push('0');
    }
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        normalized.push(c);
        if c == '.' && !chars.peek().is_some_and(|next| next.is_ascii_digit()) {
            normalized.push('0');
        }
    }
    normalized
}
