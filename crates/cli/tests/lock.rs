//! The exclusive operating-system lock seen from a second process
//! (milestone M5 AC1, AC7): this test process and a `cairn` child.

mod common;

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};

use cairn_exec::{Database, ErrorKind};
use common::{CAIRN, TempDb, arg, cairn, code, stderr};

const LOCKED: &str = "database is busy: the file is locked by another process";

#[test]
fn the_shell_cannot_open_a_file_this_process_holds() {
    let db = TempDb::new("held");
    let mut holder = Database::create(db.path()).expect("create");
    holder
        .execute("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);")
        .expect("setup");
    let main_before = std::fs::read(db.path()).expect("main");
    let log_before = std::fs::read(db.wal()).expect("log");

    let output = cairn(&[db.arg()], "INSERT INTO t VALUES (2);");
    assert_eq!(code(&output), 1);
    let err = stderr(&output);
    assert!(
        err.starts_with(&format!("error: cannot open {}: ", db.path().display()))
            && err.contains(LOCKED),
        "{err}"
    );
    assert_eq!(std::fs::read(db.path()).expect("main"), main_before);
    assert_eq!(std::fs::read(db.wal()).expect("log"), log_before);

    holder.close().expect("close");
    let output = cairn(&[db.arg()], "SELECT a FROM t;");
    assert_eq!(code(&output), 0, "{}", stderr(&output));
}

#[test]
fn a_killed_shell_releases_its_lock() {
    let db = TempDb::new("killed");
    Database::create(db.path())
        .expect("create")
        .close()
        .expect("close");
    let mut child = Command::new(CAIRN)
        .args([arg("--interactive"), db.arg()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut out = BufReader::new(child.stdout.take().expect("stdout"));
    let mut prompt = [0u8; 7];
    out.read_exact(&mut prompt).expect("first prompt");
    assert_eq!(&prompt, b"cairn> ", "the shell has opened the file");

    match Database::open(db.path()) {
        Err(error) => {
            assert_eq!(error.kind(), ErrorKind::Busy);
            assert!(error.to_string().contains("locked by another process"));
        }
        Ok(_) => panic!("opened a file the shell holds"),
    }

    child.kill().expect("kill");
    child.wait().expect("wait");
    let mut rest = String::new();
    let _ = out.read_line(&mut rest);
    Database::open(db.path())
        .expect("open after the holder died")
        .close()
        .expect("close");
}
