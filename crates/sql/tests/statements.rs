//! DDL, DML and transaction statements (milestone AC1, AC3): trees, spans
//! and canonical text.

use cairn_sql::{DataType, Expr, Ident, Span, Statement, StatementKind, parse, parse_expr};

fn parse_all(src: &str) -> Vec<Statement> {
    parse(src).unwrap_or_else(|error| panic!("{src:?}:\n{error}"))
}

fn one(src: &str) -> StatementKind {
    let mut statements = parse_all(src);
    assert_eq!(statements.len(), 1, "{src}");
    statements.remove(0).node
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

fn assert_canonical(src: &str, canonical: &str) {
    assert_eq!(one(src).to_string(), canonical, "{src}");
}

#[test]
fn create_table_with_every_type_and_constraint() {
    let src = "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, score REAL, ok BOOLEAN)";
    let StatementKind::CreateTable(table) = one(src) else {
        panic!("expected CREATE TABLE");
    };
    assert!(!table.if_not_exists);
    assert_eq!(name(&table.name), "t");
    let columns: Vec<_> = table
        .columns
        .iter()
        .map(|column| {
            let column = &column.node;
            (
                name(&column.name),
                column.data_type.node,
                column.primary_key,
                column.not_null,
                column.unique,
            )
        })
        .collect();
    assert_eq!(
        columns,
        vec![
            ("id", DataType::Integer, true, false, false),
            ("name", DataType::Text, false, true, true),
            ("score", DataType::Real, false, false, false),
            ("ok", DataType::Boolean, false, false, false),
        ]
    );
    assert_canonical(src, src);
}

#[test]
fn create_table_if_not_exists_with_quoted_and_keyword_like_names() {
    let src = r#"create table if not exists "Users" (text integer, "Order" text)"#;
    let StatementKind::CreateTable(table) = one(src) else {
        panic!("expected CREATE TABLE");
    };
    assert!(table.if_not_exists);
    assert_eq!(name(&table.name), "Users");
    assert_canonical(
        src,
        r#"CREATE TABLE IF NOT EXISTS "Users" (text INTEGER, "Order" TEXT)"#,
    );
}

#[test]
fn column_constraints_may_appear_in_any_order() {
    let ordered = one("CREATE TABLE t (a INTEGER PRIMARY KEY NOT NULL UNIQUE)");
    let shuffled = one("CREATE TABLE t (a INTEGER UNIQUE NOT NULL PRIMARY KEY)");
    assert_eq!(ordered, shuffled);
    assert_eq!(
        shuffled.to_string(),
        "CREATE TABLE t (a INTEGER PRIMARY KEY NOT NULL UNIQUE)"
    );
}

#[test]
fn create_table_spans() {
    let statement = &parse_all("CREATE TABLE t (a INTEGER NOT NULL)")[0];
    assert_eq!(statement.span, span(0, 35, 1, 1));
    let StatementKind::CreateTable(table) = &statement.node else {
        panic!("expected CREATE TABLE");
    };
    let column = &table.columns[0];
    assert_eq!(column.span, span(16, 34, 1, 17));
    assert_eq!(column.node.data_type.span, span(18, 25, 1, 19));
    assert_eq!(table.name.span, span(13, 14, 1, 14));
}

#[test]
fn drop_table_with_and_without_if_exists() {
    let StatementKind::DropTable(plain) = one("DROP TABLE t") else {
        panic!("expected DROP TABLE");
    };
    assert!(!plain.if_exists);
    let StatementKind::DropTable(guarded) = one("drop table if exists t") else {
        panic!("expected DROP TABLE");
    };
    assert!(guarded.if_exists);
    assert_canonical("drop table if exists t", "DROP TABLE IF EXISTS t");
}

#[test]
fn create_index_unique_and_not() {
    let StatementKind::CreateIndex(index) = one("CREATE INDEX i ON t (a)") else {
        panic!("expected CREATE INDEX");
    };
    assert!(!index.unique);
    assert_eq!(
        (name(&index.name), name(&index.table), name(&index.column)),
        ("i", "t", "a")
    );
    let StatementKind::CreateIndex(unique) = one("create unique index i on t (a)") else {
        panic!("expected CREATE INDEX");
    };
    assert!(unique.unique);
    assert_canonical(
        "create unique index i on t (a)",
        "CREATE UNIQUE INDEX i ON t (a)",
    );
}

#[test]
fn insert_with_and_without_columns_and_with_several_rows() {
    let StatementKind::Insert(insert) = one("INSERT INTO t VALUES (1, 'a')") else {
        panic!("expected INSERT");
    };
    assert_eq!(name(&insert.table), "t");
    assert!(insert.columns.is_none());
    assert_eq!(insert.rows.len(), 1);
    assert_eq!(insert.rows[0].node.0, vec![expr("1"), expr("'a'")]);
    assert_eq!(insert.rows[0].span, span(21, 29, 1, 22));

    let src = "INSERT INTO t (a, b) VALUES (1, 2), (3, -4)";
    let StatementKind::Insert(insert) = one(src) else {
        panic!("expected INSERT");
    };
    let columns: Vec<&str> = insert.columns.iter().flatten().map(name).collect();
    assert_eq!(columns, vec!["a", "b"]);
    assert_eq!(insert.rows.len(), 2);
    assert_eq!(insert.rows[1].node.0, vec![expr("3"), expr("-4")]);
    assert_canonical(src, src);
}

#[test]
fn update_with_and_without_where() {
    let StatementKind::Update(update) = one("UPDATE t SET a = 1, b = b + 1") else {
        panic!("expected UPDATE");
    };
    assert_eq!(name(&update.table), "t");
    let assignments: Vec<_> = update
        .assignments
        .iter()
        .map(|assignment| (name(&assignment.node.column), assignment.node.value.clone()))
        .collect();
    assert_eq!(assignments, vec![("a", expr("1")), ("b", expr("b + 1"))]);
    assert_eq!(update.assignments[1].span, span(20, 29, 1, 21));
    assert!(update.where_clause.is_none());

    let StatementKind::Update(update) = one("UPDATE t SET a = NULL WHERE b <> 2") else {
        panic!("expected UPDATE");
    };
    assert_eq!(update.where_clause, Some(expr("b <> 2")));
    assert_canonical(
        "update t set a=null where b!=2",
        "UPDATE t SET a = NULL WHERE b <> 2",
    );
}

#[test]
fn delete_with_and_without_where() {
    let StatementKind::Delete(delete) = one("DELETE FROM t") else {
        panic!("expected DELETE");
    };
    assert_eq!(name(&delete.table), "t");
    assert!(delete.where_clause.is_none());
    let StatementKind::Delete(delete) = one("DELETE FROM t WHERE id IN (1, 2)") else {
        panic!("expected DELETE");
    };
    assert_eq!(delete.where_clause, Some(expr("id IN (1, 2)")));
}

#[test]
fn transaction_statements() {
    assert_eq!(one("BEGIN"), StatementKind::Begin);
    assert_eq!(one("commit"), StatementKind::Commit);
    assert_eq!(one("Rollback"), StatementKind::Rollback);
    assert_eq!(StatementKind::Begin.to_string(), "BEGIN");
}

#[test]
fn statements_are_separated_by_semicolons() {
    let statements = parse_all("BEGIN;\nINSERT INTO t VALUES (1);\nCOMMIT;");
    let kinds: Vec<String> = statements.iter().map(ToString::to_string).collect();
    assert_eq!(kinds, vec!["BEGIN", "INSERT INTO t VALUES (1)", "COMMIT"]);
    assert_eq!(statements[0].span, span(0, 5, 1, 1));
    assert_eq!(statements[1].span, span(7, 31, 2, 1));
    assert_eq!(statements[2].span, span(33, 39, 3, 1));
    assert_eq!(parse_all("SELECT 1;").len(), 1);
    assert_eq!(parse_all("SELECT 1 ; SELECT 2").len(), 2);
}
