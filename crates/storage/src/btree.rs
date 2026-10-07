//! Balanced B+ tree of byte-string keys and values stored in pager pages.
//!
//! A `BTree` is a small handle holding the current root page id; every
//! operation borrows the `Pager` that owns the pages. The root changes when
//! it splits or collapses, so callers that persist a tree must re-read
//! `root()` after each modification (for example into `Pager::set_root`).
//!
//! Insertion splits a node whose cells no longer fit in one page. After an
//! insert, replace or delete leaves a non-root node below half full, it is
//! rebalanced with an adjacent sibling: merged if both fit in one page,
//! otherwise the cells are redistributed evenly between the two. The
//! resulting fill bounds are described in the node layout documentation.
//!
//! Operations are not atomic on their own: an error part-way through can
//! leave the tree inconsistent in the pager's transaction overlay, which
//! `check` will report. Callers undo that with `Pager::rollback` or
//! `Pager::rollback_to`, so a half-done operation never reaches the file.

use std::collections::HashSet;
use std::ops::Bound;

use crate::error::{Result, SizeKind, StorageError};
use crate::node::{
    INTERNAL_MIN_FILL, Internal, LEAF_MIN_FILL, Leaf, LeafCell, MAX_KEY_LEN, MAX_VALUE_LEN, Node,
    PAYLOAD, REBALANCE_BELOW, internal_payload, split_internal_cells, split_leaf_cells,
};
use crate::page::PageId;
use crate::pager::Pager;

/// Upper bound on tree height; deeper paths can only come from corruption.
const MAX_DEPTH: usize = 64;

/// Handle to a B-tree rooted at a pager page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BTree {
    root: PageId,
}

/// What a modified child reports to its parent.
enum Outcome {
    /// Written (or untouched) and within bounds; the parent is unchanged.
    Fine,
    /// The node split; `right` holds keys `>= sep` and needs a parent cell.
    Split { sep: Vec<u8>, right: PageId },
    /// A non-root node was written with payload below `REBALANCE_BELOW`.
    Underfull,
    /// The root internal node lost its last key; `child` becomes the root.
    Collapse { child: PageId },
}

impl BTree {
    /// Allocates an empty tree and returns its root page id.
    pub fn create(pager: &mut Pager) -> Result<PageId> {
        let id = pager.allocate()?;
        store(pager, id, &Node::Leaf(Leaf::default()))?;
        Ok(id)
    }

    /// A handle to the tree rooted at `root`. Pages are validated as they
    /// are read.
    pub fn open(root: PageId) -> BTree {
        BTree { root }
    }

    /// The current root page id.
    pub fn root(&self) -> PageId {
        self.root
    }

    pub fn get(&self, pager: &mut Pager, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if key.len() > MAX_KEY_LEN {
            return Ok(None);
        }
        let leaf = find_leaf(pager, self.root, Some(key))?;
        let Ok(index) = search(&leaf, key) else {
            return Ok(None);
        };
        Ok(leaf.cells.into_iter().nth(index).map(|(_, value)| value))
    }

    /// Inserts `key`, replacing the value if the key already exists. Keys
    /// longer than 256 bytes or values longer than 1024 bytes are rejected
    /// with `TooLarge` before any page is touched.
    pub fn insert(&mut self, pager: &mut Pager, key: &[u8], value: &[u8]) -> Result<()> {
        if key.len() > MAX_KEY_LEN {
            return Err(StorageError::TooLarge {
                kind: SizeKind::Key,
                len: key.len(),
                max: MAX_KEY_LEN,
            });
        }
        if value.len() > MAX_VALUE_LEN {
            return Err(StorageError::TooLarge {
                kind: SizeKind::Value,
                len: value.len(),
                max: MAX_VALUE_LEN,
            });
        }
        let outcome = insert_rec(pager, self.root, key, value, true, 0)?;
        self.fix_root(pager, outcome)
    }

