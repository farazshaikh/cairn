//! Planner tests: the access path EXPLAIN reports for each predicate
//! shape, and equivalence of indexed plans with full scans over generated
//! predicates.

use std::fs;
use std::path::PathBuf;

use cairn_exec::{Database, QueryResult, Value};

struct TempDb(PathBuf, Database);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let path =
            std::env::temp_dir().join(format!("cairn-planner-{}-{name}.db", std::process::id()));
        let _ = fs::remove_file(&path);
        let db = Database::create(&path).expect("create");
        TempDb(path, db)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db
        .execute(sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .pop()
    {
        Some(QueryResult::Rows { rows, .. }) => rows,
        other => panic!("{sql}: {other:?}"),
    }
}

/// The (operation, detail) of the plan's access leaf, the last EXPLAIN row.
fn access(db: &mut Database, sql: &str) -> (String, String) {
    let plan = rows(db, &format!("EXPLAIN {sql}"));
    let text = |v: Option<&Value>| match v {
        Some(Value::Text(t)) => t.clone(),
        other => panic!("{other:?}"),
    };
    let last = plan.last().expect("plan rows");
    (text(last.get(2)), text(last.get(3)))
}

fn setup(db: &mut Database) {
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT, r REAL);
         CREATE INDEX t_a ON t (a);
         CREATE INDEX t_r ON t (r);
         CREATE TABLE u (k INTEGER, v TEXT);
         CREATE INDEX u_k ON u (k);
         INSERT INTO t VALUES (1, 1, 'x', 0.5), (2, 2, 'y', 1.5), (3, NULL, 'z', NULL);
         INSERT INTO u VALUES (1, 'one'), (2, 'two');",
    )
    .expect("setup");
}

#[test]
fn explain_reports_the_chosen_access_path() {
    let mut tmp = TempDb::new("explain");
    let db = &mut tmp.1;
    setup(db);
    let cases = [
        (
            "SELECT * FROM t WHERE id = 5",
            "PRIMARY KEY LOOKUP",
            "t (id = 5)",
        ),
        (
            "SELECT * FROM t WHERE 5 = id",
            "PRIMARY KEY LOOKUP",
            "t (id = 5)",
        ),
        (
            "SELECT * FROM t WHERE id > 3 AND id <= 9",
            "PRIMARY KEY RANGE",
            "t (id > 3 AND id <= 9)",
        ),
        (
            "SELECT * FROM t WHERE id BETWEEN 2 AND 4",
            "PRIMARY KEY RANGE",
            "t (id >= 2 AND id <= 4)",
        ),
        (
            "SELECT * FROM t WHERE id > 1 AND id >= 1 AND 7 > id",
            "PRIMARY KEY RANGE",
            "t (id > 1 AND id < 7)",
        ),
        (
            "SELECT * FROM t WHERE a = 2",
            "INDEX LOOKUP",
            "t USING t_a (a = 2)",
        ),
        (
            "SELECT * FROM t WHERE a < 10",
            "INDEX RANGE",
            "t USING t_a (a < 10)",
        ),
        (
            "SELECT * FROM t WHERE a = NULL",
            "INDEX LOOKUP",
            "t USING t_a (a = NULL)",
        ),
        (
            "SELECT * FROM t WHERE r >= 1",
            "INDEX RANGE",
            "t USING t_r (r >= 1.0)",
        ),
        (
            "SELECT * FROM t WHERE a = 2 AND id = 1",
            "PRIMARY KEY LOOKUP",
            "t (id = 1)",
        ),
        (
            "SELECT * FROM t WHERE a = 1 + 1",
            "INDEX LOOKUP",
            "t USING t_a (a = 2)",
        ),
        (
            "SELECT * FROM t AS x WHERE x.a = 2",
            "INDEX LOOKUP",
            "t AS x USING t_a (a = 2)",
        ),
        ("SELECT * FROM t WHERE b = 'x'", "SCAN", "t"),
        ("SELECT * FROM t WHERE a + 1 = 2", "SCAN", "t"),
        ("SELECT * FROM t WHERE a = 1 OR a = 2", "SCAN", "t"),
        ("SELECT * FROM t WHERE a = id", "SCAN", "t"),
        ("SELECT * FROM t WHERE a <> 2", "SCAN", "t"),
        ("SELECT * FROM t WHERE a NOT BETWEEN 1 AND 2", "SCAN", "t"),
        ("SELECT * FROM t WHERE a = 1.5", "SCAN", "t"),
        ("SELECT * FROM t", "SCAN", "t"),
    ];
    for (sql, operation, detail) in cases {
        assert_eq!(
            access(db, sql),
            (operation.to_string(), detail.to_string()),
            "{sql}"
        );
    }
    assert!(rows(db, "SELECT * FROM t WHERE a = NULL").is_empty());
    assert!(rows(db, "SELECT * FROM t WHERE a > NULL").is_empty());
}

