//! Operator precedence (milestone AC5), checked by printing each parsed
//! tree fully parenthesised. Lowest to highest: OR, AND, NOT, comparison,
//! IS/BETWEEN/IN/LIKE, `||`, `+ -`, `* / %`, unary `-`.

use cairn_sql::parse_expr;

fn full(src: &str) -> String {
    let expr = parse_expr(src).unwrap_or_else(|error| panic!("{src:?}:\n{error}"));
    expr.fully_parenthesized().to_string()
}

fn assert_shapes(cases: &[(&str, &str)]) {
    for (src, shape) in cases {
        assert_eq!(full(src), *shape, "{src}");
    }
}

#[test]
fn each_level_binds_tighter_than_the_one_below() {
    assert_shapes(&[
        ("a OR b AND c", "(a OR (b AND c))"),
        ("a AND b OR c", "((a AND b) OR c)"),
        ("a AND NOT b", "(a AND (NOT b))"),
        ("NOT a AND b", "((NOT a) AND b)"),
        ("NOT a OR b", "((NOT a) OR b)"),
        ("NOT a = b", "(NOT (a = b))"),
        ("NOT x IS NULL", "(NOT (x IS NULL))"),
        ("a = b IS NULL", "(a = (b IS NULL))"),
        ("a < b BETWEEN 1 AND 2", "(a < (b BETWEEN 1 AND 2))"),
        ("a = b IN (1, 2)", "(a = (b IN (1, 2)))"),
        ("a = b LIKE c", "(a = (b LIKE c))"),
        ("a || b IS NULL", "((a || b) IS NULL)"),
        ("a LIKE b || c", "(a LIKE (b || c))"),
        ("a || b + c", "(a || (b + c))"),
        ("a + b || c", "((a + b) || c)"),
        ("a + b * c", "(a + (b * c))"),
        ("a * b + c", "((a * b) + c)"),
        ("-a * b", "((-a) * b)"),
        ("a % b * -c", "((a % b) * (-c))"),
    ]);
}

#[test]
fn binary_operators_are_left_associative() {
    assert_shapes(&[
        ("a - b - c", "((a - b) - c)"),
        ("a / b * c", "((a / b) * c)"),
        ("a < b < c", "((a < b) < c)"),
        ("a OR b OR c", "((a OR b) OR c)"),
        ("a || b || c", "((a || b) || c)"),
    ]);
}

#[test]
fn between_bounds_bind_at_the_concat_level() {
    assert_shapes(&[
        ("x BETWEEN 1 AND 2 AND y", "((x BETWEEN 1 AND 2) AND y)"),
        (
            "x BETWEEN a + 1 AND b || c",
            "(x BETWEEN (a + 1) AND (b || c))",
        ),
        ("x NOT BETWEEN 1 AND 2", "(x NOT BETWEEN 1 AND 2)"),
        ("x BETWEEN 1 AND 2 = y", "((x BETWEEN 1 AND 2) = y)"),
    ]);
}

#[test]
fn prefix_operators_parentheses_and_compound_forms() {
    assert_shapes(&[
        ("- -a", "(-(-a))"),
        ("NOT NOT a", "(NOT (NOT a))"),
        ("(a + b) * c", "((a + b) * c)"),
        ("a = (NOT b)", "(a = (NOT b))"),
        ("x IS NOT NULL AND y", "((x IS NOT NULL) AND y)"),
        ("a <> b OR c != d", "((a <> b) OR (c <> d))"),
        ("a >= b AND c <= d", "((a >= b) AND (c <= d))"),
        ("x NOT IN (1, 2 + 3)", "(x NOT IN (1, (2 + 3)))"),
        (
            "x LIKE 'a%' OR y NOT LIKE 'b'",
            "((x LIKE 'a%') OR (y NOT LIKE 'b'))",
        ),
        (
            "CASE WHEN a = 1 THEN b + 1 ELSE -c END",
            "CASE WHEN (a = 1) THEN (b + 1) ELSE (-c) END",
        ),
        ("count(DISTINCT a + b)", "count(DISTINCT (a + b))"),
        ("t.a * f(b) + 1", "((t.a * f(b)) + 1)"),
    ]);
}
