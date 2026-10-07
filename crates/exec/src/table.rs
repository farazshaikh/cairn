//! Row and index access over `cairn-storage` B-trees, and the apply phase
//! of write statements.
//!
//! Write statements first build a complete [`WriteSet`] while validating;
//! only [`apply`] touches the pager. A storage error during apply can leave
//! trees half-written in the transaction overlay; the caller rolls the
//! statement's savepoint or transaction back, so nothing reaches the log.

use std::collections::BTreeSet;
use std::ops::Bound;

use cairn_storage::{BTree, PageId, Pager};

use crate::catalog::{Catalog, IndexDef, TableDef};
use crate::codec::key::{decode_row_key, index_key, row_key, value_key};
use crate::codec::record::decode_record;
use crate::error::ExecError;
use crate::value::Value;

/// A row of a table and its key.
pub type KeyedRow = (i64, Vec<Value>);

fn decode(table: &TableDef, bytes: &[u8]) -> Result<Vec<Value>, ExecError> {
    decode_record(bytes, table.columns.len()).map_err(|reason| {
        ExecError::corrupt(format!("corrupt record in table {}: {reason}", table.name))
    })
}

fn corrupt_key(table: &str) -> ExecError {
    ExecError::corrupt(format!("corrupt record in table {table}: bad key"))
}

/// Rows with keys in `(low, high)`, in key order.
pub fn rows_in_range(
    pager: &mut Pager,
    table: &TableDef,
    low: Bound<i64>,
    high: Bound<i64>,
) -> Result<Vec<KeyedRow>, ExecError> {
    let low = low.map(row_key);
    let high = high.map(row_key);
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = BTree::open(table.root)
        .range(
            pager,
            low.as_ref().map(|k| k.as_slice()),
            high.as_ref().map(|k| k.as_slice()),
        )?
        .collect::<Result<_, _>>()?;
    pairs
        .into_iter()
        .map(|(key, value)| {
            let key = decode_row_key(&key).ok_or_else(|| corrupt_key(&table.name))?;
            Ok((key, decode(table, &value)?))
        })
        .collect()
}

pub fn scan(pager: &mut Pager, table: &TableDef) -> Result<Vec<KeyedRow>, ExecError> {
    rows_in_range(pager, table, Bound::Unbounded, Bound::Unbounded)
}

pub fn get_row(
    pager: &mut Pager,
    table: &TableDef,
    key: i64,
) -> Result<Option<Vec<Value>>, ExecError> {
    match BTree::open(table.root).get(pager, &row_key(key))? {
        Some(bytes) => Ok(Some(decode(table, &bytes)?)),
        None => Ok(None),
    }
}

/// Row keys of index entries whose column value lies in `(low, high)`.
/// NULL entries are never returned: comparisons never match NULL.
pub fn index_range_keys(
    pager: &mut Pager,
    index: &IndexDef,
    low: Bound<&Value>,
    high: Bound<&Value>,
) -> Result<Vec<i64>, ExecError> {
    let after = |v: &Value| {
        let mut key = value_key(v);
        key.extend_from_slice(&[0xFF; 8]);
        key
    };
    let start = match low {
        Bound::Unbounded => Bound::Included(vec![1]),
        Bound::Included(v) => Bound::Included(value_key(v)),
        Bound::Excluded(v) => Bound::Excluded(after(v)),
    };
    let end = match high {
        Bound::Unbounded => Bound::Unbounded,
        Bound::Included(v) => Bound::Included(after(v)),
        Bound::Excluded(v) => Bound::Excluded(value_key(v)),
    };
    let keys: Vec<(Vec<u8>, Vec<u8>)> = BTree::open(index.root)
        .range(
            pager,
            start.as_ref().map(Vec::as_slice),
            end.as_ref().map(Vec::as_slice),
        )?
        .collect::<Result<_, _>>()?;
    keys.into_iter()
        .map(|(key, _)| {
            let split = key.len().saturating_sub(8);
            key.get(split..)
                .and_then(decode_row_key)
                .ok_or_else(|| ExecError::corrupt(format!("corrupt index {}", index.name)))
        })
        .collect()
}

/// Row keys of the index entries for exactly `value` (none for NULL).
pub fn index_lookup_keys(
    pager: &mut Pager,
    index: &IndexDef,
    value: &Value,
) -> Result<Vec<i64>, ExecError> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    index_range_keys(pager, index, Bound::Included(value), Bound::Included(value))
}

/// Fetches rows by key in the given order, skipping keys without a row.
pub fn rows_by_keys(
    pager: &mut Pager,
    table: &TableDef,
    keys: Vec<i64>,
) -> Result<Vec<KeyedRow>, ExecError> {
    let mut rows = Vec::with_capacity(keys.len());
    for key in keys {
        if let Some(row) = get_row(pager, table, key)? {
            rows.push((key, row));
        }
    }
    Ok(rows)
}

