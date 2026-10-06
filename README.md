# cairn

A small embedded SQL database written in Rust using only the standard library.

A cairn database is a single file of fixed-size pages holding B-tree tables,
with a write-ahead log for atomic, crash-safe transactions, and a subset of SQL
for defining and querying tables. The `cairn` shell opens a database file and
runs SQL interactively or from a script.

## Layout

| Crate | Purpose |
|-------|---------|
| `crates/storage` | Page file, buffer pool, B-tree tables, write-ahead log |
| `crates/sql` | Tokenizer, parser and syntax tree for the supported SQL |
| `crates/exec` | Catalog, planner and executor connecting SQL to storage |
| `crates/cli` | The `cairn` interactive shell |

Status: `crates/storage` provides the pager, buffer pool and B-tree (see
[Storage](#storage)); its write-ahead log and the other crates are scaffolds,
each delivered by a later milestone.

## Storage

`cairn-storage` keeps a database in one file of fixed 4096-byte pages. Page 0
is the header; every other page is a B-tree node or a free page. All integers
are little-endian and encoded field by field. Malformed files and pages are
reported as `StorageError` values and never cause a panic. The same tables are
documented in the crate source (`header.rs`, `pager.rs`, `node.rs`).

### Header page (page 0)

| Offset | Size | Field |
|--------|------|-------|
| 0 | 6 | magic `cairn\0` |
| 6 | 2 | reserved, zero |
| 8 | 4 | format version (1) |
| 12 | 4 | page size (4096) |
| 16 | 4 | page count; must equal file length / 4096 |
| 20 | 4 | free-list head page id (0 = empty) |
| 24 | 4 | free-list length |
| 28 | 36 | reserved, zero |
| 64 | 640 | 16 named root slots of 40 bytes |
| 704 | 3392 | reserved, zero |

Root slot: name length `u8` (0 = empty slot), 3 reserved bytes, a 32-byte
zero-padded UTF-8 name, and the root page id `u32` at offset 36.

`Pager::open` checks the file in this order: shorter than one page
(`FileTooShort`), length not a multiple of 4096 (`NotPageMultiple`), wrong
magic (`BadMagic`), version other than 1 (`UnsupportedVersion`), then every
remaining header field (`Corrupt { page: 0, .. }`).

### Free pages

A freed page is overwritten with tag `3` at offset 0, three zero bytes, the
next free page id (`u32`, 0 = end) at offset 4, and zeros after that. Freed
pages form a last-in, first-out list whose head and length are in the header.
`allocate` takes the most recently freed page before growing the file, and the
list survives close and reopen. `free` rejects page 0, ids past the end of the
file and pages that already have the free-page layout.

### B-tree nodes

| Offset | Leaf | Internal |
|--------|------|----------|
| 0 | tag `1` | tag `2` |
| 1 | reserved, zero | reserved, zero |
| 2 | cell count `u16` | key count `u16` (at least 1) |
| 4 | next leaf page id `u32` (0 = last) | leftmost child page id `u32` |
| 8 | cells: key length `u16`, value length `u16`, key, value | cells: key length `u16`, key, right child `u32` |

Cells are packed in ascending key order, and every byte after the last cell is
zero. Keys compare as unsigned bytes. Child `i` of an internal node holds keys
`k` with `key[i-1] <= k < key[i]`. All pairs live in the leaves, which are
linked left to right for range scans.

Keys may be 0 to 256 bytes and values 0 to 1024 bytes; larger ones are
rejected with `StorageError::TooLarge`. A node splits when its cells exceed
the 4088-byte payload area. A non-root node whose payload falls below half
(2044 bytes) after an insert, replace or delete is merged with an adjacent
sibling if both fit in one page; otherwise their cells are redistributed
evenly. Because a single cell can be up to 1284 bytes, a strict half-full
minimum cannot always be restored. `BTree::check` therefore enforces the bound
that splits and rebalancing always guarantee: 1402 bytes for leaves and 1782
bytes for internal nodes.

### Buffer pool

Each `Pager` caches pages in a bounded pool. Its capacity is chosen at open
(`create_with_capacity` / `open_with_capacity`; at least 8 pages, default
256). The least recently used unpinned page is evicted, and dirty pages are
written back on eviction and on `sync`. `pin` / `unpin` keep a page resident;
pins nest. If every frame is pinned and another page is needed, the call
returns `StorageError::PoolExhausted` and leaves the pager unchanged.
`stats`, `resident` and `is_cached` expose the pool state.

### API

```rust
use std::ops::Bound;
use cairn_storage::{BTree, Pager};

let mut pager = Pager::create("app.cairn")?;   // fails if the file exists
let mut tree = BTree::open(BTree::create(&mut pager)?);
tree.insert(&mut pager, b"apple", b"red")?;    // replaces an existing value
let value = tree.get(&mut pager, b"apple")?;   // Some(b"red".to_vec())
for pair in tree.range(&mut pager, Bound::Included(&b"a"[..]), Bound::Unbounded)? {
    let (key, value) = pair?;
}
let removed = tree.delete(&mut pager, b"apple")?;  // true
tree.check(&mut pager).expect("tree is valid");    // Err(description) otherwise
pager.set_root("fruit", tree.root())?;  // the root moves on split or collapse
pager.close()?;                         // sync and report errors
```

- `Pager`: `create`, `open`, `create_with_capacity`, `open_with_capacity`,
  `allocate`, `read`, `write`, `free`, `page_count`, `sync`, `close`, `pin`,
  `unpin`, `free_list`, `set_root`, `root`, `remove_root`, `roots`, and the
  pool accessors above.
- `BTree`: `create`, `open`, `root`, `get`, `insert`, `delete`, `range`,
  `check`. A tree is a small handle and every method takes `&mut Pager`, so
  several trees can share one file. `range` is lazy, and inverted or empty
  bounds yield nothing.
- `StorageError`: `Io`, `FileTooShort`, `NotPageMultiple`, `BadMagic`,
  `UnsupportedVersion`, `Corrupt { page, reason }`, `PoolExhausted`,
  `PoolTooSmall`, `TooLarge`, `InvalidPage`, `DatabaseFull`,
  `InvalidRootName`, `RootTableFull`.

`sync` writes dirty pages and the header, then calls `File::sync_all`.
Dropping a `Pager` syncs on a best-effort basis and ignores errors, so call
`close` to see them.

Not yet provided: a write-ahead log or crash atomicity (a crash before `sync`
can leave a file that `open` rejects), file locking or concurrent access,
overflow pages for larger keys or values, and page checksums.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

No dependencies outside the Rust standard library are allowed.
