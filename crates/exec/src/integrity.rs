//! Whole-database consistency checks, shared by [`crate::Database::check`]
//! (first problem as an error) and `PRAGMA integrity_check` (every problem).
//!
//! In order, [`collect`] reports:
//! - the catalog and every table and index B-tree failing `check()`;
//! - a page owned by two trees;
//! - per table: a primary key that differs from its row key, a row id not
//!   below the next row id, an index whose entries differ from the rows, and
//!   duplicates in a unique index;
//! - a page that is on the free list and owned by a tree;
//! - a page that is neither free nor owned (leaked).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use cairn_storage::{BTree, PageId, Pager};

use crate::catalog::{Catalog, IndexDef, TableDef};
use crate::codec::key::decode_index_key;
use crate::table::{KeyedRow, scan};
use crate::value::{OrdRow, SqlType, Value};

/// Every problem found, in the order above; empty when the database is
/// consistent.
pub fn collect(pager: &mut Pager, catalog: &Catalog) -> Vec<String> {
    let mut audit = Audit {
        pager,
        problems: Vec::new(),
        owners: BTreeMap::new(),
        complete: true,
    };
    audit.owners.insert(PageId(0), "the header".to_string());
    audit.tree("catalog".to_string(), catalog.root);
    for table in catalog.tables.values() {
        audit.tree(format!("table {}", table.name), table.root);
        let rows = match scan(audit.pager, table) {
            Ok(rows) => rows,
            Err(e) => {
                audit.problems.push(format!("table {}: {e}", table.name));
                continue;
            }
        };
        audit.table_rules(table, &rows);
        for index in catalog.indexes_of(&table.name) {
            audit.tree(format!("index {}", index.name), index.root);
            audit.index_rules(table, &index, &rows);
        }
    }
    audit.free_pages();
    audit.problems
}

struct Audit<'p> {
    pager: &'p mut Pager,
    problems: Vec<String>,
    owners: BTreeMap<PageId, String>,
    /// False once a tree could not be walked, so unowned pages are not
    /// reported as leaks that are really an unreadable tree.
    complete: bool,
}

impl Audit<'_> {
    fn tree(&mut self, owner: String, root: PageId) {
        let tree = BTree::open(root);
        if let Err(problem) = tree.check(self.pager) {
            self.problems.push(format!("{owner}: {problem}"));
        }
        let pages = match tree.pages(self.pager) {
            Ok(pages) => pages,
            Err(e) => {
                self.problems.push(format!("{owner}: {e}"));
                self.complete = false;
                return;
            }
        };
        for page in pages {
            match self.owners.get(&page) {
                Some(other) => self
                    .problems
                    .push(format!("page {page} is used by {other} and {owner}")),
                None => {
                    self.owners.insert(page, owner.clone());
                }
            }
        }
    }

    fn table_rules(&mut self, table: &TableDef, rows: &[KeyedRow]) {
        for (key, row) in rows {
            match table.pk {
                Some(pk) if row.get(pk) != Some(&Value::Integer(*key)) => {
                    self.problems.push(format!(
                        "table {}: key {key} differs from its primary key",
                        table.name
                    ));
                }
                None if *key >= table.next_rowid => {
                    self.problems.push(format!(
                        "table {}: row id {key} not below next row id",
                        table.name
                    ));
                }
                _ => {}
            }
        }
    }

    fn index_rules(&mut self, table: &TableDef, index: &IndexDef, rows: &[KeyedRow]) {
        let ty = table
            .columns
            .get(index.column)
            .map_or(SqlType::Null, |c| c.ty);
        let stored = match self.index_entries(index, ty) {
            Ok(stored) => stored,
            Err(problem) => {
                self.problems
                    .push(format!("index {}: {problem}", index.name));
                return;
            }
        };
        let expected: BTreeSet<(OrdRow, i64)> = rows
            .iter()
            .map(|(key, row)| {
                let value = row.get(index.column).cloned().unwrap_or(Value::Null);
                (OrdRow(vec![value]), *key)
            })
            .collect();
        if stored != expected {
            self.problems.push(format!(
                "index {} does not match table {}",
                index.name, table.name
            ));
        }
        if index.unique {
            let mut seen = BTreeSet::new();
            for (value, _) in &stored {
                if !value.0.iter().all(Value::is_null) && !seen.insert(value.clone()) {
                    self.problems
                        .push(format!("unique index {} has duplicates", index.name));
                    break;
                }
            }
        }
    }

    fn index_entries(
        &mut self,
        index: &IndexDef,
        ty: SqlType,
    ) -> Result<BTreeSet<(OrdRow, i64)>, String> {
        let range = BTree::open(index.root)
            .range(self.pager, Bound::Unbounded, Bound::Unbounded)
            .map_err(|e| e.to_string())?;
        let mut stored = BTreeSet::new();
        for pair in range {
            let (key, _) = pair.map_err(|e| e.to_string())?;
            let (value, row) = decode_index_key(&key, ty).map_err(|e| e.to_string())?;
            stored.insert((OrdRow(vec![value]), row));
        }
        Ok(stored)
    }

    fn free_pages(&mut self) {
        let free: BTreeSet<PageId> = match self.pager.free_list() {
            Ok(list) => list.into_iter().collect(),
            Err(e) => {
                self.problems.push(format!("free list: {e}"));
                return;
            }
        };
        for page in &free {
            if let Some(owner) = self.owners.get(page) {
                self.problems
                    .push(format!("page {page} is both free and in use by {owner}"));
            }
        }
        if !self.complete {
            return;
        }
        for id in 1..self.pager.page_count() {
            let page = PageId(id);
            if !self.owners.contains_key(&page) && !free.contains(&page) {
                self.problems
                    .push(format!("page {page} is neither free nor in use"));
            }
        }
    }
}
