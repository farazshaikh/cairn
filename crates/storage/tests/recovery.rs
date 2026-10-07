//! Recovery and checkpoints (milestone M4 AC4, AC5): replay of committed
//! transactions at open, ignored incomplete or corrupt tails, idempotent
//! replay, and manual, automatic and close-time checkpoints.

mod common;

use std::path::Path;
use std::sync::Arc;

use cairn_storage::fault::{CrashMode, Fault, FaultVfs, Op};
use cairn_storage::{
    DEFAULT_CHECKPOINT_FRAMES, Options, PAGE_SIZE, Page, PageId, Pager, StorageError,
};

type TestResult = Result<(), StorageError>;

const DB: &str = "/rec.db";
const WAL: &str = "/rec.db-wal";

fn options() -> Options {
    Options::new(8, DEFAULT_CHECKPOINT_FRAMES)
}

fn open(vfs: &FaultVfs) -> Result<Pager, StorageError> {
    Pager::open_with(Arc::new(vfs.clone()), Path::new(DB), options())
}

fn filled(byte: u8) -> Page {
    let mut page = Page::zeroed();
    page.bytes_mut().fill(byte);
    page
}

/// Every page and the page count, as the handle sees them.
fn dump(pager: &mut Pager) -> Result<(u32, Vec<Page>), StorageError> {
    let count = pager.page_count();
    let pages = (1..count)
        .map(|id| pager.read(PageId(id)))
        .collect::<Result<_, _>>()?;
    Ok((count, pages))
}

/// A database with `txns` committed transactions still in the log: each
/// allocates one page and rewrites page 1 with the transaction number.
fn logged(txns: u8) -> Result<(FaultVfs, Pager), StorageError> {
    let vfs = FaultVfs::new();
    let mut pager = Pager::create_with(Arc::new(vfs.clone()), Path::new(DB), options())?;
    pager.allocate()?;
    pager.commit()?;
    for n in 1..=txns {
        let id = pager.allocate()?;
        pager.write(id, &filled(n))?;
        pager.write(PageId(1), &filled(n))?;
        pager.commit()?;
    }
    Ok((vfs, pager))
}

#[test]
fn committed_transactions_survive_a_power_loss() -> TestResult {
    let (vfs, pager) = logged(5)?;
    let image = vfs.crash(CrashMode::SyncedOnly);
    drop(pager);
    assert_eq!(
        image.read_file(Path::new(DB)).map(|b| b.len()),
        Some(PAGE_SIZE),
        "nothing was checkpointed before the crash"
    );
    let mut pager = open(&image)?;
    assert_eq!(pager.page_count(), 7);
    assert_eq!(pager.read(PageId(1))?, filled(5));
    for n in 1..=5u8 {
        assert_eq!(pager.read(PageId(1 + u32::from(n)))?, filled(n));
    }
    assert_eq!(
        image.read_file(Path::new(WAL)).map(|b| b.len()),
        Some(0),
        "replay emptied the log"
    );
    assert_eq!(
        image.read_file(Path::new(DB)).map(|b| b.len()),
        Some(7 * PAGE_SIZE)
    );
    pager.close()
}

#[test]
fn uncommitted_and_torn_tails_are_ignored() -> TestResult {
    let (vfs, mut pager) = logged(3)?;
    pager.begin()?;
    pager.write(PageId(1), &filled(0xEE))?;
    let unfinished = vfs.crash(CrashMode::KeepWrites);
    let mut reopened = open(&unfinished)?;
    assert_eq!(
        reopened.read(PageId(1))?,
        filled(3),
        "the open transaction never reached the log"
    );
    reopened.close()?;

    vfs.arm(Fault::TearWrite {
        write: vfs.writes() + 2,
        keep_seed: 1000,
    });
    assert!(pager.commit().is_err(), "the torn frame fails the commit");
    let torn = vfs.crash(CrashMode::KeepWrites);
    let mut reopened = open(&torn)?;
    assert_eq!(reopened.read(PageId(1))?, filled(3));
    assert_eq!(reopened.page_count(), 5);
    reopened.close()?;
    drop(pager);
    Ok(())
}

#[test]
fn a_corrupt_last_transaction_is_dropped_and_earlier_ones_kept() -> TestResult {
    let (vfs, pager) = logged(4)?;
    let image = vfs.crash(CrashMode::SyncedOnly);
    drop(pager);
    let mut log = image.read_file(Path::new(WAL)).expect("log");
    let last_commit = log.len() - 32;
    let flip = last_commit - 100;
    log[flip] ^= 0x40;
    image.write_file(Path::new(WAL), log);
    let mut pager = open(&image)?;
    assert_eq!(pager.read(PageId(1))?, filled(3), "transaction 4 is gone");
    assert_eq!(pager.page_count(), 5);
    assert_eq!(pager.read(PageId(4))?, filled(3));
    pager.close()
}