    /// Removes `key`; returns whether it was present. Deleting an absent key
    /// writes nothing.
    pub fn delete(&mut self, pager: &mut Pager, key: &[u8]) -> Result<bool> {
        if key.len() > MAX_KEY_LEN {
            return Ok(false);
        }
        let (found, outcome) = delete_rec(pager, self.root, key, true, 0)?;
        if found {
            self.fix_root(pager, outcome)?;
        }
        Ok(found)
    }

    /// Iterates over pairs with keys in `(start, end)` in ascending unsigned
    /// byte order. Inverted or empty bounds yield nothing.
    pub fn range<'a>(
        &self,
        pager: &'a mut Pager,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Range<'a>> {
        let end_owned = end.map(<[u8]>::to_vec);
        if is_empty_range(start, end) {
            return Ok(Range::empty(pager, end_owned));
        }
        let start_key = match start {
            Bound::Included(key) | Bound::Excluded(key) => Some(key),
            Bound::Unbounded => None,
        };
        let leaf = find_leaf(pager, self.root, start_key)?;
        let cells: Vec<LeafCell> = leaf
            .cells
            .into_iter()
            .filter(|(key, _)| after_start(key, start))
            .collect();
        Ok(Range {
            pager,
            cells: cells.into_iter(),
            next_leaf: leaf.next,
            end: end_owned,
            hops: 0,
            done: false,
        })
    }

    /// Verifies the tree structure and returns the first violation found:
    /// key order within and across nodes, equal leaf depth, fill bounds of
    /// every non-root node, that every reachable page is allocated (inside
    /// the file, not on the free list, reached once) and the leaf chain.
    pub fn check(&self, pager: &mut Pager) -> std::result::Result<(), String> {
        let free = pager
            .free_list()
            .map_err(|e| format!("free list: {e}"))?
            .into_iter()
            .collect();
        let mut checker = Checker {
            pager,
            free,
            seen: HashSet::new(),
            leaf_depth: None,
            leaves: Vec::new(),
        };
        checker.visit(self.root, None, None, 0)?;
        checker.check_leaf_chain()
    }

    /// Every page of the tree, root first (pre-order). A page that does not
    /// decode as a node is listed but not descended into (`check` reports
    /// it), so page accounting still knows who references it. Fails with
    /// `Corrupt` on a page outside the file, a page reached twice or a path
    /// deeper than 64 levels, so a corrupt tree cannot loop.
    pub fn pages(&self, pager: &mut Pager) -> Result<Vec<PageId>> {
        let mut seen = HashSet::new();
        let mut pages = Vec::new();
        let mut stack = vec![(self.root, 0usize)];
        while let Some((id, depth)) = stack.pop() {
            if id.0 == 0 || id.0 >= pager.page_count() {
                return Err(StorageError::Corrupt {
                    page: id,
                    reason: "tree page outside the file",
                });
            }
            if depth > MAX_DEPTH {
                return Err(too_deep(id));
            }
            if !seen.insert(id) {
                return Err(StorageError::Corrupt {
                    page: id,
                    reason: "tree page reachable more than once",
                });
            }
            pages.push(id);
            let node = match load(pager, id) {
                Ok(node) => node,
                Err(StorageError::Corrupt { .. }) => continue,
                Err(e) => return Err(e),
            };
            if let Node::Internal(node) = node {
                for index in (0..=node.cells.len()).rev() {
                    stack.push((node.child(index, id)?, depth + 1));
                }
            }
        }
        Ok(pages)
    }

    fn fix_root(&mut self, pager: &mut Pager, outcome: Outcome) -> Result<()> {
        match outcome {
            Outcome::Fine | Outcome::Underfull => Ok(()),
            Outcome::Split { sep, right } => {
                let root = pager.allocate()?;
                let node = Internal {
                    leftmost: self.root,
                    cells: vec![(sep, right)],
                };
                store(pager, root, &Node::Internal(node))?;
                self.root = root;
                Ok(())
            }
            Outcome::Collapse { child } => {
                pager.free(self.root)?;
                self.root = child;
                Ok(())
            }
        }
    }
}

fn load(pager: &mut Pager, id: PageId) -> Result<Node> {
    let page = pager.read(id)?;
    Node::decode(&page, id, pager.page_count())
}

