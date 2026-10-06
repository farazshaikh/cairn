//! Expression forms, spans and the nesting limit (milestone AC5, AC7).

use cairn_sql::{
    BinaryOp, Expr, ExprKind, FunctionArgs, Literal, MAX_DEPTH, Span, UnaryOp, parse_expr,
};

fn expr(src: &str) -> Expr {
    parse_expr(src).unwrap_or_else(|error| panic!("{src:?}:\n{error}"))
}

fn depth_error(src: &str) -> cairn_sql::SqlError {
    let error = parse_expr(src).expect_err("nesting over the limit");
    assert_eq!(error.message, "expression nesting exceeds the limit of 200");
    error
}

fn column_name(expr: &Expr) -> (Option<&str>, &str) {
    let ExprKind::Column { table, name } = &expr.node else {
        panic!("not a column: {expr:?}");
    };
    (
        table.as_ref().map(|table| table.node.0.as_str()),
        name.node.0.as_str(),
    )
}

#[test]
fn literals() {
    let cases = [
        ("42", Literal::Integer(42)),
        ("2.5", Literal::Real(2.5)),
        ("'it''s'", Literal::String("it's".to_string())),
        ("NULL", Literal::Null),
        ("TRUE", Literal::Boolean(true)),
        ("false", Literal::Boolean(false)),
    ];
    for (src, literal) in cases {
        assert_eq!(expr(src).node, ExprKind::Literal(literal), "{src}");
    }
}

