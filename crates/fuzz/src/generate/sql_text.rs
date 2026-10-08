//! Random SQL text for the robustness target: arbitrary bytes, streams of
//! tokens, and statements nested at the expression depth limit.

use crate::rng::Rng;

/// Up to 512 random bytes, read as UTF-8 with replacement characters, and
/// sometimes seeded with a SQL fragment.
pub fn bytes(rng: &mut Rng) -> String {
    let len = rng.index(513);
    let raw: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
    let text = String::from_utf8_lossy(&raw).into_owned();
    if rng.chance(1, 2) {
        format!("{}{text}", rng.pick(&FRAGMENTS))
    } else {
        text
    }
}

const FRAGMENTS: [&str; 6] = [
    "SELECT ",
    "CREATE TABLE t (",
    "INSERT INTO t VALUES (",
    "EXPLAIN SELECT ",
    "PRAGMA ",
    "SELECT * FROM t WHERE ",
];

/// Lexer vocabulary, including malformed tokens.
const VOCABULARY: [&str; 92] = [
    "AND",
    "AS",
    "ASC",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASE",
    "COMMIT",
    "CREATE",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXISTS",
    "FALSE",
    "FROM",
    "GROUP",
    "HAVING",
    "IF",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NOT",
    "NULL",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "PRIMARY",
    "ROLLBACK",
    "SELECT",
    "SET",
    "TABLE",
    "THEN",
    "TRUE",
    "UNIQUE",
    "UPDATE",
    "VALUES",
    "WHEN",
    "WHERE",
    "EXPLAIN",
    "PRAGMA",
    "INTEGER",
    "TEXT",
    "count",
    "sum",
    "t",
    "a",
    "\"Q\"",
    "\"un",
    "0",
    "42",
    "9223372036854775807",
    "9223372036854775808",
    "1.5",
    ".5",
    "1.",
    "1e999",
    "1a",
    "'s'",
    "'it''s'",
    "'open",
    "-- c\n",
    "/* c */",
    "/* open",
    "=",
    "<>",
    "!=",
    "<=",
    ">=",
    "||",
    "+",
    "-",
    "*",
    "/",
    "%",
    "(",
    ")",
    ",",
    ".",
    ";",
    "é",
];

const SPACES: [&str; 6] = [" ", " ", "", "\n", "\t", "\r\n"];

/// Up to 64 tokens from the vocabulary, joined by random whitespace.
pub fn tokens(rng: &mut Rng) -> String {
    let count = rng.index(65);
    let mut out = String::new();
    for _ in 0..count {
        let word = rng.pick(&VOCABULARY);
        out.push_str(word);
        let space = rng.pick(&SPACES);
        out.push_str(space);
    }
    out
}

/// A SELECT whose expression is `depth` levels deep, built from one
/// nesting form: parentheses, unary minus, NOT, CASE or a call.
pub fn nested(rng: &mut Rng, depth: usize) -> String {
    let (open, close, leaf) = *rng.pick(&[
        ("(", ")", "1"),
        ("- ", "", "1"),
        ("NOT ", "", "TRUE"),
        ("abs(", ")", "1"),
        ("CASE WHEN TRUE THEN ", " END", "1"),
    ]);
    let mut sql = "SELECT ".to_string();
    for _ in 0..depth {
        sql.push_str(open);
    }
    sql.push_str(leaf);
    for _ in 0..depth {
        sql.push_str(close);
    }
    sql
}

/// A chain of `length` binary operators, which counts towards the depth
/// limit as tree height.
pub fn chain(length: usize) -> String {
    let mut sql = "SELECT 1".to_string();
    for _ in 0..length {
        sql.push_str(" + 1");
    }
    sql
}