/// Every write a statement makes, built during validation.
#[derive(Debug, Default)]
pub struct WriteSet {
    pub index_deletes: Vec<(String, Vec<u8>)>,
    pub row_deletes: Vec<(String, i64)>,
    pub row_inserts: Vec<(String, i64, Vec<u8>)>,
    pub index_inserts: Vec<(String, Vec<u8>)>,
    pub next_rowid: Option<(String, i64)>,
}

impl WriteSet {
    pub fn delete_row(&mut self, table: &TableDef, indexes: &[IndexDef], key: i64, row: &[Value]) {
        for index in indexes {
            let value = row.get(index.column).cloned().unwrap_or(Value::Null);
            self.index_deletes
                .push((index.name.clone(), index_key(&value, key)));
        }
        self.row_deletes.push((table.name.clone(), key));
    }

    pub fn insert_row(
        &mut self,
        table: &TableDef,
        indexes: &[IndexDef],
        key: i64,
        record: Vec<u8>,
        row: &[Value],
    ) {
        self.row_inserts.push((table.name.clone(), key, record));
        for index in indexes {
            let value = row.get(index.column).cloned().unwrap_or(Value::Null);
            self.index_inserts
                .push((index.name.clone(), index_key(&value, key)));
        }
    }
}

/// Applies a write set in order: index deletes, row deletes, row inserts,
/// index inserts, then catalog updates for moved roots and row ids.
pub fn apply(pager: &mut Pager, catalog: &mut Catalog, writes: WriteSet) -> Result<(), ExecError> {
    let mut tables = BTreeSet::new();
    let mut indexes = BTreeSet::new();
    for (name, key) in &writes.index_deletes {
        let root = index_root(catalog, name)?;
        let mut tree = BTree::open(root);
        tree.delete(pager, key)?;
        move_index_root(catalog, name, tree.root(), &mut indexes);
    }
    for (name, key) in &writes.row_deletes {
        let root = table_root(catalog, name)?;
        let mut tree = BTree::open(root);
        tree.delete(pager, &row_key(*key))?;
        move_table_root(catalog, name, tree.root(), &mut tables);
    }
    for (name, key, record) in &writes.row_inserts {
        let root = table_root(catalog, name)?;
        let mut tree = BTree::open(root);
        tree.insert(pager, &row_key(*key), record)?;
        move_table_root(catalog, name, tree.root(), &mut tables);
    }
    for (name, key) in &writes.index_inserts {
        let root = index_root(catalog, name)?;
        let mut tree = BTree::open(root);
        tree.insert(pager, key, &[])?;
        move_index_root(catalog, name, tree.root(), &mut indexes);
    }
    if let Some((name, next)) = &writes.next_rowid
        && let Some(table) = catalog.tables.get_mut(name)
    {
        table.next_rowid = *next;
        tables.insert(name.clone());
    }
    for name in tables {
        if let Some(table) = catalog.tables.get(&name).cloned() {
            catalog.write_table_header(pager, &table)?;
        }
    }
    for name in indexes {
        if let Some(index) = catalog.indexes.get(&name).cloned() {
            catalog.write_index(pager, &index)?;
        }
    }
    Ok(())
}

fn missing(what: &str, name: &str) -> ExecError {
    ExecError::corrupt(format!("corrupt catalog: {what} {name} disappeared"))
}

fn table_root(catalog: &Catalog, name: &str) -> Result<PageId, ExecError> {
    catalog
        .tables
        .get(name)
        .map(|t| t.root)
        .ok_or_else(|| missing("table", name))
}

fn index_root(catalog: &Catalog, name: &str) -> Result<PageId, ExecError> {
    catalog
        .indexes
        .get(name)
        .map(|i| i.root)
        .ok_or_else(|| missing("index", name))
}

fn move_table_root(catalog: &mut Catalog, name: &str, root: PageId, moved: &mut BTreeSet<String>) {
    if let Some(table) = catalog.tables.get_mut(name)
        && table.root != root
    {
        table.root = root;
        moved.insert(name.to_string());
    }
}

fn move_index_root(catalog: &mut Catalog, name: &str, root: PageId, moved: &mut BTreeSet<String>) {
    if let Some(index) = catalog.indexes.get_mut(name)
        && index.root != root
    {
        index.root = root;
        moved.insert(name.to_string());
    }
}

/// Deletes every key of a tree, which frees every page but the root as
/// nodes merge and the root collapses, then frees the root.
pub fn destroy_tree(pager: &mut Pager, root: PageId) -> Result<(), ExecError> {
    let keys: Vec<Vec<u8>> = BTree::open(root)
        .range(pager, Bound::Unbounded, Bound::Unbounded)?
        .map(|pair| pair.map(|(key, _)| key))
        .collect::<Result<_, _>>()?;
    let mut tree = BTree::open(root);
    for key in keys {
        tree.delete(pager, &key)?;
    }
    pager.free(tree.root())?;
    Ok(())
}
