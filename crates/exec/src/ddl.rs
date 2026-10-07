//! CREATE TABLE, DROP TABLE and CREATE [UNIQUE] INDEX.
//!
//! Each statement validates fully before its first write. Implicit unique
//! indexes named `cairn_autoindex_<table>_<column>` enforce UNIQUE columns
//! and non-INTEGER PRIMARY KEY columns; user index names may not start
//! with `cairn_`.

use std::collections::BTreeSet;

use cairn_sql::{CreateIndex, CreateTable, DropTable, Ident};
use cairn_storage::{BTree, MAX_KEY_LEN};

use crate::catalog::{ColumnDef, IMPLICIT_PREFIX, IndexDef, MAX_COLUMNS, TableDef, valid_name};
use crate::codec::key::index_key;
use crate::database::{Database, QueryResult};
use crate::error::{ErrorKind, ExecError};
use crate::table::{destroy_tree, scan};
use crate::value::{OrdRow, SqlType};

fn check_name(ident: &Ident) -> Result<(), ExecError> {
    if valid_name(&ident.node.0) {
        return Ok(());
    }
    Err(ExecError::at(
        ErrorKind::Type,
        format!(
            "invalid name: {} (names are 1 to 64 bytes without NUL)",
            ident.node.0
        ),
        ident.span,
    ))
}

impl Database {
    pub(crate) fn create_table(&mut self, create: &CreateTable) -> Result<QueryResult, ExecError> {
        let name = &create.name;
        check_name(name)?;
        if self.catalog.tables.contains_key(&name.node.0) {
            if create.if_not_exists {
                return Ok(QueryResult::Affected(0));
            }
            return Err(ExecError::at(
                ErrorKind::AlreadyExists,
                format!("table {} already exists", name.node.0),
                name.span,
            ));
        }
        let mut columns: Vec<ColumnDef> = Vec::with_capacity(create.columns.len());
        for def in &create.columns {
            let column = &def.node;
            check_name(&column.name)?;
            if columns.iter().any(|c| c.name == column.name.node.0) {
                return Err(ExecError::at(
                    ErrorKind::AlreadyExists,
                    format!("duplicate column name: {}", column.name.node.0),
                    def.span,
                ));
            }
            if column.primary_key && columns.iter().any(|c| c.primary_key) {
                return Err(ExecError::at(
                    ErrorKind::Type,
                    format!("table {} has more than one primary key", name.node.0),
                    def.span,
                ));
            }
            columns.push(ColumnDef {
                name: column.name.node.0.clone(),
                ty: SqlType::from_data_type(column.data_type.node),
                primary_key: column.primary_key,
                not_null: column.not_null,
                unique: column.unique,
            });
        }
        if columns.len() > MAX_COLUMNS {
            return Err(ExecError::at(
                ErrorKind::TooLarge,
                format!(
                    "table {} has {} columns, maximum {MAX_COLUMNS}",
                    name.node.0,
                    columns.len()
                ),
                name.span,
            ));
        }
        let pk = columns
            .iter()
            .position(|c| c.primary_key && c.ty == SqlType::Integer);
        let mut table = TableDef {
            name: name.node.0.clone(),
            root: cairn_storage::PageId(0),
            pk,
            next_rowid: 1,
            columns,
        };
        let implicit = self.implicit_indexes(&table);
        self.guarded(|pager, catalog| {
            table.root = BTree::create(pager)?;
            catalog.write_table(pager, &table)?;
            catalog.tables.insert(table.name.clone(), table);
            for (name, column) in implicit {
                let index = IndexDef {
                    table: name_of(&create.name),
                    name,
                    column,
                    unique: true,
                    implicit: true,
                    root: BTree::create(pager)?,
                };
                catalog.write_index(pager, &index)?;
                catalog.indexes.insert(index.name.clone(), index);
            }
            Ok(())
        })?;
        Ok(QueryResult::Affected(0))
    }

