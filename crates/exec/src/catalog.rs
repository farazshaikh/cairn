//! The schema catalog: tables, columns and indexes, persisted in one
//! B-tree whose root is in page-0 root slot `catalog`, and cached in
//! memory.
//!
//! Catalog entries (every value starts with version byte 1):
//!
//! | Key | Value |
//! |-----|-------|
//! | `4D` (`M`) | `[1]`: format meta |
//! | `54` (`T`) + table name | `[1]`, root u32 LE, key kind u8 (0 hidden row id, 1 INTEGER PRIMARY KEY), PK ordinal u16 LE (`FFFF` if none), next row id i64 LE, column count u16 LE |
//! | `43` (`C`) + table + `00` + ordinal u16 BE | `[1]`, type u8 (1 INTEGER, 2 REAL, 3 TEXT, 4 BOOLEAN), flags u8 (bit 0 PRIMARY KEY, bit 1 NOT NULL as declared, bit 2 UNIQUE), name length u8, name |
//! | `49` (`I`) + index name | `[1]`, root u32 LE, flags u8 (bit 0 unique, bit 1 implicit), column ordinal u16 LE, table name length u8, table name |
//!
//! Names are 1 to 64 bytes of UTF-8 without NUL, which keeps every
//! catalog key far below the 256-byte key limit. One entry per column keeps
//! every value below the 1024-byte value limit.

use std::collections::BTreeMap;
use std::ops::Bound;

use cairn_storage::{BTree, PageId, Pager};

use crate::error::ExecError;
use crate::value::SqlType;

pub const CATALOG_ROOT: &str = "catalog";
pub const CATALOG_VERSION: u8 = 1;
pub const MAX_NAME_LEN: usize = 64;
pub const MAX_COLUMNS: usize = 255;
pub const IMPLICIT_PREFIX: &str = "cairn_";

const NO_PK: u16 = u16::MAX;

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: SqlType,
    pub primary_key: bool,
    pub not_null: bool,
    pub unique: bool,
}