fn store(pager: &mut Pager, id: PageId, node: &Node) -> Result<()> {
    pager.write(id, &node.encode(id)?)
}

fn search(leaf: &Leaf, key: &[u8]) -> std::result::Result<usize, usize> {
    leaf.cells.binary_search_by(|(k, _)| k.as_slice().cmp(key))
}

fn too_deep(id: PageId) -> StorageError {
    StorageError::Corrupt {
        page: id,
        reason: "tree deeper than 64 levels",
    }
}

/// Descends to the leaf that would hold `key`, or the leftmost leaf.
fn find_leaf(pager: &mut Pager, root: PageId, key: Option<&[u8]>) -> Result<Leaf> {
    let mut id = root;
    for _ in 0..=MAX_DEPTH {
        match load(pager, id)? {
            Node::Leaf(leaf) => return Ok(leaf),
            Node::Internal(node) => {
                let index = key.map_or(0, |k| node.child_index(k));
                id = node.child(index, id)?;
            }
        }
    }
    Err(too_deep(id))
}

fn insert_rec(
    pager: &mut Pager,
    id: PageId,
    key: &[u8],
    value: &[u8],
    is_root: bool,
    depth: usize,
) -> Result<Outcome> {
    if depth > MAX_DEPTH {
        return Err(too_deep(id));
    }
    match load(pager, id)? {
        Node::Leaf(mut leaf) => {
            match search(&leaf, key) {
                Ok(index) => {
                    if let Some(cell) = leaf.cells.get_mut(index) {
                        cell.1 = value.to_vec();
                    }
                }
                Err(index) => leaf.cells.insert(index, (key.to_vec(), value.to_vec())),
            }
            finish_leaf(pager, id, leaf, is_root)
        }
        Node::Internal(node) => {
            let index = node.child_index(key);
            let child = node.child(index, id)?;
            let outcome = insert_rec(pager, child, key, value, false, depth + 1)?;
            update_parent(pager, id, node, index, outcome, is_root)
        }
    }
}

fn delete_rec(
    pager: &mut Pager,
    id: PageId,
    key: &[u8],
    is_root: bool,
    depth: usize,
) -> Result<(bool, Outcome)> {
    if depth > MAX_DEPTH {
        return Err(too_deep(id));
    }
    match load(pager, id)? {
        Node::Leaf(mut leaf) => {
            let Ok(index) = search(&leaf, key) else {
                return Ok((false, Outcome::Fine));
            };
            leaf.cells.remove(index);
            Ok((true, finish_leaf(pager, id, leaf, is_root)?))
        }
        Node::Internal(node) => {
            let index = node.child_index(key);
            let child = node.child(index, id)?;
            let (found, outcome) = delete_rec(pager, child, key, false, depth + 1)?;
            Ok((
                found,
                update_parent(pager, id, node, index, outcome, is_root)?,
            ))
        }
    }
}

/// Applies a child's outcome to its parent and classifies the parent.
fn update_parent(
    pager: &mut Pager,
    id: PageId,
    mut node: Internal,
    index: usize,
    outcome: Outcome,
    is_root: bool,
) -> Result<Outcome> {
    match outcome {
        Outcome::Fine => return Ok(Outcome::Fine),
        Outcome::Split { sep, right } => node.cells.insert(index, (sep, right)),
        Outcome::Underfull => rebalance(pager, id, &mut node, index)?,
        Outcome::Collapse { .. } => {
            return Err(StorageError::Corrupt {
                page: id,
                reason: "non-root child collapsed",
            });
        }
    }
    finish_internal(pager, id, node, is_root)
}

fn finish_leaf(pager: &mut Pager, id: PageId, leaf: Leaf, is_root: bool) -> Result<Outcome> {
    let size = leaf.payload();
    if size > PAYLOAD {
        let (left, right) = split_leaf_cells(leaf.cells);
        let sep = right
            .first()
            .map(|(key, _)| key.clone())
            .ok_or(StorageError::Corrupt {
                page: id,
                reason: "leaf split produced an empty half",
            })?;
        let right_id = pager.allocate()?;
        store(
            pager,
            right_id,
            &Node::Leaf(Leaf {
                cells: right,
                next: leaf.next,
            }),
        )?;
        store(
            pager,
            id,
            &Node::Leaf(Leaf {
                cells: left,
                next: Some(right_id),
            }),
        )?;
        return Ok(Outcome::Split {
            sep,
            right: right_id,
        });
    }
    store(pager, id, &Node::Leaf(leaf))?;
    Ok(classify(size, is_root))
}