#[test]
fn joins_use_an_index_on_the_inner_join_column() {
    let mut tmp = TempDb::new("join");
    let db = &mut tmp.1;
    setup(db);
    let plan = rows(db, "EXPLAIN SELECT t.id, u.v FROM t JOIN u ON u.k = t.a");
    let operations: Vec<Value> = plan.iter().filter_map(|r| r.get(2).cloned()).collect();
    assert_eq!(
        operations,
        ["PROJECT", "INDEX JOIN", "SCAN"]
            .map(|s| Value::Text(s.into()))
            .to_vec()
    );
    assert_eq!(
        plan.get(1).and_then(|r| r.get(3)).cloned(),
        Some(Value::Text("INNER u USING INDEX u_k (k = t.a)".into()))
    );
    let plan = rows(db, "EXPLAIN SELECT * FROM u LEFT JOIN t ON t.id = u.k");
    assert_eq!(
        plan.get(1).and_then(|r| r.get(3)).cloned(),
        Some(Value::Text("LEFT t USING PRIMARY KEY (id = u.k)".into()))
    );
    let plan = rows(db, "EXPLAIN SELECT * FROM u JOIN t ON t.b = u.v");
    assert_eq!(
        plan.get(1).and_then(|r| r.get(2)).cloned(),
        Some(Value::Text("NESTED LOOP JOIN".into()))
    );
    let indexed = rows(
        db,
        "SELECT t.id, u.v FROM t LEFT JOIN u ON u.k = t.a ORDER BY t.id",
    );
    let scanned = rows(
        db,
        "SELECT t.id, u.v FROM t LEFT JOIN u ON u.k + 0 = t.a ORDER BY t.id",
    );
    assert_eq!(indexed, scanned);
    assert_eq!(indexed.len(), 3);
}

