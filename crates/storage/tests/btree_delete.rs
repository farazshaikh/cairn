//! B-tree delete with borrow/merge rebalancing and root collapse, free-page
//! reuse, and model-checked mixed workloads (milestone AC2, AC4, AC5, AC7).

mod common;

use std::collections::BTreeMap;
use std::ops::Bound::Unbounded;

use cairn_storage::{BTree, MAX_KEY_LEN, MAX_VALUE_LEN, Pager, StorageError};
use common::{SEED, TempPath, XorShift64, shuffle};

type TestResult = Result<(), StorageError>;
type Pairs = Vec<(Vec<u8>, Vec<u8>)>;

const N: usize = 10_000;

fn key(i: usize) -> Vec<u8> {
    let mut key = format!("key-{i:05}-").into_bytes();
    key.extend(std::iter::repeat_n(b'x', i % 40));
    key
}

fn value(i: usize) -> Vec<u8> {
    vec![(i % 251) as u8; i % 200]
}

fn scan_all(tree: &BTree, pager: &mut Pager) -> Result<Pairs, StorageError> {
    tree.range(pager, Unbounded, Unbounded)?.collect()
}

fn shuffled(n: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    shuffle(&mut order, &mut XorShift64::new(SEED));
    order
}

/// Root page is an empty leaf: tag 1 and a zero cell count.
fn root_is_empty_leaf(tree: &BTree, pager: &mut Pager) -> Result<bool, StorageError> {
    let page = pager.read(tree.root())?;
    Ok(page.bytes()[0] == 1 && page.bytes()[2..4] == [0, 0])
}

#[test]
fn delete_absent_key_returns_false_and_writes_nothing() -> TestResult {
    let tmp = TempPath::new("absent");
    let mut pager = Pager::create_with_capacity(tmp.path(), 8)?;
    let mut tree = BTree::open(BTree::create(&mut pager)?);
    assert!(!tree.delete(&mut pager, b"missing")?, "empty tree");
    for i in 0..1000 {
        tree.insert(&mut pager, &key(i), &value(i))?;
    }
    pager.sync()?;
    let before = scan_all(&tree, &mut pager)?;
    let writes = pager.stats().page_writes;
    assert!(!tree.delete(&mut pager, b"missing")?);
    assert!(!tree.delete(&mut pager, &vec![0u8; MAX_KEY_LEN + 1])?);
    pager.sync()?;
    assert_eq!(pager.stats().page_writes, writes, "no page was dirtied");
    assert_eq!(scan_all(&tree, &mut pager)?, before);
    assert_eq!(tree.check(&mut pager), Ok(()));
    pager.close()
}

#[test]
fn single_deletes_keep_the_tree_valid() -> TestResult {
    let tmp = TempPath::new("each-delete");
    let mut pager = Pager::create_with_capacity(tmp.path(), 8)?;
    let mut tree = BTree::open(BTree::create(&mut pager)?);
    for i in 0..2000 {
        tree.insert(&mut pager, &key(i), &value(i))?;
    }
    for &i in &shuffled(2000) {
        assert!(tree.delete(&mut pager, &key(i))?);
        assert_eq!(tree.get(&mut pager, &key(i))?, None);
        assert_eq!(tree.check(&mut pager), Ok(()), "after deleting {i}");
    }
    assert!(root_is_empty_leaf(&tree, &mut pager)?);
    pager.close()
}