fn finish_internal(
    pager: &mut Pager,
    id: PageId,
    node: Internal,
    is_root: bool,
) -> Result<Outcome> {
    if is_root && node.cells.is_empty() {
        return Ok(Outcome::Collapse {
            child: node.leftmost,
        });
    }
    let size = node.payload();
    if size > PAYLOAD {
        let (left, (sep, right_leftmost), right) =
            split_internal_cells(node.cells).ok_or(StorageError::Corrupt {
                page: id,
                reason: "internal node too small to split",
            })?;
        let right_id = pager.allocate()?;
        let right_node = Internal {
            leftmost: right_leftmost,
            cells: right,
        };
        store(pager, right_id, &Node::Internal(right_node))?;
        let left_node = Internal {
            leftmost: node.leftmost,
            cells: left,
        };
        store(pager, id, &Node::Internal(left_node))?;
        return Ok(Outcome::Split {
            sep,
            right: right_id,
        });
    }
    store(pager, id, &Node::Internal(node))?;
    Ok(classify(size, is_root))
}

fn classify(size: usize, is_root: bool) -> Outcome {
    if !is_root && size < REBALANCE_BELOW {
        return Outcome::Underfull;
    }
    Outcome::Fine
}

/// Merges the under-full child at `index` with an adjacent sibling, or
/// redistributes their cells evenly when both do not fit in one page.
/// Updates the parent's cells in memory; the caller writes the parent.
fn rebalance(
    pager: &mut Pager,
    parent_id: PageId,
    parent: &mut Internal,
    index: usize,
) -> Result<()> {
    let sep_index = index.saturating_sub(1);
    let left_id = parent.child(sep_index, parent_id)?;
    let right_id = parent.child(sep_index + 1, parent_id)?;
    let missing_sep = StorageError::Corrupt {
        page: parent_id,
        reason: "separator index out of range",
    };
    let sep = parent
        .cells
        .get(sep_index)
        .map(|c| c.0.clone())
        .ok_or(missing_sep)?;
    let new_sep = match (load(pager, left_id)?, load(pager, right_id)?) {
        (Node::Leaf(left), Node::Leaf(right)) => {
            rebalance_leaves(pager, (left_id, left), (right_id, right))?
        }
        (Node::Internal(left), Node::Internal(right)) => {
            rebalance_internals(pager, (left_id, left), sep, (right_id, right))?
        }
        _ => {
            return Err(StorageError::Corrupt {
                page: right_id,
                reason: "siblings are of different kinds",
            });
        }
    };
    match new_sep {
        None => {
            parent.cells.remove(sep_index);
        }
        Some(key) => {
            if let Some(cell) = parent.cells.get_mut(sep_index) {
                cell.0 = key;
            }
        }
    }
    Ok(())
}

/// Returns `None` after a merge (the right page is freed) or the new
/// separator after a redistribution.
fn rebalance_leaves(
    pager: &mut Pager,
    (left_id, left): (PageId, Leaf),
    (right_id, right): (PageId, Leaf),
) -> Result<Option<Vec<u8>>> {
    let mut cells = left.cells;
    cells.extend(right.cells);
    if cells.iter().map(crate::node::leaf_cell_size).sum::<usize>() <= PAYLOAD {
        store(
            pager,
            left_id,
            &Node::Leaf(Leaf {
                cells,
                next: right.next,
            }),
        )?;
        pager.free(right_id)?;
        return Ok(None);
    }
    let (left_cells, right_cells) = split_leaf_cells(cells);
    let sep = right_cells.first().map(|(key, _)| key.clone());
    store(
        pager,
        left_id,
        &Node::Leaf(Leaf {
            cells: left_cells,
            next: left.next,
        }),
    )?;
    store(
        pager,
        right_id,
        &Node::Leaf(Leaf {
            cells: right_cells,
            next: right.next,
        }),
    )?;
    Ok(sep)
}

