//! B-tree insert, replace, lookup, ordered range scans and the 10 000-key
//! shuffled workload (milestone AC4, AC5, AC7).

mod common;

use std::collections::BTreeMap;
use std::ops::Bound::{self, Excluded, Included, Unbounded};

use cairn_storage::{BTree, MAX_KEY_LEN, MAX_VALUE_LEN, PageId, Pager, SizeKind, StorageError};
use common::{SEED, TempPath, XorShift64, shuffle};

type TestResult = Result<(), StorageError>;
type Pairs = Vec<(Vec<u8>, Vec<u8>)>;
type OwnedBounds = (Bound<Vec<u8>>, Bound<Vec<u8>>);
type Bounds<'a> = (Bound<&'a [u8]>, Bound<&'a [u8]>);

const N: usize = 10_000;

fn key(i: usize) -> Vec<u8> {
    let mut key = format!("key-{i:05}-").into_bytes();
    key.extend(std::iter::repeat_n(b'x', i % 40));
    key
}

fn value(i: usize) -> Vec<u8> {
    vec![(i % 251) as u8; i % 200]
}

fn new_tree(tmp: &TempPath, capacity: usize) -> Result<(Pager, BTree), StorageError> {
    let mut pager = Pager::create_with_capacity(tmp.path(), capacity)?;
    let root = BTree::create(&mut pager)?;
    Ok((pager, BTree::open(root)))
}

fn scan(
    tree: &BTree,
    pager: &mut Pager,
    start: Bound<&[u8]>,
    end: Bound<&[u8]>,
) -> Result<Pairs, StorageError> {
    tree.range(pager, start, end)?.collect()
}

fn scan_all(tree: &BTree, pager: &mut Pager) -> Result<Pairs, StorageError> {
    scan(tree, pager, Unbounded, Unbounded)
}

fn root_is_internal(tree: &BTree, pager: &mut Pager) -> Result<bool, StorageError> {
    Ok(pager.read(tree.root())?.bytes()[0] == 2)
}

#[test]
fn empty_tree() -> TestResult {
    let tmp = TempPath::new("empty-tree");
    let (mut pager, tree) = new_tree(&tmp, 8)?;
    assert_eq!(tree.get(&mut pager, b"anything")?, None);
    assert!(scan_all(&tree, &mut pager)?.is_empty());
    assert_eq!(tree.check(&mut pager), Ok(()));
    let reopened = BTree::open(tree.root());
    assert_eq!(reopened.get(&mut pager, b"")?, None);
    pager.close()
}

#[test]
fn insert_get_and_replace() -> TestResult {
    let tmp = TempPath::new("replace");
    let (mut pager, mut tree) = new_tree(&tmp, 8)?;
    tree.insert(&mut pager, b"k", b"v1")?;
    tree.insert(&mut pager, b"other", b"o")?;
    tree.insert(&mut pager, b"k", b"v2")?;
    assert_eq!(tree.get(&mut pager, b"k")?, Some(b"v2".to_vec()));
    let all = scan_all(&tree, &mut pager)?;
    assert_eq!(all.iter().filter(|(k, _)| k == b"k").count(), 1);
    assert_eq!(all.len(), 2);
    tree.insert(&mut pager, b"", b"")?;
    assert_eq!(tree.get(&mut pager, b"")?, Some(Vec::new()));
    assert_eq!(tree.check(&mut pager), Ok(()));
    pager.close()
}

#[test]
fn too_large_keys_and_values_are_rejected() -> TestResult {
    let tmp = TempPath::new("toolarge");
    let (mut pager, mut tree) = new_tree(&tmp, 8)?;
    let max_key = vec![7u8; MAX_KEY_LEN];
    let max_value = vec![8u8; MAX_VALUE_LEN];
    tree.insert(&mut pager, &max_key, &max_value)?;
    assert_eq!(tree.get(&mut pager, &max_key)?, Some(max_value.clone()));

    let page_count = pager.page_count();
    assert!(matches!(
        tree.insert(&mut pager, &vec![1u8; MAX_KEY_LEN + 1], b"v"),
        Err(StorageError::TooLarge {
            kind: SizeKind::Key,
            len: 257,
            max: 256
        })
    ));
    assert!(matches!(
        tree.insert(&mut pager, b"k", &vec![1u8; MAX_VALUE_LEN + 1]),
        Err(StorageError::TooLarge {
            kind: SizeKind::Value,
            len: 1025,
            max: 1024
        })
    ));
    assert!(matches!(
        tree.insert(&mut pager, &max_key, &vec![1u8; MAX_VALUE_LEN + 1]),
        Err(StorageError::TooLarge { .. })
    ));
    assert_eq!(pager.page_count(), page_count);
    assert_eq!(scan_all(&tree, &mut pager)?, [(max_key.clone(), max_value)]);
    assert_eq!(tree.get(&mut pager, &vec![1u8; MAX_KEY_LEN + 1])?, None);
    assert_eq!(tree.check(&mut pager), Ok(()));
    pager.close()
}

