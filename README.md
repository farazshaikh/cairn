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
[Storage](#storage)) and `crates/sql` the tokenizer and parser (see
[SQL](#sql)). The write-ahead log, `crates/exec` and `crates/cli` are
scaffolds, each delivered by a later milestone.

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

## SQL

`cairn-sql` turns SQL text into a typed syntax tree. It does not check
names, types or meaning; that is the job of `crates/exec`.

```rust
use cairn_sql::{parse, parse_expr, tokenize};

let statements = parse("SELECT a FROM t WHERE a > 1; DELETE FROM t")?;  // Vec<Statement>
let canonical = statements[0].to_string();      // "SELECT a FROM t WHERE a > 1"
let expr = parse_expr("1 + 2 * 3")?;            // one expression
let shape = expr.fully_parenthesized().to_string();  // "(1 + (2 * 3))"
let tokens = tokenize("a <> 'x'")?;             // Vec<Token>, always ending with Eof
```

Every token and syntax node carries a `Span`: byte offsets `start` and `end`
(exclusive), and the 1-based `line` and `column` of `start`. Columns count
characters, and only `\n` starts a line. Syntax nodes are `Spanned<T>`, whose
equality ignores spans, so the same SQL with different spacing parses to equal
trees.

Errors are `SqlError { message, span, source_line }`. Parse errors name what
was expected and what was found, and `Display` shows the line with a caret:

```text
line 3, column 6: expected expression, found end of input
WHERE
     ^
```

### Lexical rules

- Keywords are case-insensitive. Unquoted identifiers are ASCII letters,
  digits and `_`, not starting with a digit, and are lowercased. Double-quoted
  identifiers keep their case and may contain any character; `""` is a quote.
- Reserved words cannot be unquoted identifiers: `AND AS ASC BEGIN BETWEEN BY
  CASE COMMIT CREATE DELETE DESC DISTINCT DROP ELSE END EXISTS FALSE FROM GROUP
  HAVING IF IN INDEX INNER INSERT INTO IS JOIN KEY LEFT LIKE LIMIT NOT NULL
  OFFSET ON OR ORDER PRIMARY ROLLBACK SELECT SET TABLE THEN TRUE UNIQUE UPDATE
  VALUES WHEN WHERE`. Type names and function names (`text`, `count`) are not
  reserved.
- Integers are digits and must fit `i64`. Reals have a decimal point or an
  exponent (`1.5`, `.5`, `1.`, `1e3`, `2.5E-2`) and must be finite `f64`
  values. Numbers have no sign: `-1` is unary minus applied to `1`, so the
  smallest `i64` is written `-9223372036854775807 - 1`.
- Strings use single quotes, with `''` for a quote, and may span lines.
  `NULL`, `TRUE` and `FALSE` are literals.
- Comments: `-- to end of line` and `/* block */` (not nested).
- Operators: `= <> != < <= > >= + - * / % || ( ) , . ;` (`!=` is `<>`).
- Errors: an unterminated string, quoted identifier or block comment, an
  unknown character, an out-of-range number, or a letter right after a number.

### Statements

Statements are separated by `;`; one trailing `;` is allowed, and empty input
or an empty statement (`;;`) is an error. Square brackets mark optional parts.

```text
CREATE TABLE [IF NOT EXISTS] t (col type [constraint]..., ...)
    type:       INTEGER | REAL | TEXT | BOOLEAN
    constraint: PRIMARY KEY | NOT NULL | UNIQUE   (any order, each at most once)
DROP TABLE [IF EXISTS] t
CREATE [UNIQUE] INDEX i ON t (col)
INSERT INTO t [(col, ...)] VALUES (expr, ...), ...
UPDATE t SET col = expr, ... [WHERE expr]
DELETE FROM t [WHERE expr]
BEGIN | COMMIT | ROLLBACK

SELECT [DISTINCT] item, ...
    [FROM t [AS a] [[INNER | LEFT] JOIN u [AS b] ON expr]...]
    [WHERE expr] [GROUP BY expr, ...] [HAVING expr]
    [ORDER BY expr [ASC | DESC], ...] [LIMIT integer [OFFSET integer]]
    item: * | t.* | expr [AS alias]
```

Clauses must appear in this order. `SELECT` without `FROM` is allowed. Aliases
need `AS`, and `LIMIT` and `OFFSET` take integer literals.

### Expressions

Operators from lowest to highest precedence; binary operators are
left-associative (`a - b - c` is `(a - b) - c`):

| Level | Operators |
|-------|-----------|
| 1 | `OR` |
| 2 | `AND` |
| 3 | `NOT` (prefix) |
| 4 | `= <> != < <= > >=` |
| 5 | `IS [NOT] NULL`, `[NOT] BETWEEN x AND y`, `[NOT] IN (expr, ...)`, `[NOT] LIKE pattern` |
| 6 | `\|\|` |
| 7 | `+ -` |
| 8 | `* / %` |
| 9 | `-` (prefix) |

Operands are literals, columns (`col` or `t.col`), parenthesised expressions,
`CASE WHEN cond THEN result ... [ELSE result] END`, and calls `name()`,
`name(expr, ...)`, `name(*)` and `name(DISTINCT expr, ...)`, such as
`COUNT(*)` and `COUNT(DISTINCT x)`. `BETWEEN` bounds and `LIKE` patterns bind
at the `||` level, so `x BETWEEN 1 AND 2 AND y` is `(x BETWEEN 1 AND 2) AND y`.

An operator never takes an operand that binds more loosely without
parentheses: write `a = (NOT b)` rather than `a = NOT b`, and
`(a IS NULL) || b` rather than `a IS NULL || b`.

Expressions may nest at most 200 levels deep (`MAX_DEPTH`). That counts
parentheses, prefix operators, `CASE`, calls and `IN` lists, and also the
height of the tree, so a chain of more than 200 binary operators
(`1 + 1 + ... + 1`) is rejected too. Deeper input is an error, never a stack
overflow.

### Canonical form

`Display` on any syntax tree type prints SQL that parses back to an equal
tree: upper-case keywords, `<>` for not-equal, `JOIN` for `INNER JOIN`, no
`ASC`, reals always with a `.` or exponent, identifiers quoted only when
needed (`"Order"`, `"select"`), and only the parentheses the tree needs.
`Expr::fully_parenthesized` wraps every operator instead.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

No dependencies outside the Rust standard library are allowed.