fn rebalance_internals(
    pager: &mut Pager,
    (left_id, left): (PageId, Internal),
    sep: Vec<u8>,
    (right_id, right): (PageId, Internal),
) -> Result<Option<Vec<u8>>> {
    let mut cells = left.cells;
    cells.push((sep, right.leftmost));
    cells.extend(right.cells);
    if internal_payload(&cells) <= PAYLOAD {
        store(
            pager,
            left_id,
            &Node::Internal(Internal {
                leftmost: left.leftmost,
                cells,
            }),
        )?;
        pager.free(right_id)?;
        return Ok(None);
    }
    let (left_cells, (new_sep, right_leftmost), right_cells) =
        split_internal_cells(cells).ok_or(StorageError::Corrupt {
            page: left_id,
            reason: "internal siblings too small to redistribute",
        })?;
    let left_node = Internal {
        leftmost: left.leftmost,
        cells: left_cells,
    };
    store(pager, left_id, &Node::Internal(left_node))?;
    let right_node = Internal {
        leftmost: right_leftmost,
        cells: right_cells,
    };
    store(pager, right_id, &Node::Internal(right_node))?;
    Ok(Some(new_sep))
}

fn is_empty_range(start: Bound<&[u8]>, end: Bound<&[u8]>) -> bool {
    match (start, end) {
        (Bound::Included(s), Bound::Included(e)) => s > e,
        (Bound::Included(s) | Bound::Excluded(s), Bound::Excluded(e))
        | (Bound::Excluded(s), Bound::Included(e)) => s >= e,
        _ => false,
    }
}

fn after_start(key: &[u8], start: Bound<&[u8]>) -> bool {
    match start {
        Bound::Included(s) => key >= s,
        Bound::Excluded(s) => key > s,
        Bound::Unbounded => true,
    }
}

fn before_end(key: &[u8], end: &Bound<Vec<u8>>) -> bool {
    match end {
        Bound::Included(e) => key <= e.as_slice(),
        Bound::Excluded(e) => key < e.as_slice(),
        Bound::Unbounded => true,
    }
}

/// Lazy iterator over a key range. It reads one leaf at a time and follows
/// the leaf chain; it borrows the pager, so the tree cannot change while it
/// is alive. After an error it yields that error once and then ends.
pub struct Range<'a> {
    pager: &'a mut Pager,
    cells: std::vec::IntoIter<LeafCell>,
    next_leaf: Option<PageId>,
    end: Bound<Vec<u8>>,
    hops: u32,
    done: bool,
}

impl<'a> Range<'a> {
    fn empty(pager: &'a mut Pager, end: Bound<Vec<u8>>) -> Range<'a> {
        Range {
            pager,
            cells: Vec::new().into_iter(),
            next_leaf: None,
            end,
            hops: 0,
            done: true,
        }
    }

    fn load_next(&mut self, id: PageId) -> Result<()> {
        self.hops += 1;
        if self.hops >= self.pager.page_count() {
            return Err(StorageError::Corrupt {
                page: id,
                reason: "leaf chain longer than the file",
            });
        }
        let Node::Leaf(leaf) = load(self.pager, id)? else {
            return Err(StorageError::Corrupt {
                page: id,
                reason: "leaf chain points at an internal node",
            });
        };
        self.cells = leaf.cells.into_iter();
        self.next_leaf = leaf.next;
        Ok(())
    }
}

impl Iterator for Range<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            if let Some((key, value)) = self.cells.next() {
                if before_end(&key, &self.end) {
                    return Some(Ok((key, value)));
                }
                self.done = true;
                return None;
            }
            let Some(next) = self.next_leaf else {
                self.done = true;
                return None;
            };
            if let Err(e) = self.load_next(next) {
                self.done = true;
                return Some(Err(e));
            }
        }
        None
    }
}

