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

Status: scaffold. Each crate is delivered by a milestone.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

No dependencies outside the Rust standard library are allowed.