    /// Names and columns of the implicit unique indexes a new table needs.
    fn implicit_indexes(&self, table: &TableDef) -> Vec<(String, usize)> {
        let mut taken: BTreeSet<String> = self.catalog.indexes.keys().cloned().collect();
        let mut out = Vec::new();
        for (ordinal, column) in table.columns.iter().enumerate() {
            let needs_index = Some(ordinal) != table.pk && (column.unique || column.primary_key);
            if !needs_index {
                continue;
            }
            let base = format!("{IMPLICIT_PREFIX}autoindex_{}_{}", table.name, column.name);
            let mut name = base.clone();
            let mut suffix = 2;
            while taken.contains(&name) {
                name = format!("{base}_{suffix}");
                suffix += 1;
            }
            taken.insert(name.clone());
            out.push((name, ordinal));
        }
        out
    }

    pub(crate) fn drop_table(&mut self, drop: &DropTable) -> Result<QueryResult, ExecError> {
        let Some(table) = self.catalog.tables.get(&drop.name.node.0).cloned() else {
            if drop.if_exists {
                return Ok(QueryResult::Affected(0));
            }
            return Err(ExecError::at(
                ErrorKind::NotFound,
                format!("no such table: {}", drop.name.node.0),
                drop.name.span,
            ));
        };
        let indexes = self.catalog.indexes_of(&table.name);
        self.guarded(|pager, catalog| {
            for index in &indexes {
                destroy_tree(pager, index.root)?;
                catalog.remove_index(pager, &index.name)?;
                catalog.indexes.remove(&index.name);
            }
            destroy_tree(pager, table.root)?;
            catalog.remove_table(pager, &table)?;
            catalog.tables.remove(&table.name);
            Ok(())
        })?;
        Ok(QueryResult::Affected(0))
    }

    pub(crate) fn create_index(&mut self, create: &CreateIndex) -> Result<QueryResult, ExecError> {
        let name = &create.name;
        check_name(name)?;
        if name.node.0.starts_with(IMPLICIT_PREFIX) {
            return Err(ExecError::at(
                ErrorKind::Type,
                format!("index name {} is reserved (prefix cairn_)", name.node.0),
                name.span,
            ));
        }
        if self.catalog.indexes.contains_key(&name.node.0) {
            return Err(ExecError::at(
                ErrorKind::AlreadyExists,
                format!("index {} already exists", name.node.0),
                name.span,
            ));
        }
        let table = self
            .catalog
            .tables
            .get(&create.table.node.0)
            .cloned()
            .ok_or_else(|| {
                ExecError::at(
                    ErrorKind::NotFound,
                    format!("no such table: {}", create.table.node.0),
                    create.table.span,
                )
            })?;
        let column = table.column(&create.column.node.0).ok_or_else(|| {
            ExecError::at(
                ErrorKind::NotFound,
                format!("no such column: {}", create.column.node.0),
                create.column.span,
            )
        })?;
        let rows = scan(&mut self.pager, &table)?;
        let mut seen = BTreeSet::new();
        let mut keys = Vec::with_capacity(rows.len());
        for (key, row) in &rows {
            let value = row
                .get(column)
                .cloned()
                .unwrap_or(crate::value::Value::Null);
            if create.unique && !value.is_null() && !seen.insert(OrdRow(vec![value.clone()])) {
                return Err(ExecError::at(
                    ErrorKind::Constraint,
                    format!(
                        "cannot create unique index {}: column {}.{} has duplicate values",
                        name.node.0,
                        table.name,
                        table.column_name(column)
                    ),
                    name.span,
                ));
            }
            let entry = index_key(&value, *key);
            if entry.len() > MAX_KEY_LEN {
                return Err(ExecError::at(
                    ErrorKind::TooLarge,
                    format!(
                        "index key too large for index {}: {} bytes, maximum {MAX_KEY_LEN}",
                        name.node.0,
                        entry.len()
                    ),
                    name.span,
                ));
            }
            keys.push(entry);
        }
        let index_name = name.node.0.clone();
        self.guarded(|pager, catalog| {
            let mut tree = BTree::open(BTree::create(pager)?);
            for key in &keys {
                tree.insert(pager, key, &[])?;
            }
            let index = IndexDef {
                name: index_name,
                table: table.name.clone(),
                column,
                unique: create.unique,
                implicit: false,
                root: tree.root(),
            };
            catalog.write_index(pager, &index)?;
            catalog.indexes.insert(index.name.clone(), index);
            Ok(())
        })?;
        Ok(QueryResult::Affected(0))
    }
}

fn name_of(ident: &Ident) -> String {
    ident.node.0.clone()
}
