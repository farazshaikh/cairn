//! Canonical `Display` round trip (milestone AC6): for every statement and
//! expression form, `parse(display(parse(s))) == parse(s)` and the printed
//! text is a fixed point. Also checks that no prefix of any input panics.

use cairn_sql::{
    ExprKind, Literal, MAX_DEPTH, SelectItem, Statement, StatementKind, parse, parse_expr,
};

/// Each entry exercises the forms named in its comment.
const CORPUS: &[&str] = &[
    // AC3 statements.
    "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, score REAL, ok BOOLEAN)",
    r#"create table if not exists "My Table" ("Select" text unique not null primary key, text real)"#,
    "DROP TABLE t",
    r#"drop table if exists "T""#,
    "CREATE INDEX i ON t (a)",
    r#"CREATE UNIQUE INDEX "Idx" ON t ("B")"#,
    "INSERT INTO t VALUES (1, 'a', NULL, TRUE)",
    "INSERT INTO t (a, b) VALUES (1, 2), (3, -4), (.5, 1e300)",
    "UPDATE t SET a = 1, b = b + 1",
    "UPDATE t SET a = NULL WHERE b <> 2 AND c IS NOT NULL",
    "DELETE FROM t",
    "DELETE FROM t WHERE id IN (1, 2, 3)",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "BEGIN; INSERT INTO t VALUES (1); COMMIT;",
    // AC4 SELECT clauses.
    "SELECT 1",
    "SELECT 1 + 2, 'x' AS y",
    "SELECT DISTINCT a, b FROM t",
    "SELECT * FROM t",
    "SELECT t.*, u.* FROM t JOIN u ON t.id = u.id",
    "SELECT a AS x FROM t AS s",
    "SELECT * FROM a INNER JOIN b ON TRUE LEFT JOIN c AS d ON d.k = a.k JOIN e ON e.x > 1",
    "SELECT a FROM t WHERE a > 1",
    "SELECT a, count(*) FROM t GROUP BY a, b",
    "SELECT a FROM t GROUP BY a HAVING count(*) > 1",
    "SELECT a FROM t ORDER BY a, b ASC, c DESC",
    "SELECT a FROM t LIMIT 10",
    "SELECT a FROM t LIMIT 10 OFFSET 5",
    "SELECT DISTINCT t.a, count(DISTINCT u.b) AS n FROM t AS t LEFT JOIN u ON u.id = t.id \
     WHERE t.a IS NOT NULL GROUP BY t.a HAVING count(*) >= 2 ORDER BY n DESC, t.a LIMIT 5 OFFSET 1",
    // AC5 literals, columns and functions.
    "SELECT 0, 42, 9223372036854775807, 1.5, .5, 1., 1e3, 2.5E-2, 0.1, 1e-400",
    "SELECT 'it''s', '', 'multi\nline', NULL, TRUE, FALSE",
    "SELECT -9223372036854775807 - 1",
    r#"SELECT t.col, "T"."Col", "a""b", "select""#,
    "SELECT count(*), count(DISTINCT a), max(a, b + 1), now()",
    // AC5 operators and predicates.
    "SELECT a OR b AND NOT c",
    "SELECT a <> b, a != b, a < b, a <= b, a > b, a >= b, a = b",
    "SELECT a || b, a + b, a - b, a * b, a / b, a % b, -a",
    "SELECT x BETWEEN 1 AND 2, x NOT BETWEEN a + 1 AND b || c",
    "SELECT x IN (1, 2), x NOT IN ('a')",
    "SELECT x LIKE 'a%', x NOT LIKE y || '%'",
    "SELECT x IS NULL, x IS NOT NULL",
    "SELECT CASE WHEN a = 1 THEN 'one' WHEN a = 2 THEN 'two' ELSE 'many' END",
    "SELECT CASE WHEN a THEN 1 END",
    // Parentheses the tree needs, and redundant ones it drops.
    "SELECT (a + b) * c, a - (b - c), -(a + b), - -a, a = (NOT b)",
    "SELECT (a IS NULL) || b, NOT (a AND b), (a OR b) AND c, ((a))",
    "SELECT (x BETWEEN 1 AND 2) BETWEEN y AND z, a || (b IS NULL)",
    "SELECT (a = b) = c, a = (b = c), (NOT a) = b",
    // Precedence and associativity (see tests/precedence.rs).
    "SELECT a OR b AND c, NOT a = b, a = b IS NULL, a < b BETWEEN 1 AND 2",
    "SELECT a = b IN (1, 2), a = b LIKE c, a || b IS NULL, a LIKE b || c",
    "SELECT a || b + c, a + b * c, -a * b, a % b * -c",
    "SELECT x BETWEEN 1 AND 2 AND y, a - b - c, a / b * c, a < b < c",
    // Comments and odd spacing.
    "SELECT /* c */ a -- d\nFROM\tt\r\nWHERE(a)=1",
];

