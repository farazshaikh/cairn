//! The in-memory database: tables with their rows in key order, and
//! indexes, which never change results but decide the order an access
//! path reads rows in (README "Keys, constraints and atomicity" and
//! "Planner and EXPLAIN").

use std::collections::BTreeMap;

use crate::value::{RValue, SType};

#[derive(Debug, Clone)]
pub(crate) struct Column {
    pub name: String,
    pub ty: SType,
    pub primary_key: bool,
    pub not_null: bool,
    pub unique: bool,
}

impl Column {
    pub fn required(&self) -> bool {
        self.primary_key || self.not_null
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    /// The INTEGER PRIMARY KEY column, which is the row key.
    pub int_pk: Option<usize>,
    /// Rows sorted by key: the INTEGER PRIMARY KEY, or a hidden row id.
    pub rows: Vec<(i64, Vec<RValue>)>,
    pub next_rowid: i64,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    pub fn column_types(&self) -> Vec<(String, SType)> {
        self.columns
            .iter()
            .map(|c| (c.name.clone(), c.ty))
            .collect()
    }

    pub fn sort_rows(&mut self) {
        self.rows.sort_by_key(|(key, _)| *key);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Index {
    pub table: String,
    pub column: usize,
    pub unique: bool,
}

/// Every table and index, by name. Index names share one namespace.
#[derive(Debug, Clone, Default)]
pub(crate) struct State {
    pub tables: BTreeMap<String, Table>,
    pub indexes: BTreeMap<String, Index>,
}

impl State {
    /// The indexes of `table` in name order.
    pub fn indexes_of(&self, table: &str) -> Vec<&Index> {
        self.indexes
            .values()
            .filter(|index| index.table == table)
            .collect()
    }
}

/// Names are 1 to 64 bytes without NUL.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && !name.contains('\0')
}

/// The README implicit index name: `cairn_autoindex_<table>_<column>`, cut
/// to 64 bytes on a character boundary, then `_2`, `_3`, ... until free.
pub(crate) fn implicit_index_name(table: &str, column: &str, state: &State) -> String {
    let full = format!("cairn_autoindex_{table}_{column}");
    let mut n = 1u64;
    loop {
        let suffix = if n == 1 {
            String::new()
        } else {
            format!("_{n}")
        };
        let mut end = full.len().min(64 - suffix.len());
        while !full.is_char_boundary(end) {
            end -= 1;
        }
        let name = format!("{}{suffix}", full.get(..end).unwrap_or(""));
        if !state.indexes.contains_key(&name) {
            return name;
        }
        n += 1;
    }
}