struct Checker<'p> {
    pager: &'p mut Pager,
    free: HashSet<PageId>,
    seen: HashSet<PageId>,
    leaf_depth: Option<usize>,
    leaves: Vec<(PageId, Option<PageId>)>,
}

impl Checker<'_> {
    fn visit(
        &mut self,
        id: PageId,
        lower: Option<&[u8]>,
        upper: Option<&[u8]>,
        depth: usize,
    ) -> std::result::Result<(), String> {
        let fail = |problem: String| format!("page {id}: {problem}");
        let is_root = depth == 0;
        if id.0 == 0 || id.0 >= self.pager.page_count() {
            return Err(fail(
                "reachable page is not allocated (outside the file)".into(),
            ));
        }
        if self.free.contains(&id) {
            return Err(fail("reachable page is on the free list".into()));
        }
        if !self.seen.insert(id) {
            return Err(fail("page is reachable more than once".into()));
        }
        if depth > MAX_DEPTH {
            return Err(fail("tree deeper than 64 levels".into()));
        }
        let node = load(self.pager, id).map_err(|e| fail(e.to_string()))?;
        match node {
            Node::Leaf(leaf) => {
                check_keys(leaf.cells.iter().map(|c| c.0.as_slice()), lower, upper)
                    .map_err(fail)?;
                let size = leaf.payload();
                if !is_root && size < LEAF_MIN_FILL {
                    return Err(fail(format!(
                        "non-root leaf holds {size} payload bytes, below the minimum {LEAF_MIN_FILL}"
                    )));
                }
                match self.leaf_depth {
                    None => self.leaf_depth = Some(depth),
                    Some(expected) if expected != depth => {
                        return Err(fail(format!("leaf at depth {depth}, expected {expected}")));
                    }
                    Some(_) => {}
                }
                self.leaves.push((id, leaf.next));
                Ok(())
            }
            Node::Internal(node) => {
                check_keys(node.cells.iter().map(|c| c.0.as_slice()), lower, upper)
                    .map_err(fail)?;
                let size = node.payload();
                if !is_root && size < INTERNAL_MIN_FILL {
                    return Err(fail(format!(
                        "non-root internal node holds {size} payload bytes, below the minimum {INTERNAL_MIN_FILL}"
                    )));
                }
                let mut child_lower = lower;
                for index in 0..=node.cells.len() {
                    let child = node.child(index, id).map_err(|e| fail(e.to_string()))?;
                    let child_upper = node
                        .cells
                        .get(index)
                        .map_or(upper, |c| Some(c.0.as_slice()));
                    self.visit(child, child_lower, child_upper, depth + 1)?;
                    child_lower = child_upper;
                }
                Ok(())
            }
        }
    }

    fn check_leaf_chain(&self) -> std::result::Result<(), String> {
        let expected_next = self
            .leaves
            .iter()
            .skip(1)
            .map(|(id, _)| Some(*id))
            .chain([None]);
        for ((id, next), expected) in self.leaves.iter().zip(expected_next) {
            if *next != expected {
                let show = |p: Option<PageId>| p.map_or("none".to_owned(), |p| p.to_string());
                return Err(format!(
                    "page {id}: next leaf link is {}, expected {}",
                    show(*next),
                    show(expected)
                ));
            }
        }
        Ok(())
    }
}