#[test]
fn checkpoint_copies_syncs_then_truncates() -> TestResult {
    let (vfs, mut pager) = logged(3)?;
    let start = vfs.events().len();
    assert_eq!(pager.checkpoint()?, 5, "pages 0..=4 were logged");
    let ops: Vec<(bool, Op)> = vfs.events()[start..]
        .iter()
        .map(|e| (e.path == Path::new(WAL), e.op))
        .collect();
    let first_main = ops.iter().position(|(log, _)| !log).expect("main write");
    assert_eq!(
        ops[first_main - 1],
        (true, Op::Sync),
        "the log is synced first"
    );
    let tail: Vec<(bool, Op)> = ops[ops.len() - 4..].to_vec();
    assert_eq!(
        tail,
        vec![
            (false, Op::SetLen(5 * PAGE_SIZE as u64)),
            (false, Op::Sync),
            (true, Op::SetLen(0)),
            (true, Op::Sync),
        ]
    );
    assert_eq!(pager.log_frames(), 0);
    assert_eq!(pager.checkpoint()?, 0, "nothing left to copy");
    let image = vfs.crash(CrashMode::SyncedOnly);
    let mut reopened = open(&image)?;
    assert_eq!(dump(&mut reopened)?, dump(&mut pager)?);
    reopened.close()?;
    pager.close()
}

#[test]
fn a_commit_past_1000_frames_checkpoints_automatically() -> TestResult {
    let vfs = FaultVfs::new();
    let mut pager = Pager::create_with(Arc::new(vfs.clone()), Path::new(DB), options())?;
    let mut checkpointed = false;
    for round in 0..40u8 {
        for _ in 0..30 {
            let id = pager.allocate()?;
            pager.write(id, &filled(round))?;
        }
        let before = pager.log_frames();
        pager.commit()?;
        let after = pager.log_frames();
        assert!(
            after <= DEFAULT_CHECKPOINT_FRAMES,
            "round {round}: {after} frames left"
        );
        if after < before {
            assert!(
                before + 31 > DEFAULT_CHECKPOINT_FRAMES,
                "checkpoint only above the threshold"
            );
            assert_eq!(after, 0);
            checkpointed = true;
        }
    }
    assert!(checkpointed);
    assert_eq!(pager.stats().checkpoints, 1);
    pager.close()
}

#[test]
fn close_checkpoints_and_missing_or_empty_logs_open() -> TestResult {
    let (vfs, pager) = logged(2)?;
    pager.close()?;
    assert_eq!(vfs.read_file(Path::new(WAL)).map(|b| b.len()), Some(0));
    let mut reopened = open(&vfs)?;
    assert_eq!(reopened.read(PageId(1))?, filled(2));
    reopened.close()?;

    let without_log = FaultVfs::new();
    without_log.write_file(Path::new(DB), vfs.read_file(Path::new(DB)).expect("main"));
    let mut reopened = open(&without_log)?;
    assert_eq!(reopened.read(PageId(3))?, filled(2));
    reopened.close()?;
    assert!(
        without_log.read_file(Path::new(WAL)).is_none(),
        "a database that commits nothing never creates a log"
    );
    Ok(())
}

/// Regression: a crash part-way through a checkpoint can leave the main
/// file's header page newer than the log's first transaction. Recovery must
/// still replay the whole log rather than distrust it.
#[test]
fn a_crash_at_every_write_of_a_checkpoint_recovers_the_committed_state() -> TestResult {
    let (_, mut reference) = logged(6)?;
    let expected = dump(&mut reference)?;
    drop(reference);
    let mut positions = 0;
    for n in 1.. {
        let (vfs, mut pager) = logged(6)?;
        vfs.arm(Fault::StopAfterWrite(vfs.writes() + n));
        let _ = pager.checkpoint();
        if !vfs.stopped() {
            break;
        }
        positions += 1;
        for mode in [CrashMode::KeepWrites, CrashMode::SyncedOnly] {
            let mut reopened = open(&vfs.crash(mode))?;
            assert_eq!(dump(&mut reopened)?, expected, "write {n}, {mode:?}");
        }
        drop(pager);
    }
    assert!(positions >= 8, "the checkpoint made {positions} writes");
    Ok(())
}

/// Crashing at every write and every sync of recovery, in both crash modes,
/// then reopening cleanly gives exactly the state of an uninterrupted
/// recovery.
#[test]
fn replay_is_idempotent_under_crashes_at_every_step() -> TestResult {
    let (vfs, pager) = logged(6)?;
    let image = vfs.crash(CrashMode::SyncedOnly);
    drop(pager);
    let mut clean = open(&image.crash(CrashMode::KeepWrites))?;
    let expected = dump(&mut clean)?;
    drop(clean);
    let mut positions = 0;
    for fault in [
        Fault::StopAfterWrite as fn(u64) -> Fault,
        Fault::StopAfterSync,
    ] {
        for n in 1.. {
            let attempt = image.crash(CrashMode::KeepWrites);
            attempt.arm(fault(n));
            drop(open(&attempt));
            if !attempt.stopped() {
                break;
            }
            positions += 1;
            for mode in [CrashMode::KeepWrites, CrashMode::SyncedOnly] {
                let mut reopened = open(&attempt.crash(mode))?;
                assert_eq!(
                    dump(&mut reopened)?,
                    expected,
                    "{:?} at {n}, {mode:?}",
                    fault(n)
                );
            }
        }
    }
    assert!(
        positions >= 10,
        "recovery performed {positions} writes and syncs"
    );
    Ok(())
}