impl ColumnDef {
    /// NOT NULL as enforced: declared, or implied by PRIMARY KEY.
    pub fn required(&self) -> bool {
        self.not_null || self.primary_key
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableDef {
    pub name: String,
    pub root: PageId,
    /// Ordinal of the INTEGER PRIMARY KEY column that keys the table, or
    /// `None` when rows are keyed by a hidden row id.
    pub pk: Option<usize>,
    pub next_rowid: i64,
    pub columns: Vec<ColumnDef>,
}

impl TableDef {
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    pub fn column_name(&self, ordinal: usize) -> &str {
        self.columns
            .get(ordinal)
            .map_or("?", |column| column.name.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexDef {
    pub name: String,
    pub table: String,
    pub column: usize,
    pub unique: bool,
    pub implicit: bool,
    pub root: PageId,
}

/// The in-memory catalog and the root of its B-tree.
#[derive(Debug)]
pub struct Catalog {
    pub root: PageId,
    pub tables: BTreeMap<String, TableDef>,
    pub indexes: BTreeMap<String, IndexDef>,
}

impl Catalog {
    pub fn create(pager: &mut Pager) -> Result<Catalog, ExecError> {
        let root = BTree::create(pager)?;
        let mut catalog = Catalog {
            root,
            tables: BTreeMap::new(),
            indexes: BTreeMap::new(),
        };
        catalog.put(pager, b"M", &[CATALOG_VERSION])?;
        Ok(catalog)
    }

    pub fn load(pager: &mut Pager) -> Result<Catalog, ExecError> {
        let root = pager
            .root(CATALOG_ROOT)
            .ok_or_else(|| ExecError::corrupt("not a cairn database: no catalog"))?;
        let entries: Vec<(Vec<u8>, Vec<u8>)> = BTree::open(root)
            .range(pager, Bound::Unbounded, Bound::Unbounded)?
            .collect::<Result<_, _>>()?;
        let catalog = decode_entries(root, entries)
            .map_err(|reason| ExecError::corrupt(format!("corrupt catalog: {reason}")))?;
        validate(&catalog, pager.page_count())
            .map_err(|reason| ExecError::corrupt(format!("corrupt catalog: {reason}")))?;
        Ok(catalog)
    }

    /// Indexes of `table`, in index-name order.
    pub fn indexes_of(&self, table: &str) -> Vec<IndexDef> {
        self.indexes
            .values()
            .filter(|index| index.table == table)
            .cloned()
            .collect()
    }

    pub fn write_table(&mut self, pager: &mut Pager, table: &TableDef) -> Result<(), ExecError> {
        self.put(pager, &table_key(&table.name), &encode_table(table))?;
        for (ordinal, column) in table.columns.iter().enumerate() {
            self.put(
                pager,
                &column_key(&table.name, ordinal),
                &encode_column(column),
            )?;
        }
        Ok(())
    }

    /// Rewrites only the table header (root and next row id).
    pub fn write_table_header(
        &mut self,
        pager: &mut Pager,
        table: &TableDef,
    ) -> Result<(), ExecError> {
        self.put(pager, &table_key(&table.name), &encode_table(table))
    }

    pub fn remove_table(&mut self, pager: &mut Pager, table: &TableDef) -> Result<(), ExecError> {
        self.delete(pager, &table_key(&table.name))?;
        for ordinal in 0..table.columns.len() {
            self.delete(pager, &column_key(&table.name, ordinal))?;
        }
        Ok(())
    }

    pub fn write_index(&mut self, pager: &mut Pager, index: &IndexDef) -> Result<(), ExecError> {
        self.put(pager, &index_key(&index.name), &encode_index(index))
    }

    pub fn remove_index(&mut self, pager: &mut Pager, name: &str) -> Result<(), ExecError> {
        self.delete(pager, &index_key(name))
    }

    fn put(&mut self, pager: &mut Pager, key: &[u8], value: &[u8]) -> Result<(), ExecError> {
        let mut tree = BTree::open(self.root);
        tree.insert(pager, key, value)?;
        self.track_root(pager, tree)
    }

    fn delete(&mut self, pager: &mut Pager, key: &[u8]) -> Result<(), ExecError> {
        let mut tree = BTree::open(self.root);
        tree.delete(pager, key)?;
        self.track_root(pager, tree)
    }

    fn track_root(&mut self, pager: &mut Pager, tree: BTree) -> Result<(), ExecError> {
        if tree.root() != self.root || pager.root(CATALOG_ROOT).is_none() {
            self.root = tree.root();
            pager.set_root(CATALOG_ROOT, self.root)?;
        }
        Ok(())
    }
}

/// Checks a user-supplied name.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME_LEN && !name.contains('\0')
}

fn table_key(name: &str) -> Vec<u8> {
    let mut key = vec![b'T'];
    key.extend_from_slice(name.as_bytes());
    key
}

fn column_key(table: &str, ordinal: usize) -> Vec<u8> {
    let mut key = vec![b'C'];
    key.extend_from_slice(table.as_bytes());
    key.push(0);
    let ordinal = u16::try_from(ordinal).unwrap_or(u16::MAX);
    key.extend_from_slice(&ordinal.to_be_bytes());
    key
}

fn index_key(name: &str) -> Vec<u8> {
    let mut key = vec![b'I'];
    key.extend_from_slice(name.as_bytes());
    key
}

fn encode_table(table: &TableDef) -> Vec<u8> {
    let mut out = vec![CATALOG_VERSION];
    out.extend_from_slice(&table.root.0.to_le_bytes());
    out.push(u8::from(table.pk.is_some()));
    let pk = table
        .pk
        .and_then(|ordinal| u16::try_from(ordinal).ok())
        .unwrap_or(NO_PK);
    out.extend_from_slice(&pk.to_le_bytes());
    out.extend_from_slice(&table.next_rowid.to_le_bytes());
    let count = u16::try_from(table.columns.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&count.to_le_bytes());
    out
}

fn type_code(ty: SqlType) -> u8 {
    match ty {
        SqlType::Integer => 1,
        SqlType::Real => 2,
        SqlType::Text => 3,
        SqlType::Boolean | SqlType::Null => 4,
    }
}

fn encode_column(column: &ColumnDef) -> Vec<u8> {
    let flags = u8::from(column.primary_key)
        | (u8::from(column.not_null) << 1)
        | (u8::from(column.unique) << 2);
    let mut out = vec![CATALOG_VERSION, type_code(column.ty), flags];
    out.push(u8::try_from(column.name.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(column.name.as_bytes());
    out
}

fn encode_index(index: &IndexDef) -> Vec<u8> {
    let mut out = vec![CATALOG_VERSION];
    out.extend_from_slice(&index.root.0.to_le_bytes());
    out.push(u8::from(index.unique) | (u8::from(index.implicit) << 1));
    let column = u16::try_from(index.column).unwrap_or(u16::MAX);
    out.extend_from_slice(&column.to_le_bytes());
    out.push(u8::try_from(index.table.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(index.table.as_bytes());
    out
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Result<Cursor<'a>, String> {
        let mut cursor = Cursor { bytes, pos: 0 };
        let version = cursor.u8()?;
        if version != CATALOG_VERSION {
            return Err(format!("unknown entry version {version}"));
        }
        Ok(cursor)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self.pos + len;
        let slice = self.bytes.get(self.pos..end).ok_or("entry is truncated")?;
        self.pos = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.take(N)?
            .try_into()
            .map_err(|_| "entry is truncated".to_string())
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn name(&mut self) -> Result<String, String> {
        let len = usize::from(self.u8()?);
        let bytes = self.take(len)?;
        let name = std::str::from_utf8(bytes).map_err(|_| "name is not UTF-8")?;
        if !valid_name(name) {
            return Err(format!("invalid name {name:?}"));
        }
        Ok(name.to_string())
    }

    fn finish(&self) -> Result<(), String> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err("trailing bytes in entry".to_string())
        }
    }
}

fn utf8_name(bytes: &[u8]) -> Result<String, String> {
    let name = std::str::from_utf8(bytes).map_err(|_| "name is not UTF-8")?;
    if !valid_name(name) {
        return Err(format!("invalid name {name:?}"));
    }
    Ok(name.to_string())
}

struct TableHeader {
    root: u32,
    key_kind: u8,
    pk: u16,
    next_rowid: i64,
    column_count: usize,
}

fn decode_entries(root: PageId, entries: Vec<(Vec<u8>, Vec<u8>)>) -> Result<Catalog, String> {
    let mut meta = false;
    let mut headers: BTreeMap<String, TableHeader> = BTreeMap::new();
    let mut columns: BTreeMap<String, BTreeMap<u16, ColumnDef>> = BTreeMap::new();
    let mut indexes = BTreeMap::new();
    for (key, value) in entries {
        let Some((&tag, rest)) = key.split_first() else {
            return Err("empty key".to_string());
        };
        let mut cursor = Cursor::new(&value)?;
        match tag {
            b'M' if rest.is_empty() => meta = true,
            b'T' => {
                let header = TableHeader {
                    root: cursor.u32()?,
                    key_kind: cursor.u8()?,
                    pk: cursor.u16()?,
                    next_rowid: cursor.i64()?,
                    column_count: usize::from(cursor.u16()?),
                };
                headers.insert(utf8_name(rest)?, header);
            }
            b'C' => {
                let split = rest.len().checked_sub(3).ok_or("short column key")?;
                let (table, tail) = rest.split_at(split);
                let (separator, ordinal) = tail.split_at(1);
                if separator != [0] {
                    return Err("bad column key".to_string());
                }
                let ordinal = u16::from_be_bytes(ordinal.try_into().map_err(|_| "bad column key")?);
                let ty = match cursor.u8()? {
                    1 => SqlType::Integer,
                    2 => SqlType::Real,
                    3 => SqlType::Text,
                    4 => SqlType::Boolean,
                    other => return Err(format!("unknown column type {other}")),
                };
                let flags = cursor.u8()?;
                if flags & !0b111 != 0 {
                    return Err(format!("unknown column flags {flags}"));
                }
                let column = ColumnDef {
                    name: cursor.name()?,
                    ty,
                    primary_key: flags & 1 != 0,
                    not_null: flags & 2 != 0,
                    unique: flags & 4 != 0,
                };
                columns
                    .entry(utf8_name(table)?)
                    .or_default()
                    .insert(ordinal, column);
            }
            b'I' => {
                let root = cursor.u32()?;
                let flags = cursor.u8()?;
                if flags & !0b11 != 0 {
                    return Err(format!("unknown index flags {flags}"));
                }
                let column = usize::from(cursor.u16()?);
                let table = cursor.name()?;
                let name = utf8_name(rest)?;
                indexes.insert(
                    name.clone(),
                    IndexDef {
                        name,
                        table,
                        column,
                        unique: flags & 1 != 0,
                        implicit: flags & 2 != 0,
                        root: PageId(root),
                    },
                );
            }
            other => return Err(format!("unknown entry tag {other}")),
        }
        cursor.finish()?;
    }
    if !meta {
        return Err("missing format entry".to_string());
    }
    let mut tables = BTreeMap::new();
    for (name, header) in headers {
        let defs = columns.remove(&name).unwrap_or_default();
        let ordinals: Vec<u16> = defs.keys().copied().collect();
        let expected: Vec<u16> = (0..header.column_count)
            .map(|i| u16::try_from(i).unwrap_or(u16::MAX))
            .collect();
        if ordinals != expected || header.column_count == 0 {
            return Err(format!("table {name} has missing or extra columns"));
        }
        let pk = match (header.key_kind, header.pk) {
            (0, NO_PK) => None,
            (1, ordinal) if usize::from(ordinal) < header.column_count => {
                Some(usize::from(ordinal))
            }
            _ => return Err(format!("table {name} has an invalid key kind")),
        };
        tables.insert(
            name.clone(),
            TableDef {
                name,
                root: PageId(header.root),
                pk,
                next_rowid: header.next_rowid,
                columns: defs.into_values().collect(),
            },
        );
    }
    if let Some(table) = columns.keys().next() {
        return Err(format!("columns for unknown table {table}"));
    }
    Ok(Catalog {
        root,
        tables,
        indexes,
    })
}

fn validate(catalog: &Catalog, page_count: u32) -> Result<(), String> {
    let valid_root = |root: PageId| root.0 != 0 && root.0 < page_count;
    for table in catalog.tables.values() {
        if !valid_root(table.root) {
            return Err(format!("table {} has an invalid root", table.name));
        }
        let pks = table.columns.iter().filter(|c| c.primary_key).count();
        if pks > 1 {
            return Err(format!(
                "table {} has more than one primary key",
                table.name
            ));
        }
        let integer_pk = table
            .columns
            .iter()
            .position(|c| c.primary_key && c.ty == SqlType::Integer);
        if integer_pk != table.pk {
            return Err(format!("table {} has an inconsistent key kind", table.name));
        }
    }
    for index in catalog.indexes.values() {
        if !valid_root(index.root) {
            return Err(format!("index {} has an invalid root", index.name));
        }
        let column_exists = catalog
            .tables
            .get(&index.table)
            .is_some_and(|table| index.column < table.columns.len());
        if !column_exists {
            return Err(format!("index {} refers to an unknown column", index.name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> TableDef {
        TableDef {
            name: "people".into(),
            root: PageId(3),
            pk: Some(0),
            next_rowid: 1,
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    ty: SqlType::Integer,
                    primary_key: true,
                    not_null: false,
                    unique: false,
                },
                ColumnDef {
                    name: "name".into(),
                    ty: SqlType::Text,
                    primary_key: false,
                    not_null: true,
                    unique: true,
                },
            ],
        }
    }

    fn entries(table: &TableDef, index: &IndexDef) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut entries = vec![(b"M".to_vec(), vec![1])];
        entries.push((table_key(&table.name), encode_table(table)));
        for (i, column) in table.columns.iter().enumerate() {
            entries.push((column_key(&table.name, i), encode_column(column)));
        }
        entries.push((index_key(&index.name), encode_index(index)));
        entries
    }

    fn index() -> IndexDef {
        IndexDef {
            name: "cairn_autoindex_people_name".into(),
            table: "people".into(),
            column: 1,
            unique: true,
            implicit: true,
            root: PageId(4),
        }
    }

    #[test]
    fn entries_round_trip() {
        let catalog = decode_entries(PageId(1), entries(&table(), &index())).expect("decode");
        assert_eq!(catalog.tables.get("people"), Some(&table()));
        assert_eq!(
            catalog.indexes.get("cairn_autoindex_people_name"),
            Some(&index())
        );
        assert_eq!(validate(&catalog, 10), Ok(()));
        assert!(validate(&catalog, 4).is_err());
    }

    #[test]
    fn malformed_entries_are_errors() {
        let good = entries(&table(), &index());
        for (i, (_, value)) in good.iter().enumerate() {
            for len in 0..value.len() {
                let mut broken = good.clone();
                broken[i].1.truncate(len);
                assert!(
                    decode_entries(PageId(1), broken).is_err(),
                    "entry {i} prefix {len}"
                );
            }
        }
        let mut missing_column = good.clone();
        missing_column.remove(2);
        assert!(decode_entries(PageId(1), missing_column).is_err());
        let mut no_meta = good.clone();
        no_meta.remove(0);
        assert!(decode_entries(PageId(1), no_meta).is_err());
        let mut bad_type = good;
        bad_type[2].1[1] = 9;
        assert!(decode_entries(PageId(1), bad_type).is_err());
    }
}
