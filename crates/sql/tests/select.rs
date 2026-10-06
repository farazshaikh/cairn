//! SELECT (milestone AC4): every clause alone and together, trees, spans
//! and canonical text.

use cairn_sql::{
    Expr, Ident, JoinKind, Select, SelectItem, Span, StatementKind, parse, parse_expr,
};

fn select(src: &str) -> Select {
    let mut statements = parse(src).unwrap_or_else(|error| panic!("{src:?}:\n{error}"));
    assert_eq!(statements.len(), 1, "{src}");
    let StatementKind::Select(select) = statements.remove(0).node else {
        panic!("{src} is not a SELECT");
    };
    *select
}

fn expr(src: &str) -> Expr {
    parse_expr(src).unwrap_or_else(|error| panic!("{src:?}:\n{error}"))
}

fn name(ident: &Ident) -> &str {
    &ident.node.0
}

fn span(start: usize, end: usize, line: usize, column: usize) -> Span {
    Span {
        start,
        end,
        line,
        column,
    }
}

#[test]
fn select_without_from_for_constant_expressions() {
    let query = select("SELECT 1 + 2, 'x' AS y");
    assert!(!query.distinct);
    assert!(query.from.is_none());
    assert_eq!(
        query.items[0].node,
        SelectItem::Expr {
            expr: expr("1 + 2"),
            alias: None
        }
    );
    let SelectItem::Expr {
        alias: Some(alias), ..
    } = &query.items[1].node
    else {
        panic!("expected an aliased item");
    };
    assert_eq!(name(alias), "y");
    assert_eq!(query.items[1].span, span(14, 22, 1, 15));
}

#[test]
fn distinct_wildcards_and_aliases() {
    let query = select("SELECT DISTINCT *, t.*, a AS b FROM t");
    assert!(query.distinct);
    assert_eq!(query.items[0].node, SelectItem::Wildcard);
    let SelectItem::QualifiedWildcard(table) = &query.items[1].node else {
        panic!("expected t.*");
    };
    assert_eq!(name(table), "t");
    assert_eq!(query.items[1].span, span(19, 22, 1, 20));
    assert_eq!(query.to_string(), "SELECT DISTINCT *, t.*, a AS b FROM t");
}

#[test]
fn from_with_table_alias() {
    let query = select("SELECT s.a FROM t AS s");
    let from = query.from.expect("FROM clause");
    assert_eq!(from.span, span(11, 22, 1, 12));
    let from = from.node;
    assert_eq!(name(&from.table.node.name), "t");
    assert_eq!(from.table.node.alias.as_ref().map(name), Some("s"));
    assert_eq!(from.table.span, span(16, 22, 1, 17));
    assert!(from.joins.is_empty());
}

#[test]
fn joins_inner_and_left_chained() {
    let src =
        "SELECT * FROM a JOIN b ON a.id = b.id INNER JOIN c AS d ON TRUE LEFT JOIN e ON e.k = d.k";
    let from = select(src).from.expect("FROM clause");
    assert_eq!(from.span, span(9, 88, 1, 10));
    let from = from.node;
    let joins: Vec<_> = from
        .joins
        .iter()
        .map(|join| (join.node.kind, name(&join.node.table.node.name)))
        .collect();
    assert_eq!(
        joins,
        vec![
            (JoinKind::Inner, "b"),
            (JoinKind::Inner, "c"),
            (JoinKind::Left, "e")
        ]
    );
    assert_eq!(from.joins[0].node.on, expr("a.id = b.id"));
    assert_eq!(
        from.joins[1].node.table.node.alias.as_ref().map(name),
        Some("d")
    );
    assert_eq!(from.joins[0].span, span(16, 37, 1, 17));
    assert_eq!(
        select(src).to_string(),
        "SELECT * FROM a JOIN b ON a.id = b.id JOIN c AS d ON TRUE LEFT JOIN e ON e.k = d.k"
    );
}

#[test]
fn join_and_inner_join_are_the_same_tree() {
    assert_eq!(
        select("SELECT * FROM a JOIN b ON TRUE"),
        select("SELECT * FROM a INNER JOIN b ON TRUE")
    );
}