#[test]
fn update_and_delete_use_the_planner_and_touch_only_matches() {
    let mut tmp = TempDb::new("dml");
    let db = &mut tmp.1;
    setup(db);
    assert_eq!(
        db.execute("UPDATE t SET b = 'w' WHERE a = 2")
            .expect("update"),
        vec![QueryResult::Affected(1)]
    );
    assert_eq!(
        db.execute("DELETE FROM t WHERE id > 1").expect("delete"),
        vec![QueryResult::Affected(2)]
    );
    assert_eq!(
        rows(db, "SELECT id, b FROM t"),
        vec![vec![Value::Integer(1), Value::Text("x".into())]]
    );
    db.check().expect("check");
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn pick<'a>(&mut self, items: &'a [&'a str]) -> &'a str {
        items[(self.next() % items.len() as u64) as usize]
    }
}

#[test]
fn indexed_plans_return_the_same_rows_as_full_scans() {
    let mut tmp = TempDb::new("equivalence");
    let db = &mut tmp.1;
    db.execute(
        "CREATE TABLE ti (id INTEGER PRIMARY KEY, a INTEGER, r REAL, s TEXT, f BOOLEAN);
         CREATE INDEX ti_a ON ti (a); CREATE INDEX ti_r ON ti (r);
         CREATE INDEX ti_s ON ti (s); CREATE INDEX ti_f ON ti (f);
         CREATE TABLE tp (id INTEGER, a INTEGER, r REAL, s TEXT, f BOOLEAN);",
    )
    .expect("schema");
    let mut rng = XorShift(0x2545_F491_4F6C_DD1D);
    let ints = [
        "NULL",
        "-9223372036854775807 - 1",
        "-5",
        "-1",
        "0",
        "1",
        "2",
        "3",
        "7",
        "9223372036854775807",
    ];
    let reals = ["NULL", "-2.5", "-0.0", "0.0", "0.5", "1.0", "2.0", "1e300"];
    let texts = ["NULL", "''", "'a'", "'ab'", "'b'", "'é'", "'zz'"];
    let bools = ["NULL", "TRUE", "FALSE"];
    let mut ids = std::collections::BTreeSet::new();
    for i in 0..150 {
        let id = match i {
            0 => "-9223372036854775807 - 1".to_string(),
            1 => "9223372036854775807".to_string(),
            _ => ((rng.next() % 40) as i64 - 20).to_string(),
        };
        if !ids.insert(id.clone()) {
            continue;
        }
        let row = format!(
            "({id}, {}, {}, {}, {})",
            rng.pick(&ints),
            rng.pick(&reals),
            rng.pick(&texts),
            rng.pick(&bools)
        );
        db.execute(&format!(
            "INSERT INTO ti VALUES {row}; INSERT INTO tp VALUES {row}"
        ))
        .expect("insert");
    }
    let columns: [(&str, &[&str]); 5] = [
        ("id", &ints),
        ("a", &ints),
        (
            "r",
            &[&reals[..], &["1", "-3", "9223372036854775807"]].concat(),
        ),
        ("s", &texts),
        ("f", &bools),
    ];
    let int_reals = ["0.5", "2.0", "-1.5"];
    let ops = ["=", "<", "<=", ">", ">=", "BETWEEN"];
    let mut indexed = 0;
    for n in 0..300 {
        let (column, values) = columns[(rng.next() % 5) as usize];
        let constant = |rng: &mut XorShift| {
            if (column == "id" || column == "a") && rng.next().is_multiple_of(6) {
                rng.pick(&int_reals).to_string()
            } else {
                values[(rng.next() % values.len() as u64) as usize].to_string()
            }
        };
        let op = rng.pick(&ops);
        let mut predicate = if op == "BETWEEN" {
            let (low, high) = (constant(&mut rng), constant(&mut rng));
            format!("{column} BETWEEN {low} AND {high}")
        } else if rng.next().is_multiple_of(3) {
            format!("{} {} {column}", constant(&mut rng), flip(op))
        } else {
            format!("{column} {op} {}", constant(&mut rng))
        };
        if n % 4 == 0 {
            let (other, other_values) = columns[(rng.next() % 5) as usize];
            let value = other_values[(rng.next() % other_values.len() as u64) as usize];
            predicate = format!("{predicate} AND {other} >= {value}");
        }
        let query = |table: &str| {
            format!("SELECT id, a, r, s, f FROM {table} WHERE {predicate} ORDER BY id")
        };
        let (operation, _) = access(db, &query("ti"));
        if operation != "SCAN" {
            indexed += 1;
        }
        assert_eq!(
            rows(db, &query("ti")),
            rows(db, &query("tp")),
            "{predicate}"
        );
    }
    assert!(
        indexed >= 150,
        "only {indexed} of 300 predicates used an index"
    );
}

fn flip(op: &str) -> &str {
    match op {
        "<" => ">",
        "<=" => ">=",
        ">" => "<",
        ">=" => "<=",
        other => other,
    }
}
