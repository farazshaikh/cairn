//! Runs the example in the README "Shell" section through the `cairn`
//! binary and checks that the output shown there is exactly what it prints.

mod common;

use common::{TempDb, arg, cairn, code, stderr, stdout};

/// The body of the first fenced block with `language` after `heading`.
fn block_after<'a>(readme: &'a str, heading: &str, language: &str) -> &'a str {
    let section = &readme[readme.find(heading).expect("heading")..];
    let fence = format!("```{language}\n");
    let start = section.find(&fence).expect("opening fence") + fence.len();
    let len = section[start..].find("```").expect("closing fence");
    &section[start..start + len]
}

#[test]
fn readme_shell_example_produces_the_documented_output() {
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md"))
        .expect("read README.md");
    let script = block_after(&readme, "## Shell", "sql");
    let expected = block_after(&readme, "## Shell", "text");
    let db = TempDb::new("readme");
    let output = cairn(&[arg("--create"), db.arg()], script);
    assert_eq!(stdout(&output), expected);
    assert_eq!(code(&output), 1, "the example's third statement fails");
    assert_eq!(stderr(&output), "");
}