#[test]
fn where_group_by_and_having() {
    let query = select("SELECT a, count(*) FROM t WHERE a > 1 GROUP BY a, b HAVING count(*) > 2");
    assert_eq!(query.where_clause, Some(expr("a > 1")));
    assert_eq!(query.group_by, vec![expr("a"), expr("b")]);
    assert_eq!(query.having, Some(expr("count(*) > 2")));
    assert!(query.order_by.is_empty());
    assert!(query.limit.is_none());
}

#[test]
fn every_clause_is_optional_and_parses_alone() {
    let cases = [
        "SELECT a FROM t WHERE a",
        "SELECT a FROM t GROUP BY a",
        "SELECT a FROM t HAVING a",
        "SELECT a FROM t ORDER BY a",
        "SELECT a FROM t LIMIT 1",
        "SELECT 1 WHERE TRUE",
    ];
    for src in cases {
        assert_eq!(select(src).to_string(), src);
    }
}

#[test]
fn order_by_directions() {
    let query = select("SELECT a FROM t ORDER BY a, b ASC, c DESC");
    let directions: Vec<bool> = query
        .order_by
        .iter()
        .map(|item| item.node.descending)
        .collect();
    assert_eq!(directions, vec![false, false, true]);
    assert_eq!(query.order_by[2].span, span(35, 41, 1, 36));
    assert_eq!(
        select("SELECT a FROM t ORDER BY a"),
        select("SELECT a FROM t ORDER BY a ASC")
    );
    assert_ne!(
        select("SELECT a FROM t ORDER BY a"),
        select("SELECT a FROM t ORDER BY a DESC")
    );
    assert_eq!(query.to_string(), "SELECT a FROM t ORDER BY a, b, c DESC");
}

#[test]
fn limit_with_and_without_offset() {
    let limit = select("SELECT a FROM t LIMIT 10").limit.expect("LIMIT");
    assert_eq!(limit.span, span(16, 24, 1, 17));
    let limit = limit.node;
    assert_eq!(limit.count.node, 10);
    assert_eq!(limit.count.span, span(22, 24, 1, 23));
    assert!(limit.offset.is_none());
    let limit = select("SELECT a FROM t LIMIT 10 OFFSET 5")
        .limit
        .expect("LIMIT");
    assert_eq!(limit.span, span(16, 33, 1, 17));
    assert_eq!(limit.node.offset.map(|offset| offset.node), Some(5));
}

#[test]
fn clause_spans_include_their_keyword_and_track_lines() {
    let query = select("SELECT a\nFROM t\nLIMIT 1");
    assert_eq!(query.from.map(|from| from.span), Some(span(9, 15, 2, 1)));
    assert_eq!(
        query.limit.map(|limit| limit.span),
        Some(span(16, 23, 3, 1))
    );
}

#[test]
fn every_clause_together() {
    let src = "select distinct t.a, count(distinct u.b) as n from t as t left join u on u.id = t.id \
               where t.a is not null group by t.a having count(*) >= 2 order by n desc, t.a limit 5 offset 1";
    let query = select(src);
    assert!(query.distinct);
    assert_eq!(query.items.len(), 2);
    assert_eq!(
        query.from.as_ref().map(|from| from.node.joins.len()),
        Some(1)
    );
    assert_eq!(query.where_clause, Some(expr("t.a IS NOT NULL")));
    assert_eq!(query.group_by, vec![expr("t.a")]);
    assert_eq!(query.having, Some(expr("count(*) >= 2")));
    assert_eq!(query.order_by.len(), 2);
    assert_eq!(
        query.limit.as_ref().map(|limit| limit.node.count.node),
        Some(5)
    );
    assert_eq!(
        query.to_string(),
        "SELECT DISTINCT t.a, count(DISTINCT u.b) AS n FROM t AS t LEFT JOIN u ON u.id = t.id \
         WHERE t.a IS NOT NULL GROUP BY t.a HAVING count(*) >= 2 ORDER BY n DESC, t.a LIMIT 5 OFFSET 1"
    );
}