#[test]
fn column_references_may_be_qualified() {
    assert_eq!(column_name(&expr("Col")), (None, "col"));
    assert_eq!(column_name(&expr("T.Col")), (Some("t"), "col"));
    assert_eq!(column_name(&expr(r#""T"."Col""#)), (Some("T"), "Col"));
}

#[test]
fn unary_and_binary_operators() {
    let ExprKind::Unary {
        op: UnaryOp::Neg,
        operand,
    } = expr("-a").node
    else {
        panic!("expected negation");
    };
    assert_eq!(column_name(&operand), (None, "a"));
    let ExprKind::Unary {
        op: UnaryOp::Not, ..
    } = expr("NOT a").node
    else {
        panic!("expected NOT");
    };
    let operators = [
        ("a OR b", BinaryOp::Or),
        ("a AND b", BinaryOp::And),
        ("a = b", BinaryOp::Eq),
        ("a <> b", BinaryOp::NotEq),
        ("a != b", BinaryOp::NotEq),
        ("a < b", BinaryOp::Lt),
        ("a <= b", BinaryOp::LtEq),
        ("a > b", BinaryOp::Gt),
        ("a >= b", BinaryOp::GtEq),
        ("a || b", BinaryOp::Concat),
        ("a + b", BinaryOp::Add),
        ("a - b", BinaryOp::Sub),
        ("a * b", BinaryOp::Mul),
        ("a / b", BinaryOp::Div),
        ("a % b", BinaryOp::Mod),
    ];
    for (src, expected) in operators {
        let ExprKind::Binary { op, .. } = expr(src).node else {
            panic!("{src} is not binary");
        };
        assert_eq!(op, expected, "{src}");
    }
}

#[test]
fn predicates_and_their_negated_forms() {
    assert!(matches!(
        expr("x IS NULL").node,
        ExprKind::IsNull { negated: false, .. }
    ));
    assert!(matches!(
        expr("x IS NOT NULL").node,
        ExprKind::IsNull { negated: true, .. }
    ));
    assert!(matches!(
        expr("x BETWEEN a AND b").node,
        ExprKind::Between { negated: false, .. }
    ));
    assert!(matches!(
        expr("x NOT BETWEEN a AND b").node,
        ExprKind::Between { negated: true, .. }
    ));
    assert!(matches!(
        expr("x LIKE 'a%'").node,
        ExprKind::Like { negated: false, .. }
    ));
    assert!(matches!(
        expr("x NOT LIKE 'a%'").node,
        ExprKind::Like { negated: true, .. }
    ));
    let ExprKind::InList { list, negated, .. } = expr("x NOT IN (1, 2)").node else {
        panic!("expected IN list");
    };
    assert!(negated);
    assert_eq!(list, vec![expr("1"), expr("2")]);
    assert!(matches!(
        expr("x IN (1)").node,
        ExprKind::InList { negated: false, .. }
    ));
}

#[test]
fn case_with_and_without_else() {
    let ExprKind::Case {
        branches,
        else_result,
    } = expr("CASE WHEN a THEN 1 WHEN b THEN 2 ELSE 3 END").node
    else {
        panic!("expected CASE");
    };
    assert_eq!(branches.len(), 2);
    assert_eq!(branches[1].condition, expr("b"));
    assert_eq!(branches[1].result, expr("2"));
    assert_eq!(else_result.map(|result| *result), Some(expr("3")));
    let ExprKind::Case { else_result, .. } = expr("CASE WHEN a THEN 1 END").node else {
        panic!("expected CASE");
    };
    assert!(else_result.is_none());
}

#[test]
fn function_calls() {
    let args = |src: &str| {
        let ExprKind::Function { name, args } = expr(src).node else {
            panic!("{src} is not a call");
        };
        (name.node.0, args)
    };
    assert_eq!(
        args("f()"),
        (
            "f".to_string(),
            FunctionArgs::List {
                distinct: false,
                args: vec![]
            }
        )
    );
    assert_eq!(
        args("F(a, b + 1)"),
        (
            "f".to_string(),
            FunctionArgs::List {
                distinct: false,
                args: vec![expr("a"), expr("b + 1")]
            }
        )
    );
    assert_eq!(args("COUNT(*)"), ("count".to_string(), FunctionArgs::Star));
    assert_eq!(
        args("count(DISTINCT x)"),
        (
            "count".to_string(),
            FunctionArgs::List {
                distinct: true,
                args: vec![expr("x")]
            }
        )
    );
}

#[test]
fn parentheses_are_not_nodes() {
    assert_eq!(expr("((a + b))"), expr("a + b"));
}

#[test]
fn expression_spans_cover_their_source_text() {
    let span = |start, end, line, column| Span {
        start,
        end,
        line,
        column,
    };
    let comparison = expr("x = a + b");
    assert_eq!(comparison.span, span(0, 9, 1, 1));
    let ExprKind::Binary { right, .. } = &comparison.node else {
        panic!("expected comparison");
    };
    assert_eq!(right.span, span(4, 9, 1, 5));

    let sum = expr("1 + f(a, b)");
    let ExprKind::Binary { right: call, .. } = &sum.node else {
        panic!("expected sum");
    };
    assert_eq!(call.span, span(4, 11, 1, 5));

    assert_eq!(expr("\nCASE WHEN a THEN 1 END").span, span(1, 23, 2, 1));
    assert_eq!(expr(" (a) ").span, span(1, 4, 1, 2));
    assert_eq!(expr("x IS NOT NULL").span, span(0, 13, 1, 1));
    assert_eq!(expr("x IN (1, 2)").span, span(0, 11, 1, 1));
    assert_eq!(expr("t.c").span, span(0, 3, 1, 1));
}

#[test]
fn parse_expr_rejects_trailing_tokens() {
    let error = parse_expr("a b").expect_err("trailing token");
    assert_eq!(
        error.message,
        "expected end of input after expression, found 'b'"
    );
}

#[test]
fn every_kind_of_nesting_is_accepted_at_the_limit() {
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
        format!("{}1{}", "x IN (".repeat(n), ")".repeat(n)),
    ];
    for input in inputs {
        parse_expr(&input).unwrap_or_else(|error| panic!("{error}"));
    }
}

#[test]
fn every_kind_of_nesting_fails_one_past_the_limit() {
    let n = MAX_DEPTH + 1;
    let cases = [
        (format!("{}1{}", "(".repeat(n), ")".repeat(n)), 200),
        (format!("1{}", " + 1".repeat(n)), 802),
        (format!("{}a", "NOT ".repeat(n)), 800),
        (format!("{}1", "- ".repeat(n)), 400),
        (
            format!(
                "{}1{}",
                "CASE WHEN TRUE THEN ".repeat(n),
                " ELSE 0 END".repeat(n)
            ),
            4000,
        ),
        (format!("{}1{}", "f(".repeat(n), ")".repeat(n)), 401),
        (format!("{}1{}", "x IN (".repeat(n), ")".repeat(n)), 1205),
    ];
    for (input, start) in cases {
        assert_eq!(depth_error(&input).span.start, start, "{}", &input[..20]);
    }
}

#[test]
fn very_deep_input_is_an_error_not_a_stack_overflow() {
    let deep = 100_000;
    depth_error(&format!("{}1{}", "(".repeat(deep), ")".repeat(deep)));
    depth_error(&"(".repeat(deep));
    depth_error(&format!("{}1", "- ".repeat(deep)));
    depth_error(&format!("1{}", " + 1".repeat(deep)));
    depth_error(&format!("1{}", " OR 1".repeat(deep)));
    depth_error(&format!("{}1", "f(".repeat(deep)));
}

/// Inputs that maximise recursion per nesting level, measured as the
/// deepest stack users in a debug build. All run on the default 2 MiB
/// test thread: the accepted ones must parse, the others must fail cleanly.
#[test]
fn worst_case_recursion_fits_the_default_test_stack() {
    let half = MAX_DEPTH / 2;
    let accepted = [
        format!("{}1{}", "f(".repeat(MAX_DEPTH), ")".repeat(MAX_DEPTH)),
        format!("{}1{}", "1 + (".repeat(MAX_DEPTH), ")".repeat(MAX_DEPTH)),
        format!("{}1{}", "f(1 + (".repeat(half), "))".repeat(half)),
        format!(
            "{}1{}",
            "CASE WHEN TRUE THEN 1 + (".repeat(half),
            ") END".repeat(half)
        ),
    ];
    for input in accepted {
        parse_expr(&input).unwrap_or_else(|error| panic!("{error}"));
    }
    let rejected = [
        format!(
            "{}1",
            "1 OR 1 AND 1 = 1 BETWEEN 1 || 1 + 1 * -(".repeat(MAX_DEPTH)
        ),
        format!("{}1", "1 OR 1 AND 1 = 1 || 1 + 1 * (".repeat(MAX_DEPTH)),
        format!("{}1", "f(1 + (".repeat(MAX_DEPTH)),
    ];
    for input in rejected {
        depth_error(&input);
    }
}