#[test]
fn delete_every_second_key_then_all_keys() -> TestResult {
    let tmp = TempPath::new("phases");
    let mut pager = Pager::create_with_capacity(tmp.path(), 8)?;
    let mut tree = BTree::open(BTree::create(&mut pager)?);
    let order = shuffled(N);
    for &i in &order {
        tree.insert(&mut pager, &key(i), &value(i))?;
    }
    assert_eq!(tree.check(&mut pager), Ok(()));
    let peak = pager.page_count();
    let full_root = tree.root();

    for &i in order.iter().filter(|&&i| i % 2 == 1) {
        assert!(tree.delete(&mut pager, &key(i))?, "delete {i}");
    }
    assert_eq!(
        tree.check(&mut pager),
        Ok(()),
        "after deleting every second key"
    );
    let remaining = scan_all(&tree, &mut pager)?;
    let expected: Pairs = (0..N).step_by(2).map(|i| (key(i), value(i))).collect();
    assert_eq!(remaining, expected);
    for i in (1..N).step_by(2) {
        assert_eq!(tree.get(&mut pager, &key(i))?, None);
    }
    assert!(
        !pager.free_list()?.is_empty(),
        "merges returned pages to the free list"
    );

    pager.set_root("phases", tree.root())?;
    pager.close()?;
    let mut pager = Pager::open_with_capacity(tmp.path(), 8)?;
    let mut tree = BTree::open(pager.root("phases").expect("root persisted"));
    assert_eq!(tree.check(&mut pager), Ok(()), "after reopen");
    assert_eq!(scan_all(&tree, &mut pager)?, expected);

    for &i in order.iter().filter(|&&i| i % 2 == 0) {
        assert!(tree.delete(&mut pager, &key(i))?, "delete {i}");
    }
    assert_eq!(tree.check(&mut pager), Ok(()), "after deleting all keys");
    assert!(scan_all(&tree, &mut pager)?.is_empty());
    assert_eq!(tree.get(&mut pager, &key(0))?, None);
    assert!(root_is_empty_leaf(&tree, &mut pager)?);
    assert_ne!(tree.root(), full_root, "root collapsed to a leaf");
    assert!(!tree.delete(&mut pager, &key(0))?);
    let free = pager.free_list()?;
    assert_eq!(
        free.len() + 2,
        pager.page_count() as usize,
        "every page but the header and root is free"
    );

    for &i in &order {
        tree.insert(&mut pager, &key(i), &value(i))?;
    }
    assert!(pager.page_count() <= peak, "re-insert reused freed pages");
    assert_eq!(tree.check(&mut pager), Ok(()));
    assert_eq!(scan_all(&tree, &mut pager)?.len(), N);
    pager.close()
}

#[test]
fn maximum_size_entries() -> TestResult {
    let tmp = TempPath::new("max-size");
    let mut pager = Pager::create_with_capacity(tmp.path(), 8)?;
    let mut tree = BTree::open(BTree::create(&mut pager)?);
    let big_key = |i: usize| {
        let mut k = format!("{i:06}").into_bytes();
        k.resize(MAX_KEY_LEN, b'k');
        k
    };
    let order = shuffled(2000);
    for (n, &i) in order.iter().enumerate() {
        tree.insert(&mut pager, &big_key(i), &vec![i as u8; MAX_VALUE_LEN])?;
        if n % 100 == 0 {
            assert_eq!(tree.check(&mut pager), Ok(()), "after {n} inserts");
        }
    }
    assert_eq!(tree.check(&mut pager), Ok(()));
    for (n, &i) in order.iter().rev().enumerate() {
        assert!(tree.delete(&mut pager, &big_key(i))?);
        if n % 100 == 0 {
            assert_eq!(tree.check(&mut pager), Ok(()), "after {n} deletes");
        }
    }
    assert_eq!(tree.check(&mut pager), Ok(()));
    assert!(root_is_empty_leaf(&tree, &mut pager)?);
    pager.close()
}

/// Random key of 1..=256 bytes drawn from a 3000-key pool.
fn pool_key(rng: &mut XorShift64) -> Vec<u8> {
    let id = rng.below(3000);
    let mut key = format!("{id:04}").into_bytes();
    let len = 1 + id * 7919 % MAX_KEY_LEN;
    key.resize(len.max(4), (id % 256) as u8);
    key.truncate(len);
    key
}

#[test]
fn mixed_workload_matches_model() -> TestResult {
    let tmp = TempPath::new("mixed");
    let mut pager = Pager::create_with_capacity(tmp.path(), 16)?;
    let mut tree = BTree::open(BTree::create(&mut pager)?);
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut rng = XorShift64::new(SEED ^ 0xABCD);
    for step in 0..20_000 {
        let k = pool_key(&mut rng);
        match rng.below(10) {
            0..=4 => {
                let v = vec![(step % 256) as u8; rng.below(MAX_VALUE_LEN + 1)];
                tree.insert(&mut pager, &k, &v)?;
                model.insert(k, v);
            }
            5..=7 => {
                assert_eq!(
                    tree.delete(&mut pager, &k)?,
                    model.remove(&k).is_some(),
                    "step {step}"
                );
            }
            _ => assert_eq!(
                tree.get(&mut pager, &k)?,
                model.get(&k).cloned(),
                "step {step}"
            ),
        }
        if step % 1000 == 999 {
            assert_eq!(tree.check(&mut pager), Ok(()), "at step {step}");
        }
    }
    let expected: Pairs = model.into_iter().collect();
    assert_eq!(scan_all(&tree, &mut pager)?, expected);
    pager.close()
}