/// Keys must be strictly ascending and within `[lower, upper)`.
fn check_keys<'k>(
    keys: impl Iterator<Item = &'k [u8]>,
    lower: Option<&[u8]>,
    upper: Option<&[u8]>,
) -> std::result::Result<(), String> {
    let mut prev: Option<&[u8]> = None;
    for key in keys {
        if prev.is_some_and(|p| p >= key) {
            return Err(format!("keys out of order at {key:02x?}"));
        }
        if lower.is_some_and(|l| key < l) {
            return Err(format!("key {key:02x?} is below its separator range"));
        }
        if upper.is_some_and(|u| key >= u) {
            return Err(format!("key {key:02x?} is at or above its separator range"));
        }
        prev = Some(key);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_common::TempPath;

    /// A two-level tree: root internal with separator "m" over two leaves
    /// that each hold enough bytes to satisfy the fill bound.
    struct Fixture {
        _tmp: TempPath,
        pager: Pager,
        tree: BTree,
        left: PageId,
        right: PageId,
    }

    fn cells(prefix: u8, n: u8) -> Vec<LeafCell> {
        (0..n).map(|i| (vec![prefix, i], vec![i; 100])).collect()
    }

    fn fixture() -> Fixture {
        let tmp = TempPath::new("check");
        let mut pager = Pager::create(tmp.path()).expect("create");
        let root = pager.allocate().expect("root");
        let left = pager.allocate().expect("left");
        let right = pager.allocate().expect("right");
        let leaf = |cells, next| Node::Leaf(Leaf { cells, next });
        store(&mut pager, left, &leaf(cells(b'a', 20), Some(right))).expect("left");
        store(&mut pager, right, &leaf(cells(b'n', 20), None)).expect("right");
        let node = Internal {
            leftmost: left,
            cells: vec![(b"m".to_vec(), right)],
        };
        store(&mut pager, root, &Node::Internal(node)).expect("root");
        Fixture {
            _tmp: tmp,
            pager,
            tree: BTree::open(root),
            left,
            right,
        }
    }

    fn check_error(f: &mut Fixture) -> String {
        f.tree.check(&mut f.pager).expect_err("check should fail")
    }

    #[test]
    fn fixture_is_valid() {
        let mut f = fixture();
        assert_eq!(f.tree.check(&mut f.pager), Ok(()));
    }

    #[test]
    fn pages_lists_every_tree_page_and_survives_undecodable_leaves() {
        let mut f = fixture();
        let root = f.tree.root();
        assert_eq!(
            f.tree.pages(&mut f.pager).expect("pages"),
            vec![root, f.left, f.right]
        );
        f.pager.free(f.right).expect("free a live leaf");
        assert_eq!(
            f.tree.pages(&mut f.pager).expect("pages"),
            vec![root, f.left, f.right],
            "a freed leaf is still listed as referenced"
        );
        let single = BTree::open(f.left);
        assert_eq!(single.pages(&mut f.pager).expect("pages"), vec![f.left]);
    }

    #[test]
    fn reports_keys_out_of_order_in_a_node() {
        let mut f = fixture();
        let mut page = f.pager.read(f.left).expect("read");
        // Second cell key starts at 8 + (4 + 2 + 100) + 4; make it sort first.
        page.bytes_mut()[8 + 106 + 4] = 0;
        f.pager.write(f.left, &page).expect("write");
        let error = check_error(&mut f);
        assert!(error.contains(&format!("page {}", f.left)), "{error}");
        assert!(error.contains("keys not in ascending order"), "{error}");
    }

    #[test]
    fn reports_key_outside_separator_range() {
        let mut f = fixture();
        let leaf = Node::Leaf(Leaf {
            cells: cells(b'z', 20),
            next: Some(f.right),
        });
        store(&mut f.pager, f.left, &leaf).expect("store");
        let error = check_error(&mut f);
        assert!(error.contains("at or above its separator range"), "{error}");
    }

    #[test]
    fn reports_leaf_at_wrong_depth() {
        let mut f = fixture();
        let a = f.pager.allocate().expect("a");
        let b = f.pager.allocate().expect("b");
        let leaf = |cells, next| Node::Leaf(Leaf { cells, next });
        store(&mut f.pager, a, &leaf(cells(b'n', 20), Some(b))).expect("a");
        store(&mut f.pager, b, &leaf(cells(b'q', 20), None)).expect("b");
        let seps: Vec<(Vec<u8>, PageId)> = vec![(vec![b'q'], b)];
        let mut node = Internal {
            leftmost: a,
            cells: seps,
        };
        // Pad the separator so the internal node meets its fill bound.
        node.cells[0].0.resize(INTERNAL_MIN_FILL, b'q');
        node.cells[0].0.truncate(crate::node::MAX_KEY_LEN);
        let padding: Vec<(Vec<u8>, PageId)> = (0..7u8)
            .map(|i| {
                let mut key = vec![b'r', i];
                key.resize(crate::node::MAX_KEY_LEN, 0);
                (key, b)
            })
            .collect();
        node.cells.extend(padding);
        store(&mut f.pager, f.right, &Node::Internal(node)).expect("internal");
        let left = Node::Leaf(Leaf {
            cells: cells(b'a', 20),
            next: Some(a),
        });
        store(&mut f.pager, f.left, &left).expect("left");
        let error = check_error(&mut f);
        assert!(error.contains("expected 1"), "{error}");
    }

    #[test]
    fn reports_underfilled_non_root_leaf() {
        let mut f = fixture();
        let leaf = Node::Leaf(Leaf {
            cells: cells(b'n', 2),
            next: None,
        });
        store(&mut f.pager, f.right, &leaf).expect("store");
        let error = check_error(&mut f);
        assert!(error.contains(&format!("page {}", f.right)), "{error}");
        assert!(error.contains("below the minimum 1402"), "{error}");
    }

    #[test]
    fn reports_reachable_free_page() {
        let mut f = fixture();
        f.pager.free(f.right).expect("free");
        let error = check_error(&mut f);
        assert!(error.contains("on the free list"), "{error}");
    }

    #[test]
    fn reports_child_outside_the_file() {
        let mut f = fixture();
        let mut page = f.pager.read(f.tree.root()).expect("read");
        // Root cell: key_len(2) + "m" + child at offset 8 + 3.
        page.bytes_mut()[11..15].copy_from_slice(&99u32.to_le_bytes());
        f.pager.write(f.tree.root(), &page).expect("write");
        let error = check_error(&mut f);
        assert!(error.contains("child pointer"), "{error}");
    }

    #[test]
    fn reports_page_reached_twice() {
        let mut f = fixture();
        let node = Internal {
            leftmost: f.left,
            cells: vec![(b"m".to_vec(), f.left)],
        };
        store(&mut f.pager, f.tree.root(), &Node::Internal(node)).expect("store");
        let error = check_error(&mut f);
        assert!(error.contains("more than once"), "{error}");
    }

    #[test]
    fn reports_broken_leaf_chain() {
        let mut f = fixture();
        let leaf = Node::Leaf(Leaf {
            cells: cells(b'a', 20),
            next: None,
        });
        store(&mut f.pager, f.left, &leaf).expect("store");
        let error = check_error(&mut f);
        assert!(error.contains("next leaf link is none"), "{error}");
    }

    #[test]
    fn corrupt_root_is_reported_by_operations_and_check() {
        let mut f = fixture();
        let mut page = f.pager.read(f.tree.root()).expect("read");
        page.bytes_mut()[0] = 9;
        f.pager.write(f.tree.root(), &page).expect("write");
        let root = f.tree.root();
        assert!(matches!(
            f.tree.get(&mut f.pager, b"a"),
            Err(StorageError::Corrupt { page, reason: "node tag" }) if page == root
        ));
        assert!(matches!(
            f.tree.insert(&mut f.pager, b"a", b""),
            Err(StorageError::Corrupt { .. })
        ));
        assert!(matches!(
            f.tree.delete(&mut f.pager, b"a"),
            Err(StorageError::Corrupt { .. })
        ));
        assert!(
            f.tree
                .range(&mut f.pager, Bound::Unbounded, Bound::Unbounded)
                .is_err()
        );
        assert!(check_error(&mut f).contains("node tag"));
    }

    #[test]
    fn range_reports_corrupt_leaf_in_chain_once() {
        let mut f = fixture();
        let mut page = f.pager.read(f.right).expect("read");
        page.bytes_mut()[1] = 7;
        f.pager.write(f.right, &page).expect("write");
        let items: Vec<_> = f
            .tree
            .range(&mut f.pager, Bound::Unbounded, Bound::Unbounded)
            .expect("range")
            .collect();
        assert_eq!(items.len(), 21);
        assert!(items[..20].iter().all(Result::is_ok));
        assert!(matches!(items[20], Err(StorageError::Corrupt { .. })));
    }
}