#[test]
fn keys_compare_as_unsigned_bytes() -> TestResult {
    let tmp = TempPath::new("unsigned");
    let (mut pager, mut tree) = new_tree(&tmp, 8)?;
    let keys: [&[u8]; 6] = [&[0x7F], &[0x80], &[0xFF], &[], &[0x00], &[0x00, 0x00]];
    for k in keys {
        tree.insert(&mut pager, k, k)?;
    }
    let order: Vec<Vec<u8>> = scan_all(&tree, &mut pager)?
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    let expected: Vec<Vec<u8>> = vec![
        vec![],
        vec![0x00],
        vec![0x00, 0x00],
        vec![0x7F],
        vec![0x80],
        vec![0xFF],
    ];
    assert_eq!(order, expected);
    pager.close()
}

#[test]
fn ascending_and_descending_inserts_split_and_move_the_root() -> TestResult {
    for descending in [false, true] {
        let tmp = TempPath::new("splits");
        let (mut pager, mut tree) = new_tree(&tmp, 8)?;
        let first_root = tree.root();
        let mut order: Vec<usize> = (0..2000).collect();
        if descending {
            order.reverse();
        }
        for &i in &order {
            tree.insert(&mut pager, &key(i), &value(i))?;
        }
        assert_ne!(tree.root(), first_root, "root moved after splits");
        assert!(root_is_internal(&tree, &mut pager)?);
        assert_eq!(tree.check(&mut pager), Ok(()));
        for i in 0..2000 {
            assert_eq!(tree.get(&mut pager, &key(i))?, Some(value(i)));
        }
        pager.close()?;
    }
    Ok(())
}

#[test]
fn replacing_large_values_with_small_ones_rebalances() -> TestResult {
    let tmp = TempPath::new("shrink");
    let (mut pager, mut tree) = new_tree(&tmp, 8)?;
    for i in 0..300 {
        tree.insert(&mut pager, &key(i), &vec![1u8; 1000])?;
    }
    for i in 0..300 {
        tree.insert(&mut pager, &key(i), b"")?;
        if i % 10 == 0 {
            assert_eq!(tree.check(&mut pager), Ok(()), "after shrinking {i}");
        }
    }
    assert_eq!(tree.check(&mut pager), Ok(()));
    let all = scan_all(&tree, &mut pager)?;
    assert_eq!(all.len(), 300);
    assert!(all.iter().all(|(_, v)| v.is_empty()));
    assert!(!pager.free_list()?.is_empty(), "merged pages were freed");
    pager.close()
}

/// Bounds over the 10 000-key data set that `BTreeMap::range` accepts.
fn well_ordered_ranges() -> Vec<OwnedBounds> {
    let mut between = key(100);
    between.push(0);
    vec![
        (Unbounded, Included(key(3000))),
        (Included(key(2000)), Excluded(key(2500))),
        (Excluded(key(7000)), Unbounded),
        (Included(key(10)), Included(key(10))),
        (Included(between.clone()), Included(key(400))),
        (Excluded(between), Excluded(key(101))),
        (Excluded(key(9998)), Included(key(9999))),
        (Included(b"a".to_vec()), Excluded(b"key-".to_vec())),
        (Included(b"zzz".to_vec()), Unbounded),
    ]
}

fn as_ref(bound: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    bound.as_ref().map(Vec::as_slice)
}

