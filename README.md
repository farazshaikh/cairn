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

Status: `crates/storage` provides the pager, buffer pool, B-tree and
write-ahead log (see [Storage](#storage) and
[Transactions and durability](#transactions-and-durability)), `crates/sql`
the tokenizer and parser (see [SQL](#sql)), and `crates/exec` the catalog,
planner, executor and SQL transactions (see [Querying](#querying)).
`crates/cli` is a scaffold delivered by a later milestone.

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

Each `Pager` caches committed pages in a bounded pool. Its capacity is chosen
at open (`create_with_capacity` / `open_with_capacity`; at least 8 pages,
default 256). The least recently used unpinned page is evicted. The pool
never writes: uncommitted changes live in the transaction overlay and reach
the file only through the write-ahead log (see
[Transactions and durability](#transactions-and-durability)). `pin` /
`unpin` keep a page resident; pins nest. If every frame is pinned and
another page must be loaded, the call returns `StorageError::PoolExhausted`
and leaves the pager unchanged. `stats`, `resident` and `is_cached` expose
the pool state.

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
  `create_with` / `open_with` (a `Vfs` and `Options`), `allocate`, `read`,
  `write`, `free`, `page_count`, `begin`, `commit`, `rollback`,
  `in_transaction`, `savepoint`, `rollback_to`, `release`, `begin_read`,
  `end_read`, `snapshot_seq`, `checkpoint`, `log_frames`, `sync`, `close`,
  `pin`, `unpin`, `free_list`, `set_root`, `root`, `remove_root` (now
  returns `Result<bool>`, because it starts a transaction), `roots`, and the
  pool accessors above. `commit` returns the sequence number of the state it
  committed; `snapshot_seq` returns the latest one, which another handle may
  already have moved past, so state cached per commit (like the exec
  catalog) must use the value `commit` returned.
- `BTree`: `create`, `open`, `root`, `get`, `insert`, `delete`, `range`,
  `check`, `pages`. A tree is a small handle and every method takes
  `&mut Pager`, so several trees can share one file. `range` is lazy, and
  inverted or empty bounds yield nothing. `pages` lists the tree's pages for
  page accounting.
- `StorageError`: `Io`, `FileTooShort`, `NotPageMultiple`, `BadMagic`,
  `UnsupportedVersion`, `Corrupt { page, reason }`, `PoolExhausted`,
  `PoolTooSmall`, `TooLarge`, `InvalidPage`, `DatabaseFull`,
  `InvalidRootName`, `RootTableFull`, `Busy { reason }`, `NoTransaction`,
  `TransactionOpen`, `InvalidSavepoint`, `Unusable`.
- `Vfs` / `VfsFile`: the file layer every access goes through. `OsVfs` is
  the real file system; `cairn_storage::fault::FaultVfs` is the in-memory,
  fault-injecting test double.

`sync` commits the open transaction, if any, and then checkpoints, so
afterwards the main file alone holds the database. Dropping a `Pager` commits
an implicit transaction, rolls back an explicit one (`begin`) and
checkpoints, ignoring errors, so call `close` to see them.

Not yet provided: file locking or access from several processes, overflow
pages for larger keys or values, and checksums on main-file pages (log
records are checksummed).

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

## Querying

`cairn-exec` runs SQL against a database file. It keeps a catalog of tables
and indexes in the file, plans each query with simple rules, and executes it
over the storage B-trees.

```rust
use cairn_exec::{Database, QueryResult};

let mut db = Database::create("library.cairn")?;   // Database::open for an existing file
let results = db.execute(SQL)?;                     // one QueryResult per statement
if let Some(QueryResult::Rows { columns, rows }) = results.last() {
    // columns: Vec<String>, rows: Vec<Vec<Value>>
}
db.close()?;                                        // sync and report errors
```

With this input as `SQL`:

```sql
CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE books (title TEXT NOT NULL, author_id INTEGER, year INTEGER, rating REAL);
CREATE INDEX books_author ON books (author_id);
INSERT INTO authors VALUES (1, 'Le Guin'), (2, 'Calvino'), (3, 'Borges');
INSERT INTO books VALUES ('The Dispossessed', 1, 1974, 4.5), ('Earthsea', 1, 1968, 4), ('Invisible Cities', 2, 1972, NULL);
SELECT a.name, COUNT(b.title) AS books, AVG(b.rating) AS rating
    FROM authors AS a LEFT JOIN books AS b ON b.author_id = a.id
    GROUP BY a.name ORDER BY books DESC, a.name;
UPDATE books SET rating = 4.2 WHERE title = 'Invisible Cities';
EXPLAIN SELECT title FROM books WHERE author_id = 1;
```

the statements produce the following results, written in the golden-test
format described below. The test `crates/exec/tests/readme_example.rs` runs
this example and checks the output.

```text
affected 0

affected 0

affected 0

affected 3

affected 3

name	books	rating
Le Guin	2	4.25
Calvino	1	NULL
Borges	0	NULL

affected 1

step	depth	operation	detail
0	0	PROJECT	title
1	1	FILTER	author_id = 1
2	2	INDEX LOOKUP	books USING books_author (author_id = 1)
```

### API

- `Database::create(path)` makes a new file (it fails if the file exists).
  `Database::open(path)` opens an existing one, replaying its write-ahead
  log, and `close` checkpoints it. `create_with` / `open_with` take a `Vfs`
  and `Options`, for example the fault-injecting test double.
- `execute(sql)` parses the whole input first, so a syntax error runs
  nothing. It then runs the statements in order, returns one `QueryResult` per
  statement, and stops at the first error. Statements that already ran keep
  their effect (committed, or pending inside an open transaction).
  `execute_each(sql)` keeps going after an error and returns each
  statement's `Result`.
- `QueryResult` is `Rows { columns, rows }` for `SELECT`, `EXPLAIN` and
  `PRAGMA`, or `Affected(n)`, which counts rows for `INSERT`, `UPDATE` and
  `DELETE` and is 0 for DDL, `BEGIN`, `COMMIT` and `ROLLBACK`. A `Value` is
  `Null`, `Integer(i64)`, `Real(f64)`, `Text(String)` or `Boolean(bool)`.
- `ExecError` has a `kind()` (`Syntax`, `Storage`, `Corrupt`, `NotFound`,
  `AlreadyExists`, `Type`, `Constraint`, `Arithmetic`, `Unsupported`,
  `TooLarge`, `Unusable`, `Busy` or `Transaction`), a `message()`, and, when
  the error relates to SQL text, a `span()` and `source_line()`. With a span,
  `Display` uses the same three-line caret form as `SqlError`.
- `check()` returns the first problem that `PRAGMA integrity_check` would
  list (see [Transactions and durability](#transactions-and-durability)).
- Statements outside `BEGIN` commit one by one (autocommit); see
  [Transactions and durability](#transactions-and-durability).

### Types

Columns are `INTEGER`, `REAL`, `TEXT` or `BOOLEAN`, and types are strict.
The only automatic conversion is that an `INTEGER` value stored in a `REAL`
column becomes `REAL`. Any other mismatch is an error, such as `'1'` in an
`INTEGER` column or `1` in a `BOOLEAN` column. Types are checked before any
row is read, so `SELECT 'a' + 1 FROM empty_table` fails too.

- Comparisons work between two numbers (`INTEGER` and `REAL` compare by exact
  numeric value), two `TEXT` values (by bytes, which is code-point order) or
  two `BOOLEAN` values (`FALSE < TRUE`). Comparing `TEXT` with `INTEGER` is
  an error.
- `+ - * /` need numbers. Two `INTEGER` operands give an `INTEGER`;
  otherwise the result is `REAL`. Integer `/` truncates toward zero, and `%`
  works only on integers and takes the sign of the dividend.
- `||` and `LIKE` need `TEXT`. `WHERE`, `ON`, `HAVING`, `CASE WHEN`, `NOT`,
  `AND` and `OR` need `BOOLEAN`, so `WHERE 1` is an error.
- `CASE` branches and `COALESCE` arguments must share a type. `INTEGER` and
  `REAL` together give `REAL`.
- Integer overflow (including `ABS` and `SUM`), a non-finite real result and
  division or modulo by zero are errors, never panics or wrapped values.
  Constant expressions in an indexable predicate (`id = 1 / 0`) are evaluated
  once while planning, even if the table is empty.

### NULL

- Comparisons, arithmetic, `||`, `LIKE`, `BETWEEN` and every function except
  `COALESCE` return `NULL` when an input is `NULL`.
- `AND`, `OR` and `NOT` use three-valued logic. `x IN (...)` is `NULL` when
  nothing matches and `x` or a list item is `NULL`. `IS [NOT] NULL` is never
  `NULL`.
- `WHERE`, `ON` and `HAVING` keep a row only when the condition is `TRUE`. A
  `CASE` branch with a `NULL` condition is skipped, and `CASE` without a
  matching branch or `ELSE` is `NULL`.
- One ordering is used everywhere: `NULL` first, then values. `ORDER BY ...
  ASC` puts `NULL`s first and `DESC` puts them last. Sorting is stable.
  `DISTINCT` and `GROUP BY` treat `NULL`s as equal. `UNIQUE` treats them as
  distinct, so a `UNIQUE` column may hold many `NULL`s.
- Aggregates skip `NULL`s. `COUNT` of no rows is 0. `SUM`, `AVG`, `MIN` and
  `MAX` of no rows (or only `NULL`s) are `NULL`. An aggregate query without
  `GROUP BY` always returns one row.

### Functions

| Function | Arguments | Result |
|----------|-----------|--------|
| `LOWER(t)`, `UPPER(t)` | `TEXT` | `TEXT` (Unicode case mapping) |
| `LENGTH(t)` | `TEXT` | `INTEGER`, in characters |
| `ABS(n)` | number | same type |
| `COALESCE(a, ...)` | one or more of a common type | first non-`NULL` argument |
| `COUNT(*)`, `COUNT(x)`, `COUNT(DISTINCT x)` | any | `INTEGER` |
| `SUM(n)`, `AVG(n)` | number | `SUM`: same type; `AVG`: `REAL` |
| `MIN(x)`, `MAX(x)` | any | same type |

`DISTINCT` is allowed in any aggregate. Aggregates are not allowed in
`WHERE`, `ON`, `GROUP BY`, `VALUES` or `SET`, and cannot be nested. In a
grouped query, a column must be inside an aggregate or appear in
`GROUP BY`. An output column takes its alias, else the bare column name,
else the canonical SQL of its expression (`count(*)`, `a + 1`). `ORDER BY`
accepts an output position (`ORDER BY 2`), an output alias or an
expression. With `SELECT DISTINCT`, every `ORDER BY` item must be in the
select list.

### Keys, constraints and atomicity

- A table with an `INTEGER PRIMARY KEY` column is a B-tree keyed by that
  column. Any other table is keyed by a hidden row id from a saved counter
  that starts at 1 and never reuses ids. `PRIMARY KEY` implies `NOT NULL`.
  `NULL` in an `INTEGER PRIMARY KEY` is an error rather than a request for a
  new key.
- Each `UNIQUE` column and each non-integer `PRIMARY KEY` gets an implicit
  unique index named `cairn_autoindex_<table>_<column>`. The name is cut to
  the 64-byte name limit, with `_2`, `_3`, ... added if it would clash with
  an existing index. User index names may not start with `cairn_`.
- `CREATE [UNIQUE] INDEX` fills the new index from the existing rows. A
  unique index on a column that already has duplicate values is an error and
  creates nothing. Every insert, update and delete keeps all indexes up to
  date.
- A statement that fails changes nothing. Every write statement first works
  out all of its new rows and index entries and checks types, `NOT NULL`,
  `PRIMARY KEY`, `UNIQUE` and the storage size limits. Only then does it
  write. Uniqueness is judged on the state after the statement, so
  `UPDATE t SET id = id + 1` works.
- Limits: an encoded row may be at most 1024 bytes and an index key at most
  256 bytes. Larger ones are `TooLarge` errors. Names are 1 to 64 bytes, and
  a table has at most 255 columns.
- A storage error in the middle of a write is undone with the rest of the
  statement: the statement runs in its own transaction, or under a savepoint
  inside `BEGIN`.
- `DROP TABLE` deletes the table's and its indexes' B-trees and returns all
  of their pages to the free list for reuse.

### Planner and EXPLAIN

For the first table in `FROM`, the planner uses the first of these that
applies:

1. a primary-key lookup for `id = c`;
2. a primary-key range for `<`, `<=`, `>`, `>=` or `BETWEEN`, combining all
   such conditions;
3. an index lookup for `col = c`;
4. an index range;
5. a full scan.

Here `c` is a constant expression and conditions are joined by `AND`.
Indexes are tried in name order. An index is not used when the constant's
type does not match the column (for example `int_col = 1.5`). The whole
`WHERE` clause is still applied to every row the access path returns. For
each joined table, if `ON` has `inner.col = <outer expression>` and that
column is the primary key or indexed, the join probes it for each outer row
(`INDEX JOIN`). Otherwise it is a nested loop over a scan. `UPDATE` and
`DELETE` choose rows the same way.

`EXPLAIN <select>` returns one row per plan step, with the columns `step`,
`depth`, `operation` and `detail`. The operations are `LIMIT`, `DISTINCT`,
`PROJECT`, `SORT`, `FILTER`, `AGGREGATE`, `NESTED LOOP JOIN`, `INDEX JOIN`,
`SCAN`, `PRIMARY KEY LOOKUP`, `PRIMARY KEY RANGE`, `INDEX LOOKUP`,
`INDEX RANGE` and `VALUES`. `EXPLAIN` is recognised only at the start of a
statement and only before `SELECT`. An `EXPLAIN` with no statement after it
is a syntax error, and nothing in the input runs. A table named `"explain"`
still works.

### File format

The catalog is a B-tree whose root is the header root slot `catalog`. It
holds versioned entries for tables, columns and indexes (catalog version 1).
Rows are stored as version-1 records: a version byte, a column count, then
one type-tagged value per column. Index keys are the column value, encoded
so that byte order equals SQL order with `NULL` first, followed by the row
key. The exact byte layouts are offset tables in `crates/exec/src/catalog.rs`,
`codec/record.rs` and `codec/key.rs`.

### Golden tests

`tests/sql/NNN_name.sql` scripts are paired with `NNN_name.expected` files.
`crates/exec/tests/golden.rs` runs each script against a fresh database and
compares the output byte for byte. It needs at least 40 scripts, and its
module documentation maps each milestone rule to the scripts that cover it.
Each statement produces one block, and blocks are separated by a blank line:

- rows: a tab-separated header line, then one line per row. Values print as
  `NULL`, `TRUE`/`FALSE`, decimal integers, reals in canonical form (`1.0`,
  `1e300`), and raw text with `\`, tab, newline and carriage return escaped;
- `affected N`;
- `error: line L, column C: message`, or `error: message` without a span.

To add a script, write the `.sql` file and its `.expected` file by hand from
the rules above. `CAIRN_BLESS=1 cargo test -p cairn-exec --test golden`
rewrites every `.expected` file from the actual output. Review those diffs
against the rules before committing them.

## Transactions and durability

Every change to a database file goes through a write-ahead log stored beside
it as `<file>-wal`. The log is created by the first commit and never deleted;
checkpoints truncate it to zero bytes.

### SQL transactions

- `BEGIN` starts a transaction, and `COMMIT` or `ROLLBACK` ends it. All three
  return `affected 0`. `ROLLBACK` discards every change since `BEGIN`,
  catalog changes included: a table created inside the transaction is gone,
  a dropped one is back, and the page count and free list are as before.
- Outside `BEGIN`, every statement commits on its own (autocommit).
- Inside `BEGIN`, a statement that fails is undone on its own, and the
  transaction stays open with the earlier statements' changes.
- `BEGIN` inside a transaction, and `COMMIT` or `ROLLBACK` without one, are
  `Transaction` errors that change nothing.
- Closing or dropping a `Database` with an open transaction discards it.
- Handles on the same file in one process share its committed state. A
  handle sees only committed changes from other handles, and each statement
  outside a transaction sees one committed state for its whole run. One
  handle at a time may write: `BEGIN`, or an autocommit write, on another
  handle while a transaction is open fails at once with a `Busy` error.
  There is no waiting and no read-only transaction, so callers retry.

### Log format

All integers are little-endian. The log starts with a 32-byte header:

| Offset | Size | Field |
|--------|------|-------|
| 0      | 8    | magic `cairnwal` |
| 8      | 4    | log format version, `u32` = 1 |
| 12     | 4    | page size, `u32` = 4096 |
| 16     | 8    | salt, `u64`; new for every log generation |
| 24     | 4    | reserved, zero |
| 28     | 4    | CRC of bytes 0..28 |

A commit appends one page frame for every page the transaction changed, in
ascending page order. Page 0 is included whenever the header (page count,
free list or roots) changed. Then comes a commit record:

| Offset | Size | Page frame (4128 bytes) | Commit record (32 bytes) |
|--------|------|-------------------------|--------------------------|
| 0      | 4    | type = 1                | type = 2 |
| 4      | 4    | page id                 | page count after the commit |
| 8      | 8    | transaction id          | transaction id |
| 16     | 8    | salt                    | salt |
| 24     | 4    | reserved, zero          | number of frames in the transaction |
| 28     | 4    | CRC                     | CRC |
| 32     | 4096 | full page image         | — |

The CRC is CRC-32/ISO-HDLC (the zlib CRC, check value `0xCBF43926`). Each
record's CRC continues from the previous record's CRC (the first from the
header's) and covers the record's first 28 bytes and, for a frame, the page
image. Transaction ids start at 1 after each truncation and increase by
exactly 1. A new log generation takes the previous salt plus one. So a
record left over from an older generation, or moved to another position,
cannot validate.

### Commit, checkpoint and recovery

- **Commit.** A transaction's changed pages stay in its handle's memory until
  commit. Commit writes the frames and the commit record, syncs the log, and
  only then publishes the new state to other handles. Pages reach the main
  file only through a checkpoint or recovery, and both sync the log before
  their first main-file write. So a changed page never reaches the main file
  before its commit record is durable.
- **Checkpoint.** A checkpoint syncs the log, copies the latest committed
  image of every logged page into the main file, sets the file length from
  the committed page count, syncs the main file, then truncates the log and
  syncs it. It runs:
  - automatically after a commit that leaves more than 1000 frames in the
    log (`Options::checkpoint_frames`);
  - on every clean close;
  - on `PRAGMA checkpoint`, which returns one row `ok` in column
    `checkpoint`.

  It is skipped, or for `PRAGMA checkpoint` refused with `Busy`, while
  another statement is reading. Inside a transaction `PRAGMA checkpoint` is a
  `Transaction` error.
- **Recovery.** The first open of a file in a process scans the log. It
  replays every complete transaction whose records all validate, in order,
  by copying its pages into the main file as a checkpoint does, and stops at
  the first incomplete, torn or corrupt record. Recovery is idempotent: it
  empties the log only after the main file is synced, so a crash during
  recovery is repaired by the next open.

**Crash guarantee.** After a crash, reopening the database yields exactly
the state after some committed transaction, never a partial one. That state
includes every transaction whose `COMMIT` (or autocommit statement) returned
`Ok`. A commit that returned an error may or may not be present: if the
failure was the log sync itself, the file refuses further statements
(`Unusable`) until every handle is closed and it is reopened.

The crash tests (`crates/exec/tests/crash.rs`) check this guarantee.
`cairn_storage::fault::FaultVfs` is an in-memory file layer that can:
- stop after the Nth write or sync;
- tear a write to a byte prefix;
- rebuild the files either as a crashed process leaves them (all writes) or
  as a power loss does (synced bytes only).

A fixed-seed workload of 230 committed transactions crashes at every write
and every sync. After each crash the test reopens the database and compares
it with a model of the committed prefixes, then runs `check()` and
`PRAGMA integrity_check`.

### PRAGMA integrity_check

`PRAGMA integrity_check` returns one row `ok` in column `integrity_check`, or
one row per problem (at most 100, then `and N more problems`). It checks:
- every catalog, table and index B-tree with `check()`;
- that no page belongs to two trees;
- record decoding, primary keys and row ids;
- that every index has exactly one entry per row and unique indexes hold no
  duplicates;
- that no page is both on the free list and in use, and that no page is
  neither (leaked).

`Database::check()` returns the first such problem as a `Corrupt` error.
`PRAGMA` is recognised before parsing, like `EXPLAIN`: `PRAGMA name` must be
a whole statement, and an unknown name is an `Unsupported` error.

### Limits

- A transaction's changed pages are held in memory (4 KiB each) until
  commit.
- Only one process may use a file. There is no operating-system lock, and
  two processes on one file will corrupt it.
- `create` empties an orphan `<file>-wal`. Otherwise a log left beside a
  file is assumed to belong to it.
- A build from before the log (milestone 3) ignores a non-empty `-wal`; a
  cleanly closed file has an empty one.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

No dependencies outside the Rust standard library are allowed.