fn statements(src: &str) -> Vec<Statement> {
    parse(src).unwrap_or_else(|error| panic!("{src:?}:\n{error}"))
}

fn render(statements: &[Statement]) -> String {
    let texts: Vec<String> = statements.iter().map(ToString::to_string).collect();
    texts.join("; ")
}

fn assert_round_trip(src: &str) {
    let first = statements(src);
    let printed = render(&first);
    let second = statements(&printed);
    assert_eq!(first, second, "\nsource:  {src}\nprinted: {printed}");
    assert_eq!(
        render(&second),
        printed,
        "display is not a fixed point for {src}"
    );
}

#[test]
fn every_corpus_entry_round_trips() {
    for src in CORPUS {
        assert_round_trip(src);
    }
}

#[test]
fn reals_round_trip_bit_for_bit() {
    let values = [0.1, 1e300, 5e-324, f64::MAX, 1.0, 123.456, 2.5e-7];
    let src = "SELECT 0.1, 1e300, 5e-324, 1.7976931348623157e308, 1.0, 123.456, 2.5e-7";
    let printed = render(&statements(src));
    let reparsed = statements(&printed);
    let StatementKind::Select(select) = &reparsed[0].node else {
        panic!("expected SELECT");
    };
    let bits: Vec<u64> = select
        .items
        .iter()
        .map(|item| match &item.node {
            SelectItem::Expr {
                expr:
                    cairn_sql::Spanned {
                        node: ExprKind::Literal(Literal::Real(value)),
                        ..
                    },
                ..
            } => value.to_bits(),
            other => panic!("not a real: {other:?}"),
        })
        .collect();
    let expected: Vec<u64> = values.iter().map(|value| value.to_bits()).collect();
    assert_eq!(bits, expected, "printed: {printed}");
}

#[test]
fn inputs_at_the_nesting_limit_round_trip() {
    let n = MAX_DEPTH;
    let inputs = [
        format!("{}1{}", "(".repeat(n), ")".repeat(n)),
        format!("1{}", " + 1".repeat(n)),
        format!("{}a", "NOT ".repeat(n)),
        format!("{}1", "- ".repeat(n)),
        format!(
            "{}1{}",
            "CASE WHEN TRUE THEN ".repeat(n),
            " ELSE 0 END".repeat(n)
        ),
        format!("{}1{}", "f(".repeat(n), ")".repeat(n)),
        format!("{}1{}", "1 - (".repeat(n), ")".repeat(n)),
    ];
    for src in inputs {
        let first = parse_expr(&src).unwrap_or_else(|error| panic!("{error}"));
        let printed = first.to_string();
        let second = parse_expr(&printed).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(first, second);
        assert_eq!(second.to_string(), printed);
    }
}

/// Truncated input is the most common malformed input; no prefix may panic.
#[test]
fn no_prefix_of_any_corpus_entry_panics() {
    for src in CORPUS {
        for (end, _) in src.char_indices().chain([(src.len(), ' ')]) {
            let _ = parse(&src[..end]);
        }
    }
}
