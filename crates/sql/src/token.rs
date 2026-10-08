//! Tokens produced by [`crate::tokenize`].

use crate::span::Span;

/// One lexical token and the source text it covers.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// What the token is.
    pub kind: TokenKind,
    /// Where the token is in the source.
    pub span: Span,
}

/// The kind of a token, with its value for identifiers and literals.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// A reserved word.
    Keyword(Keyword),
    /// An identifier, lowercased unless it was double-quoted.
    Ident(String),
    /// An integer literal that fits `i64`.
    Integer(i64),
    /// A finite real literal.
    Real(f64),
    /// A single-quoted string with `''` escapes resolved.
    String(String),
    /// `=`
    Eq,
    /// `<>` or `!=`.
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `||`
    Concat,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// `;`
    Semicolon,
    /// End of input; always the last token.
    Eof,
}

macro_rules! keywords {
    ($($variant:ident $text:literal)*) => {
        /// A reserved word. Reserved words are never identifiers; double-quote
        /// them (`"order"`) to use them as names.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Keyword {
            $(#[doc = concat!("`", $text, "`")] $variant,)*
        }

        impl Keyword {
            /// Every reserved word, in alphabetical order.
            pub const ALL: &'static [Keyword] = &[$(Keyword::$variant,)*];

            /// The keyword in upper case, as printed in canonical SQL.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Keyword::$variant => $text,)*
                }
            }
        }
    };
}

keywords! {
    And "AND" As "AS" Asc "ASC" Begin "BEGIN" Between "BETWEEN" By "BY"
    Case "CASE" Commit "COMMIT" Create "CREATE" Delete "DELETE" Desc "DESC"
    Distinct "DISTINCT" Drop "DROP" Else "ELSE" End "END" Exists "EXISTS"
    False "FALSE" From "FROM" Group "GROUP" Having "HAVING" If "IF" In "IN"
    Index "INDEX" Inner "INNER" Insert "INSERT" Into "INTO" Is "IS" Join "JOIN"
    Key "KEY" Left "LEFT" Like "LIKE" Limit "LIMIT" Not "NOT" Null "NULL"
    Offset "OFFSET" On "ON" Or "OR" Order "ORDER" Primary "PRIMARY"
    Rollback "ROLLBACK" Select "SELECT" Set "SET" Table "TABLE" Then "THEN"
    True "TRUE" Unique "UNIQUE" Update "UPDATE" Values "VALUES" When "WHEN"
    Where "WHERE"
}

impl Keyword {
    /// Finds the reserved word spelled `word`, ignoring ASCII case.
    pub fn lookup(word: &str) -> Option<Keyword> {
        Keyword::ALL
            .iter()
            .copied()
            .find(|keyword| keyword.as_str().eq_ignore_ascii_case(word))
    }
}
