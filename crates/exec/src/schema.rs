//! A read-only description of the tables and indexes a handle can see, for
//! tools such as the `cairn` shell's `.tables` and `.schema`.
//!
//! Only indexes created with `CREATE INDEX` are listed. The implicit unique
//! indexes behind `PRIMARY KEY` and `UNIQUE` columns are left out, because
//! running the table's `CREATE TABLE` recreates them.

use cairn_sql::{ColumnDef, CreateIndex, CreateTable, DataType, Ident, Name, Span, Spanned};

use crate::catalog::Catalog;
use crate::database::Database;
use crate::error::ExecError;

/// One table: its columns in declaration order and its explicit indexes in
/// name order.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSchema {
    /// The table name.
    pub name: String,
    /// The columns in declaration order.
    pub columns: Vec<ColumnSchema>,
    /// Indexes made by `CREATE INDEX`, in name order.
    pub indexes: Vec<IndexSchema>,
}

/// One column as declared. `not_null` is the declared constraint, so a
/// PRIMARY KEY column does not report it unless it was written.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSchema {
    /// The column name.
    pub name: String,
    /// The declared type.
    pub data_type: DataType,
    /// Declared `PRIMARY KEY`.
    pub primary_key: bool,
    /// Declared `NOT NULL`.
    pub not_null: bool,
    /// Declared `UNIQUE`.
    pub unique: bool,
}

/// One index created with `CREATE [UNIQUE] INDEX`.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSchema {
    /// The index name.
    pub name: String,
    /// The indexed column.
    pub column: String,
    /// Made by `CREATE UNIQUE INDEX`.
    pub unique: bool,
}

impl Database {
    /// Every table in name order, as this handle sees the database now:
    /// committed state, plus its own open transaction.
    pub fn schema(&mut self) -> Result<Vec<TableSchema>, ExecError> {
        self.read(|db| describe(&db.catalog))
    }
}

fn describe(catalog: &Catalog) -> Result<Vec<TableSchema>, ExecError> {
    let mut tables = Vec::with_capacity(catalog.tables.len());
    for table in catalog.tables.values() {
        let mut columns = Vec::with_capacity(table.columns.len());
        for column in &table.columns {
            let data_type = column
                .ty
                .data_type()
                .ok_or_else(|| ExecError::corrupt("corrupt catalog: column type"))?;
            columns.push(ColumnSchema {
                name: column.name.clone(),
                data_type,
                primary_key: column.primary_key,
                not_null: column.not_null,
                unique: column.unique,
            });
        }
        let indexes = catalog
            .indexes_of(&table.name)
            .into_iter()
            .filter(|index| !index.implicit)
            .map(|index| IndexSchema {
                column: table.column_name(index.column).to_string(),
                name: index.name,
                unique: index.unique,
            })
            .collect();
        tables.push(TableSchema {
            name: table.name.clone(),
            columns,
            indexes,
        });
    }
    Ok(tables)
}

impl TableSchema {
    /// `CREATE TABLE` in cairn-sql's canonical form, without `;`. Names are
    /// quoted only when needed, so the text parses back to this table.
    pub fn create_table_sql(&self) -> String {
        let columns = self
            .columns
            .iter()
            .map(|column| {
                spanned(ColumnDef {
                    name: ident(&column.name),
                    data_type: spanned(column.data_type),
                    primary_key: column.primary_key,
                    not_null: column.not_null,
                    unique: column.unique,
                })
            })
            .collect();
        CreateTable {
            if_not_exists: false,
            name: ident(&self.name),
            columns,
        }
        .to_string()
    }

    /// One canonical `CREATE [UNIQUE] INDEX` per explicit index, without `;`.
    pub fn create_index_sql(&self) -> Vec<String> {
        self.indexes
            .iter()
            .map(|index| {
                CreateIndex {
                    unique: index.unique,
                    name: ident(&index.name),
                    table: ident(&self.name),
                    column: ident(&index.column),
                }
                .to_string()
            })
            .collect()
    }
}

/// Printing ignores spans, so generated nodes share one placeholder.
fn spanned<T>(node: T) -> Spanned<T> {
    Spanned::new(
        node,
        Span {
            start: 0,
            end: 0,
            line: 1,
            column: 1,
        },
    )
}

fn ident(name: &str) -> Ident {
    spanned(Name(name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_sql::StatementKind;

    fn table() -> TableSchema {
        TableSchema {
            name: "My T".to_string(),
            columns: vec![
                ColumnSchema {
                    name: "id".to_string(),
                    data_type: DataType::Integer,
                    primary_key: true,
                    not_null: false,
                    unique: false,
                },
                ColumnSchema {
                    name: "select".to_string(),
                    data_type: DataType::Text,
                    primary_key: false,
                    not_null: true,
                    unique: true,
                },
            ],
            indexes: vec![IndexSchema {
                name: "by_sel".to_string(),
                column: "select".to_string(),
                unique: false,
            }],
        }
    }

    #[test]
    fn generated_sql_quotes_names_only_when_needed() {
        let table = table();
        assert_eq!(
            table.create_table_sql(),
            "CREATE TABLE \"My T\" (id INTEGER PRIMARY KEY, \"select\" TEXT NOT NULL UNIQUE)"
        );
        assert_eq!(
            table.create_index_sql(),
            vec!["CREATE INDEX by_sel ON \"My T\" (\"select\")".to_string()]
        );
    }

    #[test]
    fn generated_sql_parses_back_to_the_same_definition() {
        let table = table();
        let parsed = cairn_sql::parse(&table.create_table_sql()).expect("parse table");
        let [statement] = parsed.as_slice() else {
            panic!("one statement expected");
        };
        let StatementKind::CreateTable(create) = &statement.node else {
            panic!("CREATE TABLE expected");
        };
        assert_eq!(create.name.node.0, "My T");
        assert_eq!(create.columns.len(), 2);
        assert_eq!(create.to_string(), table.create_table_sql());
        let index_sql = &table.create_index_sql()[0];
        let parsed = cairn_sql::parse(index_sql).expect("parse index");
        assert_eq!(parsed[0].node.to_string(), *index_sql);
    }
}