fn assert_ranges_match(
    tree: &BTree,
    pager: &mut Pager,
    model: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> TestResult {
    for (start, end) in well_ordered_ranges() {
        let expected: Pairs = model
            .range::<[u8], _>((as_ref(&start), as_ref(&end)))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let actual = scan(tree, pager, as_ref(&start), as_ref(&end))?;
        assert_eq!(actual, expected, "range {start:?}..{end:?}");
    }
    Ok(())
}

#[test]
fn ten_thousand_shuffled_inserts() -> TestResult {
    let tmp = TempPath::new("ten-thousand");
    let (mut pager, mut tree) = new_tree(&tmp, 8)?;
    let mut order: Vec<usize> = (0..N).collect();
    shuffle(&mut order, &mut XorShift64::new(SEED));
    let mut model = BTreeMap::new();
    for &i in &order {
        tree.insert(&mut pager, &key(i), &value(i))?;
        model.insert(key(i), value(i));
    }
    assert_eq!(tree.check(&mut pager), Ok(()));
    assert!(
        root_is_internal(&tree, &mut pager)?,
        "tree height is greater than 1"
    );
    for i in 0..N {
        assert_eq!(tree.get(&mut pager, &key(i))?, Some(value(i)), "key {i}");
    }
    let all = scan_all(&tree, &mut pager)?;
    assert_eq!(all.len(), N);
    assert!(
        all.windows(2).all(|w| w[0].0 < w[1].0),
        "strictly ascending"
    );
    let expected: Pairs = model.clone().into_iter().collect();
    assert_eq!(all, expected);
    assert_ranges_match(&tree, &mut pager, &model)?;

    pager.set_root("ten_thousand", tree.root())?;
    pager.close()?;
    let mut pager = Pager::open_with_capacity(tmp.path(), 8)?;
    let root = pager.root("ten_thousand").expect("root persisted");
    let tree = BTree::open(root);
    assert_eq!(tree.check(&mut pager), Ok(()));
    assert_eq!(scan_all(&tree, &mut pager)?, expected);
    assert_ranges_match(&tree, &mut pager, &model)?;
    pager.close()
}

#[test]
fn inverted_and_empty_bounds_yield_nothing() -> TestResult {
    let tmp = TempPath::new("bounds");
    let (mut pager, mut tree) = new_tree(&tmp, 8)?;
    for i in 0..500 {
        tree.insert(&mut pager, &key(i), &value(i))?;
    }
    let (a, b) = (key(400), key(100));
    let cases: [Bounds; 5] = [
        (Included(&a), Included(&b)),
        (Excluded(&a), Excluded(&b)),
        (Included(&b), Excluded(&b)),
        (Excluded(&b), Included(&b)),
        (Excluded(&b), Excluded(&b)),
    ];
    for (start, end) in cases {
        assert!(
            scan(&tree, &mut pager, start, end)?.is_empty(),
            "{start:?}..{end:?}"
        );
    }
    assert_eq!(
        scan(&tree, &mut pager, Included(&b), Included(&b))?.len(),
        1
    );
    pager.close()
}

#[test]
fn range_is_lazy_and_stops_early() -> TestResult {
    let tmp = TempPath::new("lazy");
    let (mut pager, mut tree) = new_tree(&tmp, 8)?;
    for i in 0..3000 {
        tree.insert(&mut pager, &key(i), &value(i))?;
    }
    let first: Pairs = tree
        .range(&mut pager, Included(&key(5)), Unbounded)?
        .take(3)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        first.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
        [key(5), key(6), key(7)]
    );
    pager.close()
}

#[test]
fn trees_share_one_pager() -> TestResult {
    let tmp = TempPath::new("two-trees");
    let mut pager = Pager::create_with_capacity(tmp.path(), 8)?;
    let mut a = BTree::open(BTree::create(&mut pager)?);
    let mut b = BTree::open(BTree::create(&mut pager)?);
    for i in 0..1000 {
        a.insert(&mut pager, &key(i), b"a")?;
        b.insert(&mut pager, &key(i), b"b")?;
    }
    assert_eq!(a.check(&mut pager), Ok(()));
    assert_eq!(b.check(&mut pager), Ok(()));
    assert_eq!(a.get(&mut pager, &key(7))?, Some(b"a".to_vec()));
    assert_eq!(b.get(&mut pager, &key(7))?, Some(b"b".to_vec()));
    assert_ne!(a.root(), PageId(0));
    pager.close()
}
