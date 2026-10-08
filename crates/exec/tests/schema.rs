//! `Database::schema`: the read-only table and index listing behind the
//! shell's `.tables` and `.schema`.

use cairn_exec::{ColumnSchema, Database, IndexSchema};
use cairn_sql::DataType;

mod common;

use common::remove_database;

struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let path =
            std::env::temp_dir().join(format!("cairn-schema-{}-{name}.db", std::process::id()));
        remove_database(&path);
        TempDb(path)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        remove_database(&self.0);
    }
}

fn names(db: &mut Database) -> Vec<String> {
    db.schema()
        .expect("schema")
        .into_iter()
        .map(|t| t.name)
        .collect()
}

#[test]
fn an_empty_database_has_no_tables() {
    let tmp = TempDb::new("empty");
    let mut db = Database::create(&tmp.0).expect("create");
    assert!(db.schema().expect("schema").is_empty());
    db.close().expect("close");
}

#[test]
fn tables_come_in_name_order_with_exact_columns() {
    let tmp = TempDb::new("order");
    let mut db = Database::create(&tmp.0).expect("create");
    db.execute(
        "CREATE TABLE zeta (a REAL);
         CREATE TABLE alpha (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, ok BOOLEAN);
         CREATE TABLE mid (x TEXT);",
    )
    .expect("ddl");
    assert_eq!(names(&mut db), ["alpha", "mid", "zeta"]);
    let alpha = db.schema().expect("schema").remove(0);
    assert_eq!(
        alpha.columns,
        vec![
            ColumnSchema {
                name: "id".into(),
                data_type: DataType::Integer,
                primary_key: true,
                not_null: false,
                unique: false,
            },
            ColumnSchema {
                name: "name".into(),
                data_type: DataType::Text,
                primary_key: false,
                not_null: true,
                unique: true,
            },
            ColumnSchema {
                name: "ok".into(),
                data_type: DataType::Boolean,
                primary_key: false,
                not_null: false,
                unique: false,
            },
        ]
    );
    assert!(alpha.indexes.is_empty(), "implicit indexes are not listed");
    db.execute("DROP TABLE mid;").expect("drop");
    assert_eq!(names(&mut db), ["alpha", "zeta"]);
    db.close().expect("close");
}

#[test]
fn only_explicit_indexes_are_listed() {
    let tmp = TempDb::new("indexes");
    let mut db = Database::create(&tmp.0).expect("create");
    db.execute(
        "CREATE TABLE t (a INTEGER UNIQUE, b TEXT);
         CREATE UNIQUE INDEX t_b ON t (b);
         CREATE INDEX t_a ON t (a);",
    )
    .expect("ddl");
    let table = db.schema().expect("schema").remove(0);
    assert_eq!(
        table.indexes,
        vec![
            IndexSchema {
                name: "t_a".into(),
                column: "a".into(),
                unique: false,
            },
            IndexSchema {
                name: "t_b".into(),
                column: "b".into(),
                unique: true,
            },
        ]
    );
    assert_eq!(
        table.create_index_sql(),
        [
            "CREATE INDEX t_a ON t (a)",
            "CREATE UNIQUE INDEX t_b ON t (b)"
        ]
    );
    db.close().expect("close");
}

#[test]
fn a_transaction_sees_its_own_tables_and_other_handles_do_not() {
    let tmp = TempDb::new("txn");
    let mut writer = Database::create(&tmp.0).expect("create");
    let mut reader = Database::open(&tmp.0).expect("open");
    writer
        .execute("BEGIN; CREATE TABLE fresh (a INTEGER);")
        .expect("begin");
    assert_eq!(names(&mut writer), ["fresh"]);
    assert!(names(&mut reader).is_empty());
    writer.execute("COMMIT;").expect("commit");
    assert_eq!(names(&mut reader), ["fresh"]);
    reader.close().expect("close reader");
    writer.close().expect("close writer");
}

#[test]
fn quoted_names_round_trip_through_create_table_sql() {
    let tmp = TempDb::new("quoted");
    let mut db = Database::create(&tmp.0).expect("create");
    db.execute("CREATE TABLE \"My T\" (\"Col\" TEXT, \"select\" INTEGER NOT NULL);")
        .expect("ddl");
    let table = db.schema().expect("schema").remove(0);
    let sql = table.create_table_sql();
    assert_eq!(
        sql,
        "CREATE TABLE \"My T\" (\"Col\" TEXT, \"select\" INTEGER NOT NULL)"
    );
    db.execute(&format!("DROP TABLE \"My T\"; {sql};"))
        .expect("replay");
    assert_eq!(db.schema().expect("schema").remove(0), table);
    db.close().expect("close");
}
