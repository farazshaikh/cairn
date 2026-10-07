//! Runs the end-to-end example in the README "Querying" section and checks
//! that the output shown there is exactly what the database produces.

use std::fs;

use cairn_exec::Database;

mod common;

/// The body of the first fenced block with `language` after `heading`.
fn block_after<'a>(readme: &'a str, heading: &str, language: &str) -> &'a str {
    let section = &readme[readme.find(heading).expect("heading")..];
    let fence = format!("```{language}\n");
    let start = section.find(&fence).expect("opening fence") + fence.len();
    let len = section[start..].find("```").expect("closing fence");
    &section[start..start + len]
}

#[test]
fn readme_querying_example_produces_the_documented_output() {
    let readme = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md"))
        .expect("read README.md");
    let sql = block_after(&readme, "## Querying", "sql");
    let expected = block_after(&readme, "## Querying", "text");
    let path = std::env::temp_dir().join(format!("cairn-readme-{}.db", std::process::id()));
    let _ = fs::remove_file(&path);
    let mut db = Database::create(&path).expect("create");
    let outcome = db
        .execute(sql)
        .map(|results| results.into_iter().map(Ok).collect());
    let actual = common::render(&outcome);
    db.check().expect("check");
    db.close().expect("close");
    let _ = fs::remove_file(&path);
    assert_eq!(actual, expected);
}
