//! The fault-injecting file layer (milestone M4 AC6): stop after the Nth
//! write or sync, torn writes, counting across files, and crash images.

use std::path::Path;

use cairn_storage::Vfs;
use cairn_storage::fault::{CrashMode, Event, Fault, FaultVfs, Op};

const A: &str = "/a.db";
const B: &str = "/a.db-wal";

fn write(vfs: &FaultVfs, path: &str, offset: u64, data: &[u8]) -> std::io::Result<()> {
    vfs.open_or_create(Path::new(path))?.write_at(offset, data)
}

fn bytes(vfs: &FaultVfs, path: &str) -> Vec<u8> {
    vfs.read_file(Path::new(path)).expect("file exists")
}

#[test]
fn stop_after_write_applies_that_write_and_refuses_the_rest() {
    let vfs = FaultVfs::new();
    vfs.arm(Fault::StopAfterWrite(3));
    write(&vfs, A, 0, b"1").expect("write 1");
    write(&vfs, B, 0, b"2").expect("write 2");
    write(&vfs, A, 1, b"3").expect("write 3");
    assert!(vfs.stopped());
    assert!(write(&vfs, A, 2, b"4").is_err());
    assert_eq!(bytes(&vfs, A), b"13");
    assert_eq!(vfs.writes(), 3, "the refused write is not counted");
    let mut file = vfs.open(Path::new(A)).expect("open");
    assert!(file.sync().is_err());
    assert!(file.set_len(0).is_err());
    let mut buf = [0u8; 2];
    assert_eq!(file.read_at(0, &mut buf).expect("reads still work"), 2);
    assert!(vfs.create_new(Path::new("/c")).is_err());
}

#[test]
fn stop_after_sync_makes_that_sync_durable() {
    let vfs = FaultVfs::new();
    vfs.arm(Fault::StopAfterSync(2));
    let mut file = vfs.create_new(Path::new(A)).expect("create");
    file.write_at(0, b"ab").expect("write");
    file.sync().expect("sync 1");
    file.write_at(2, b"cd").expect("write");
    file.sync().expect("sync 2");
    assert!(vfs.stopped());
    assert!(file.write_at(4, b"e").is_err());
    let image = vfs.crash(CrashMode::SyncedOnly);
    assert_eq!(bytes(&image, A), b"abcd");
}

#[test]
fn torn_write_keeps_a_strict_prefix_and_fails() {
    for seed in [0u64, 1, 5, 4094, 99_999] {
        let vfs = FaultVfs::new();
        vfs.arm(Fault::TearWrite {
            write: 2,
            keep_seed: seed,
        });
        write(&vfs, A, 0, &[1; 8]).expect("write 1");
        let data = vec![7u8; 4096];
        assert!(write(&vfs, A, 8, &data).is_err(), "torn write fails");
        assert!(vfs.stopped());
        let kept = bytes(&vfs, A).len() - 8;
        assert_eq!(kept, 1 + (seed % 4095) as usize, "seed {seed}");
        assert!((1..4096).contains(&kept));
    }
}

#[test]
fn set_len_counts_as_a_write_across_files() {
    let vfs = FaultVfs::new();
    write(&vfs, A, 0, b"xyz").expect("write");
    vfs.open(Path::new(A))
        .expect("open")
        .set_len(1)
        .expect("set_len");
    write(&vfs, B, 0, b"w").expect("write");
    vfs.open(Path::new(B)).expect("open").sync().expect("sync");
    assert_eq!((vfs.writes(), vfs.syncs()), (3, 1));
    assert_eq!(
        vfs.events(),
        vec![
            Event {
                path: A.into(),
                op: Op::Write { offset: 0, len: 3 }
            },
            Event {
                path: A.into(),
                op: Op::SetLen(1)
            },
            Event {
                path: B.into(),
                op: Op::Write { offset: 0, len: 1 }
            },
            Event {
                path: B.into(),
                op: Op::Sync
            },
        ]
    );
}

#[test]
fn crash_images_keep_writes_or_only_synced_bytes() {
    let vfs = FaultVfs::new();
    let mut file = vfs.create_new(Path::new(A)).expect("create");
    file.write_at(0, b"durable").expect("write");
    file.sync().expect("sync");
    file.write_at(0, b"VOLATILE").expect("write");
    file.set_len(3).expect("truncate");
    vfs.create_new(Path::new(B)).expect("never synced");

    let kept = vfs.crash(CrashMode::KeepWrites);
    assert_eq!(bytes(&kept, A), b"VOL");
    let synced = vfs.crash(CrashMode::SyncedOnly);
    assert_eq!(bytes(&synced, A), b"durable");
    assert_eq!(bytes(&synced, B), b"", "an unsynced new file exists, empty");
    assert_ne!(kept.id(), vfs.id());
    assert_ne!(kept.id(), synced.id());
    assert_eq!((kept.writes(), kept.syncs()), (0, 0));
    assert!(!kept.stopped());
}
