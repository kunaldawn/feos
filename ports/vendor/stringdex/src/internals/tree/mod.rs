use std::{collections::BTreeSet, mem};

pub use crate::internals::data_structures::TinySet;

use super::data_structures::OptUsize;

/// A read-only trie wrapper for a search tree branch node.
#[derive(Clone, Debug)]
pub struct Trie<'tree> {
    pub tree: &'tree SearchTree,
    pub node: usize,
    pub offset: usize,
}

impl<'tree> Trie<'tree> {
    /// Returns all nodes that are direct children of this node.
    pub fn children(&self) -> Box<dyn Iterator<Item = (u8, Trie<'tree>)> + 'tree> {
        let node = &self.tree.nodes[self.node];
        if self.offset < node.data.len() {
            Box::new(std::iter::once((
                self.tree.data[node.data.clone()][self.offset],
                Trie {
                    offset: self.offset + 1,
                    ..self.clone()
                },
            )))
        } else {
            Box::new(node.branches().map(|(c, node)| {
                (
                    c,
                    Trie {
                        tree: self.tree,
                        offset: 0,
                        node,
                    },
                )
            }))
        }
    }
    /// All exact matches for the string represented by this node.
    pub fn matches(&self) -> Box<dyn Iterator<Item = u32> + 'tree> {
        if self.offset == self.tree.nodes[self.node].data.len() {
            Box::new(
                self.tree.nodes[self.node]
                    .leaves
                    .leaves_whole
                    .iter()
                    .copied(),
            )
        } else {
            Box::new(std::iter::empty())
        }
    }
    /// All matches for strings that contain the string represented by this node.
    pub fn substring_matches(&self) -> Box<dyn Iterator<Item = u32> + 'tree> {
        let mut bts = BTreeSet::new();
        bts.extend(
            self.tree.nodes[self.node]
                .leaves
                .leaves_whole
                .iter()
                .copied(),
        );
        bts.extend(
            self.tree.nodes[self.node]
                .leaves
                .leaves_suffix
                .iter()
                .copied(),
        );
        for (_, child) in self.children() {
            bts.extend(child.substring_matches());
        }
        Box::new(bts.into_iter())
    }
    /// All matches for strings that start with the string represented by this node.
    pub fn prefix_matches(&self) -> Box<dyn Iterator<Item = u32> + 'tree> {
        let mut bts = BTreeSet::new();
        bts.extend(
            self.tree.nodes[self.node]
                .leaves
                .leaves_whole
                .iter()
                .copied(),
        );
        for (_, child) in self.children() {
            bts.extend(child.prefix_matches());
        }
        Box::new(bts.into_iter())
    }
    /// All matches that end with the string represented by this node,
    /// excluding exact matches.
    pub fn suffix_leaves(&self) -> Box<dyn Iterator<Item = u32> + 'tree> {
        if self.offset == self.tree.nodes[self.node].data.len() {
            Box::new(
                self.tree.nodes[self.node]
                    .leaves
                    .leaves_suffix
                    .iter()
                    .copied(),
            )
        } else {
            Box::new(std::iter::empty())
        }
    }
    /// Returns a single node that is a direct child of this node.
    pub fn child(&self, c: u8) -> Option<Trie<'tree>> {
        let node = &self.tree.nodes[self.node];
        if self.offset < node.data.len() {
            if self.tree.data[node.data.clone()][self.offset] == c {
                Some(Trie {
                    offset: self.offset + 1,
                    ..self.clone()
                })
            } else {
                None
            }
        } else {
            node.get_branch(c).map(|node| Trie {
                offset: 0,
                node,
                tree: self.tree,
            })
        }
    }
}

#[derive(Debug)]
pub struct SearchTree {
    pub data: Vec<u8>,
    pub nodes: Vec<SearchTreeBranchNode>,
}

impl Default for SearchTree {
    fn default() -> SearchTree {
        SearchTree {
            data: Vec::new(),
            nodes: vec![SearchTreeBranchNode::default()],
        }
    }
}

/// A search tree node represents a string because of both its position and
/// its data. For example, the root of the tree represents the empty string,
/// and each child represents the same string as the node itself with more
/// text appended to it (the byte of the branch, plus the node's `data`).
///
/// Theoretically, the `data` field isn't needed, but a tree with a separate
/// file for every byte would generate a lot of files.
#[derive(Debug, Default)]
pub struct SearchTreeBranchNode {
    /// The "data" represented by this node,
    /// with the initial byte (in the parent node's branch) stripped off.
    /// This is an index into the `SearchTree.data` field.
    ///
    /// This may be empty. That means this node is an interior node
    /// that only represents the branch byte.
    pub data: std::ops::Range<usize>,
    /// Leaf nodes. In the charts from ufl.edu, these are "empty nodes."
    pub leaves: SearchTreeLeaves,
    /// Branches. In a suffix tree, each branch is completely different,
    /// so there's only one branch with any particular starting byte,
    /// and there is no branch with an empty suffix.
    pub branches: Vec<u64>,
    /// This flag excludes a node from prefix-only matching.
    pub suffixes_only: bool,
}

impl SearchTreeBranchNode {
    fn get_branch_list_index(&self, key: u8) -> Result<usize, usize> {
        self.branches
            .binary_search_by_key(&key, |parts| (*parts >> 48) as u8)
    }
    pub fn get_branch(&self, key: u8) -> Option<usize> {
        self.branches
            .get(self.get_branch_list_index(key).ok()?)
            .map(|parts| (*parts & 0x0000_FFFF_FFFF_FFFF) as usize)
    }
    pub fn insert_branch(&mut self, key: u8, value: usize) {
        assert!(value < 0x0000_FFFF_FFFF_FFFF);
        match self.get_branch_list_index(key) {
            Ok(index) => self.branches[index] = ((key as u64) << 48) | (value as u64),
            Err(index) => {
                self.branches
                    .insert(index, ((key as u64) << 48) | (value as u64));
            }
        }
    }
    pub fn push_branch_unchecked(&mut self, key: u8, value: usize) {
        assert!(value < 0x0000_FFFF_FFFF_FFFF);
        self.branches.push(((key as u64) << 48) | (value as u64));
    }
    pub fn branches(
        &self,
    ) -> impl Iterator<Item = (u8, usize)> + DoubleEndedIterator + ExactSizeIterator + Clone {
        self.branches.iter().map(|&branch| {
            (
                (branch >> 48) as u8,
                (branch & 0x0000_FFFF_FFFF_FFFF) as usize,
            )
        })
    }

    pub fn branch_keys(
        &self,
    ) -> impl Iterator<Item = u8> + DoubleEndedIterator + ExactSizeIterator + Clone {
        self.branches().map(|(key, _)| key)
    }
    pub fn branch_values(
        &self,
    ) -> impl Iterator<Item = usize> + DoubleEndedIterator + ExactSizeIterator + Clone {
        self.branches().map(|(_, value)| value)
    }
    pub fn get_branch_key_by_list_index(&self, index: usize) -> u8 {
        (self.branches[index] >> 48) as u8
    }
    pub fn entry_branch<'tree>(&'tree mut self, key: u8) -> BranchEntry<'tree> {
        match self.get_branch_list_index(key) {
            Ok(index) => BranchEntry::Occupied(OccupiedBranchEntry {
                branches: &mut self.branches,
                index,
            }),
            Err(index) => BranchEntry::Vacant(VacantBranchEntry {
                branches: &mut self.branches,
                index,
                key,
            }),
        }
    }
}

pub enum BranchEntry<'tree> {
    Occupied(OccupiedBranchEntry<'tree>),
    Vacant(VacantBranchEntry<'tree>),
}

pub struct OccupiedBranchEntry<'tree> {
    branches: &'tree mut Vec<u64>,
    index: usize,
}
impl<'tree> OccupiedBranchEntry<'tree> {
    pub fn get(&self) -> usize {
        (self.branches[self.index] & 0x0000_FFFF_FFFF_FFFF) as usize
    }
}
pub struct VacantBranchEntry<'tree> {
    branches: &'tree mut Vec<u64>,
    index: usize,
    key: u8,
}
impl<'tree> VacantBranchEntry<'tree> {
    pub fn insert(&mut self, value: usize) {
        self.branches
            .insert(self.index, ((self.key as u64) << 48) | (value as u64));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchTreeLeafKind {
    Whole,
    Suffix,
}

#[derive(Clone, Debug, Default)]
pub struct SearchTreeLeaves {
    pub leaves_whole: TinySet,
    pub leaves_suffix: TinySet,
}

impl SearchTreeLeaves {
    pub fn new(id: u32, kind: SearchTreeLeafKind) -> SearchTreeLeaves {
        match kind {
            SearchTreeLeafKind::Whole => SearchTreeLeaves {
                leaves_whole: TinySet::Singular(id),
                leaves_suffix: TinySet::empty(),
            },
            SearchTreeLeafKind::Suffix => SearchTreeLeaves {
                leaves_whole: TinySet::empty(),
                leaves_suffix: TinySet::Singular(id),
            },
        }
    }
    pub fn insert(&mut self, id: u32, kind: SearchTreeLeafKind) {
        match kind {
            SearchTreeLeafKind::Whole => self.leaves_whole.push(id),
            SearchTreeLeafKind::Suffix => self.leaves_suffix.push(id),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.leaves_suffix.is_empty() && self.leaves_whole.is_empty()
    }
    pub fn len(&self) -> usize {
        self.leaves_suffix.len() + self.leaves_whole.len()
    }
    pub fn iter(&self) -> impl Iterator<Item = u32> {
        self.leaves_whole
            .iter()
            .copied()
            .chain(self.leaves_suffix.iter().copied())
    }
}

/// <https://en.wikipedia.org/wiki/Ukkonen%27s_algorithm>
///
/// <http://programmerspatch.blogspot.com.au/2013/02/ukkonens-suffix-tree-algorithm.html
#[derive(Default, Debug)]
pub struct ImplicitTreeNode {
    /// which codepoint this substring starts at
    ///
    /// this number includes the first byte, even though that's redundant
    /// with the parent's branch byte
    pub data_first: usize,
    /// number of code points covered by this node
    ///
    /// `None` means it ends at the end of the list
    pub data_len: usize,
    /// list of children
    pub branches: Vec<(ImplicitTreeChar, usize)>,
    /// suffix link
    pub suffix_link: Option<OptUsize>,
    /// parent of node
    pub parent: Option<OptUsize>,
}

#[derive(Debug)]
pub struct ImplicitTree<'data> {
    pub nodes: Vec<ImplicitTreeNode>,
    pub data: &'data [ImplicitTreeChar],
    pub lengths: &'data [usize],
    pub implicit_end: usize,
    pub first_leaf_with_longest_suffix: usize,
    pub last_extension: usize,
    pub current: Option<usize>,
    pub old_beta: ImplicitTreePos,
    pub last: ImplicitTreePos,
}

impl ImplicitTree<'_> {
    pub fn len_for_terminal(&self, id: u32) -> usize {
        self.lengths[id as usize]
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ImplicitTreeChar {
    Byte(u8),
    Terminal(u32),
}

impl Default for ImplicitTreeChar {
    fn default() -> ImplicitTreeChar {
        ImplicitTreeChar::Byte(0)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ImplicitTreePos {
    node: usize,
    codepoint: usize,
}

impl ImplicitTreeNode {
    pub fn get_branch(&self, key: ImplicitTreeChar) -> Option<usize> {
        let idx = self.branches.binary_search_by_key(&key, |&(k, _)| k).ok()?;
        Some(self.branches.get(idx)?.1)
    }
    pub fn insert_branch(&mut self, key: ImplicitTreeChar, value: usize) {
        match self.branches.binary_search_by_key(&key, |&(k, _)| k) {
            Ok(index) => self.branches[index].1 = value,
            Err(index) => {
                self.branches.insert(index, (key, value));
            }
        }
    }
    pub fn entry_branch<'tree>(
        &'tree mut self,
        key: ImplicitTreeChar,
    ) -> ImplicitBranchEntry<'tree> {
        match self.branches.binary_search_by_key(&key, |&(k, _)| k) {
            Ok(index) => ImplicitBranchEntry::Occupied(OccupiedImplicitBranchEntry {
                branches: &mut self.branches,
                index,
            }),
            Err(index) => ImplicitBranchEntry::Vacant(VacantImplicitBranchEntry {
                branches: &mut self.branches,
                index,
                key,
            }),
        }
    }
    fn is_leaf(&self) -> bool {
        self.branches.is_empty()
    }
    fn end(&self, implicit_tree: &ImplicitTree<'_>) -> usize {
        if self.data_len == usize::MAX {
            implicit_tree.implicit_end
        } else {
            self.data_len
                .saturating_add(self.data_first)
                .saturating_sub(1)
        }
    }
    fn len(&self, _implicit_tree: &ImplicitTree<'_>) -> usize {
        self.data_len
    }
}

pub enum ImplicitBranchEntry<'tree> {
    Occupied(OccupiedImplicitBranchEntry<'tree>),
    Vacant(VacantImplicitBranchEntry<'tree>),
}

pub struct OccupiedImplicitBranchEntry<'tree> {
    branches: &'tree mut Vec<(ImplicitTreeChar, usize)>,
    index: usize,
}
impl<'tree> OccupiedImplicitBranchEntry<'tree> {
    pub fn get(&self) -> usize {
        self.branches[self.index].1
    }
}
pub struct VacantImplicitBranchEntry<'tree> {
    branches: &'tree mut Vec<(ImplicitTreeChar, usize)>,
    index: usize,
    key: ImplicitTreeChar,
}
impl<'tree> VacantImplicitBranchEntry<'tree> {
    pub fn insert(&mut self, value: usize) {
        self.branches.insert(self.index, (self.key, value));
    }
}

impl ImplicitTreePos {
    fn at_edge_end(&self, implicit_tree: &ImplicitTree<'_>) -> bool {
        implicit_tree.nodes[self.node].end(implicit_tree) == self.codepoint
    }
    fn continues(&self, c: ImplicitTreeChar, implicit_tree: &ImplicitTree<'_>) -> bool {
        if implicit_tree.nodes[self.node].end(implicit_tree) > self.codepoint {
            implicit_tree.data[self.codepoint + 1] == c
        } else {
            implicit_tree.nodes[self.node].get_branch(c).is_some()
        }
    }
    fn node_split(&self, implicit_tree: &mut ImplicitTree<'_>) -> usize {
        let old_node_parent = implicit_tree.nodes[self.node]
            .parent
            .expect("cannot split root node");
        let new_node = implicit_tree.nodes.len();
        let data_first = implicit_tree.nodes[self.node].data_first;
        let new_node_len = self.codepoint - data_first + 1;
        implicit_tree.nodes.push(ImplicitTreeNode {
            data_first,
            data_len: new_node_len,
            parent: Some(old_node_parent),
            ..ImplicitTreeNode::default()
        });
        if !implicit_tree.nodes[self.node].is_leaf() {
            implicit_tree.nodes[self.node].data_len -= new_node_len;
        }
        let c = implicit_tree.data[data_first];
        assert!(
            implicit_tree.nodes[usize::from(old_node_parent)]
                .get_branch(c)
                .is_some()
        );
        implicit_tree.nodes[usize::from(old_node_parent)].insert_branch(c, new_node);
        let d = implicit_tree.data[self.codepoint + 1];
        implicit_tree.nodes[new_node].insert_branch(d, self.node);
        implicit_tree.nodes[self.node].data_first = self.codepoint + 1;
        implicit_tree.nodes[new_node].parent = Some(old_node_parent);
        implicit_tree.nodes[self.node].parent = Some(new_node.into());
        new_node
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImplicitTreePath {
    start: usize,
    len: usize,
}

impl ImplicitTreePath {
    pub fn prepend(&mut self, count: usize) {
        self.start -= count;
        self.len += count;
    }
}

pub fn encode_search_tree_ukkonen<'data>(mapping: impl Iterator<Item = &'data [u8]>) -> SearchTree {
    let mut implicit_tree_nodes = Vec::new();
    implicit_tree_nodes.push(ImplicitTreeNode {
        data_len: 0,
        ..ImplicitTreeNode::default()
    });
    let (data, lengths) = {
        let pt: Vec<&[u8]> = mapping.collect();
        let mut off = 0;
        let lengths: Vec<usize> = pt
            .iter()
            .map(|&entry| {
                off += entry.len() + 1;
                entry.len()
            })
            .collect();
        let mut data = Vec::with_capacity(off);
        for (i, entry) in pt.iter().copied().enumerate() {
            data.extend(entry.iter().copied().map(ImplicitTreeChar::Byte));
            data.push(ImplicitTreeChar::Terminal(i.try_into().unwrap()));
        }
        assert_eq!(data.capacity(), data.len());
        (data, lengths)
    };
    let implicit_tree = if let Some(&c0) = data.first() {
        let first_leaf_with_longest_suffix = implicit_tree_nodes.len();
        implicit_tree_nodes.push(ImplicitTreeNode {
            parent: Some(0.into()),
            data_len: usize::MAX,
            ..ImplicitTreeNode::default()
        });
        implicit_tree_nodes[0].insert_branch(c0, first_leaf_with_longest_suffix);
        let implicit_end = 0;
        let last_extension = 0;
        let mut implicit_tree = ImplicitTree {
            nodes: implicit_tree_nodes,
            data: &data[..],
            lengths: &lengths[..],
            implicit_end,
            first_leaf_with_longest_suffix,
            last_extension,
            current: None,
            old_beta: ImplicitTreePos {
                node: 0,
                codepoint: 0,
            },
            last: ImplicitTreePos {
                node: 0,
                codepoint: 0,
            },
        };
        for i in 1..=data.len() {
            phase(i, &mut implicit_tree);
        }
        implicit_tree
    } else {
        ImplicitTree {
            nodes: implicit_tree_nodes,
            data: &[][..],
            lengths: &[][..],
            implicit_end: 0,
            first_leaf_with_longest_suffix: 0,
            last_extension: 0,
            current: None,
            old_beta: ImplicitTreePos {
                node: 0,
                codepoint: 0,
            },
            last: ImplicitTreePos {
                node: 0,
                codepoint: 0,
            },
        }
    };
    fn phase(i: usize, implicit_tree: &mut ImplicitTree<'_>) {
        let mut j = implicit_tree.last_extension;
        implicit_tree.current = None;
        while j <= i {
            if !extension(j, i, implicit_tree) {
                break;
            }
            j += 1;
        }
        implicit_tree.last_extension = if j > i { i } else { j };
        implicit_tree.implicit_end += 1;
    }
    fn extension(j: usize, i: usize, implicit_tree: &mut ImplicitTree<'_>) -> bool {
        let mut res = true;
        let pos = find_beta(j, i - 1, implicit_tree);
        if implicit_tree.nodes[pos.node].is_leaf() && pos.at_edge_end(implicit_tree) {
            res = true;
        } else if !pos.continues(
            implicit_tree.data.get(i).copied().unwrap_or_default(),
            implicit_tree,
        ) {
            let leaf = implicit_tree.nodes.len();
            implicit_tree.nodes.push(ImplicitTreeNode {
                data_first: i,
                data_len: usize::MAX,
                ..ImplicitTreeNode::default()
            });
            if pos.node == 0 || pos.at_edge_end(implicit_tree) {
                implicit_tree.nodes[pos.node]
                    .insert_branch(implicit_tree.data.get(i).copied().unwrap_or_default(), leaf);
                implicit_tree.nodes[leaf].parent = Some(pos.node.into());
                if let Some(current) = implicit_tree.current.take() {
                    implicit_tree.nodes[current].suffix_link = Some(pos.node.into());
                }
            } else {
                let new_node = pos.node_split(implicit_tree);
                if let Some(current) = implicit_tree.current.take() {
                    implicit_tree.nodes[current].suffix_link = Some(new_node.into());
                }
                if i - j == 1 {
                    implicit_tree.nodes[new_node].suffix_link = Some(0.into());
                } else {
                    implicit_tree.current = Some(new_node);
                }
                implicit_tree.nodes[new_node].insert_branch(implicit_tree.data[i], leaf);
                implicit_tree.nodes[leaf].parent = Some(new_node.into());
            }
            update_old_beta(pos, i, implicit_tree);
        } else {
            if let Some(current) = implicit_tree.current.take() {
                implicit_tree.nodes[current].suffix_link = Some(pos.node.into());
            }
            update_old_beta(pos, i, implicit_tree);
            res = false;
        }
        res
    }
    fn walk_down(
        mut node: usize,
        path: ImplicitTreePath,
        implicit_tree: &mut ImplicitTree<'_>,
    ) -> ImplicitTreePos {
        let mut start = path.start;
        let mut len = path.len;
        let mut pos = None;
        node = implicit_tree.nodes[node]
            .get_branch(implicit_tree.data[start])
            .unwrap();
        while len > 0 {
            if len <= implicit_tree.nodes[node].len(implicit_tree) {
                pos = Some(ImplicitTreePos {
                    node,
                    codepoint: implicit_tree.nodes[node].data_first + len - 1,
                });
                break;
            } else {
                start += implicit_tree.nodes[node].len(implicit_tree);
                len -= implicit_tree.nodes[node].len(implicit_tree);
                node = implicit_tree.nodes[node]
                    .get_branch(implicit_tree.data[start])
                    .unwrap();
            }
        }
        pos.expect("walk_down called with dangling path")
    }
    fn find_beta(j: usize, i: usize, implicit_tree: &mut ImplicitTree<'_>) -> ImplicitTreePos {
        let p = if implicit_tree.last_extension > 0 && implicit_tree.last_extension == j {
            ImplicitTreePos {
                node: implicit_tree.old_beta.node,
                codepoint: implicit_tree.old_beta.codepoint,
            }
        } else if j > i {
            ImplicitTreePos {
                node: 0,
                codepoint: 0,
            }
        } else if j == 0 {
            ImplicitTreePos {
                node: implicit_tree.first_leaf_with_longest_suffix,
                codepoint: i,
            }
        } else {
            let mut node = implicit_tree.last.node;
            let len = implicit_tree.last.codepoint + 1
                - implicit_tree.nodes[implicit_tree.last.node].data_first;
            let mut path = ImplicitTreePath {
                start: implicit_tree.nodes[node].data_first,
                len,
            };
            node = usize::from(implicit_tree.nodes[node].parent.expect("not root"));
            while node != 0 && implicit_tree.nodes[node].suffix_link.is_none() {
                path.prepend(implicit_tree.nodes[node].data_len);
                node = usize::from(implicit_tree.nodes[node].parent.expect("not root"));
            }
            if node != 0 {
                node = usize::from(
                    implicit_tree.nodes[node]
                        .suffix_link
                        .expect("loop above ensures it"),
                );
                walk_down(node, path, implicit_tree)
            } else {
                walk_down(
                    0,
                    ImplicitTreePath {
                        start: j,
                        len: i - j + 1,
                    },
                    implicit_tree,
                )
            }
        };
        implicit_tree.last = p;
        p
    }
    fn update_old_beta(pos: ImplicitTreePos, i: usize, implicit_tree: &mut ImplicitTree<'_>) {
        implicit_tree.old_beta = if implicit_tree.nodes[pos.node].end(implicit_tree) > pos.codepoint
        {
            ImplicitTreePos {
                node: pos.node,
                codepoint: pos.codepoint + 1,
            }
        } else {
            let node = implicit_tree.nodes[pos.node]
                .get_branch(implicit_tree.data.get(i).copied().unwrap_or_default())
                .unwrap();
            ImplicitTreePos {
                node,
                codepoint: implicit_tree.nodes[node].data_first,
            }
        };
    }

    // Finally, convert to an explicit tree
    let mut explicit_tree = Vec::with_capacity(implicit_tree.nodes.len());
    fn make_explicit_tree_and_mark_suffixes_only(
        explicit_tree: &mut Vec<SearchTreeBranchNode>,
        implicit_tree: &ImplicitTree<'_>,
        node_idx: usize,
        mut total_len: usize,
    ) -> Option<usize> {
        let node = &implicit_tree.nodes[node_idx];
        // make lengths explicit
        let data_first = node.data_first;
        let mut data_len = if node.is_leaf() {
            implicit_tree.implicit_end - node.data_first + 1
        } else {
            node.data_len
        };
        if data_len + data_first >= implicit_tree.data.len() {
            // we include a null terminator while building the tree
            // the implicit tree doesn't use it
            data_len -= 1;
        };
        // convert format to leaves inlined into the node
        let (mut explicit_node, terminal) = if node_idx == 0 {
            (
                SearchTreeBranchNode {
                    data: 0..0,
                    leaves: SearchTreeLeaves::default(),
                    branches: Vec::new(),
                    // changed later if a non-suffix-only child is added,
                    // or if a whole-string leaf is added
                    suffixes_only: true,
                },
                None,
            )
        } else {
            let terminal = implicit_tree.data[data_first..data_first + data_len]
                .iter()
                .enumerate()
                .find_map(|(i, ch)| {
                    if let ImplicitTreeChar::Terminal(terminal) = *ch {
                        Some((i, terminal))
                    } else {
                        None
                    }
                });
            let (terminal, len, capacity) = if let Some((off, terminal)) = terminal {
                (Some(terminal), off, 0)
            } else {
                (None, data_len, node.branches.len() / 2)
            };
            total_len += len;
            (
                SearchTreeBranchNode {
                    data: data_first + 1..data_first + len,
                    leaves: SearchTreeLeaves::default(),
                    branches: Vec::with_capacity(capacity),
                    // changed later if a non-suffix-only child is added,
                    // or if a whole-string leaf is added
                    suffixes_only: true,
                },
                terminal,
            )
        };
        let is_terminal_for_whole_string =
            terminal.is_some_and(|id| implicit_tree.len_for_terminal(id) == total_len);
        let new_explicit_node_idx = explicit_tree.len();
        if let Some(id) = terminal {
            explicit_node.leaves.insert(
                id,
                if is_terminal_for_whole_string {
                    SearchTreeLeafKind::Whole
                } else {
                    SearchTreeLeafKind::Suffix
                },
            );
            if is_terminal_for_whole_string {
                explicit_node.suffixes_only = false;
            }
            explicit_tree.push(explicit_node);
            // Ukkonen's algorithm operates on a single string. To extend it to multiple strings,
            // we build a tree for the result of concatenating all the data into one string, separated
            // by "terminals" (which are null bytes in the backing buffer, but distinguished here).
            //
            // However, we don't actually want to give search results for people who string multiple
            // names together. To fix this, the explicit tree that we return in the end has to be
            // tree-shaken by excluding every leaf with multiple items in it.
            return Some(new_explicit_node_idx);
        } else if node.branches.is_empty() {
            // tree shaking (see above)
            return None;
        };
        explicit_tree.push(SearchTreeBranchNode::default());
        for (child_ch, child_node_idx) in node.branches.iter().copied() {
            match child_ch {
                ImplicitTreeChar::Terminal(child_terminal)
                    if implicit_tree.len_for_terminal(child_terminal) < total_len =>
                {
                    // tree shaking
                }
                ImplicitTreeChar::Terminal(child_terminal) => {
                    let child_is_terminal_for_whole_string =
                        implicit_tree.len_for_terminal(child_terminal) == total_len;
                    if child_is_terminal_for_whole_string || node_idx != 0 {
                        // don't add suffix code to root node
                        explicit_node.leaves.insert(
                            child_terminal,
                            if child_is_terminal_for_whole_string {
                                SearchTreeLeafKind::Whole
                            } else {
                                SearchTreeLeafKind::Suffix
                            },
                        );
                    }
                    if child_is_terminal_for_whole_string {
                        explicit_node.suffixes_only = false;
                    }
                }
                ImplicitTreeChar::Byte(child_byte) => {
                    let child_explicit_idx = stacker::maybe_grow(32 * 1024, 1024 * 1024, || {
                        make_explicit_tree_and_mark_suffixes_only(
                            explicit_tree,
                            implicit_tree,
                            child_node_idx,
                            total_len,
                        )
                    });
                    if let Some(child_explicit_idx) = child_explicit_idx {
                        explicit_node.push_branch_unchecked(child_byte, child_explicit_idx);
                        if !explicit_tree[child_explicit_idx].suffixes_only {
                            explicit_node.suffixes_only = false;
                        }
                    }
                }
            }
        }
        if node_idx == 0 || !explicit_node.leaves.leaves_whole.is_empty() {
            assert!(!explicit_node.suffixes_only);
        }
        explicit_tree[new_explicit_node_idx] = explicit_node;
        Some(new_explicit_node_idx)
    }
    make_explicit_tree_and_mark_suffixes_only(&mut explicit_tree, &implicit_tree, 0, 0);
    if explicit_tree.is_empty() {
        assert!(data.is_empty());
        explicit_tree.push(SearchTreeBranchNode::default());
    }
    SearchTree {
        nodes: explicit_tree,
        data: data
            .into_iter()
            .map(|ch| match ch {
                ImplicitTreeChar::Byte(byte) => byte,
                ImplicitTreeChar::Terminal(_) => b'\0',
            })
            .collect(),
    }
}

pub fn encode_search_tree_naive<'x>(mapping: impl Iterator<Item = &'x [u8]>) -> SearchTree {
    let mut tree = SearchTree::default();

    let mut mapping: Vec<(Vec<u8>, u32)> = mapping
        .enumerate()
        .map(|(id, st)| (st.to_owned(), u32::try_from(id).unwrap()))
        .collect();
    mapping.sort_by_key(|(name, _id)| name.len());
    let max = mapping.last().map(|(name, _id)| name.len()).unwrap_or(0);

    // Each entry in prev_leaves is the the index of the node we're building on
    // for that byte in the string, plus 0 for the next byte. For
    // example, if we're inserting "abac", then `prev_leaves` goes through the
    // following phases.
    //
    // # Initial phase
    //
    // leaves = `[(0, 0)]`
    //
    //       0:("")
    //
    // # After phase 0
    //
    // leaves = `[(1, 0), (0, 0)]`
    //
    //       0:("" a)
    //             │
    //       1:("")┘
    //
    // This phase is easy enough to understand, but it's important to keep in
    // mind what the leaves mean: (1, 0) points at the parent subtree that will
    // eventually have the entire string under it. The second half of the tuple
    // points into the data field (it's empty, so, of course, 0).
    //
    // # After phase 1
    //
    // leaves = `[(1, 1), (2, 0), (0, 0)]`
    //
    //       0:("" a b)
    //             │ │
    //      1:("b")┘ │
    //               │
    //         2:("")┘
    //
    // At this point, we're now doing the interleaved insert for two strings:
    // first, the entire string, but also the substring "bac".
    //
    // This chart also demonstrates why there's a second number. The progress
    // through the `data` field needs tracked, too.
    //
    // # After phase 2
    //
    // leaves = `[(1, 2), (2, 1), (1, 0), (0, 0)]`
    //
    //        0:("" a b)
    //              │ │
    //      1:("ba")┘ │
    //                │
    //         2:("a")┘
    //
    // We've now gotten to "aba".
    //
    // # After phase 3
    //
    // leaves = `[(1, 3), (2, 2), (4, 0), (5, 0), (0, 0)]`
    //
    //          0:("" a b c)
    //                │ │ │
    //      1:("" b c)┘ │ │
    //            │ │   │ │
    //    3:("ac")┘ │   │ │
    //              │   │ │
    //        4:("")┘   │ │
    //                  │ │
    //          2:("ac")┘ │
    //                    │
    //              5:("")┘
    // We have now reached the end of the string,
    // but not the algorithm. We still step through it one more time to insert
    // the actual leaves.
    //
    // # After phase 4
    //
    // This also demonstrates the way we fix up "broken" entries in the leaves
    // array, like `leaves[0]`, that point past the end of the string: since
    // breaking a node in half only ever makes it shorter, we can fix it up by
    // looking at the spot in the string where the branch would be and walking
    // the tree.
    //
    // leaves = `[(3, 2), (2, 2), (4, 0), (5, 0), (0, 0)]`
    //
    //          0:("" a b c)
    //                │ │ │
    //      1:("" b c)┘ │ │
    //            │ │   │ │
    //    3:("ac")┘ │   │ │
    //              │   │ │
    //        4:("")┘   │ │
    //                  │ │
    //          2:("ac")┘ │
    //                    │
    //              5:("")┘
    let mut leaves: Vec<Vec<(usize, usize)>> = std::iter::repeat_with(|| vec![(0, 0)])
        .take(mapping.len())
        .collect();
    for i in 0..=max {
        for (j, (name, id)) in mapping.iter().enumerate() {
            if name.len() < i {
                continue;
            }
            for (k, (leaf_node, leaf_pos)) in leaves[j].iter_mut().enumerate() {
                if tree.nodes[*leaf_node].data.len() < *leaf_pos {
                    let c_index = i - (*leaf_pos - tree.nodes[*leaf_node].data.len());
                    *leaf_pos = tree.nodes[*leaf_node].data.len();
                    for &c in name[c_index..i].iter() {
                        (*leaf_node, *leaf_pos) =
                            insert_byte_to_search_tree(c, *leaf_node, *leaf_pos, &mut tree);
                    }
                }
                if let Some(&c) = name.get(i) {
                    (*leaf_node, *leaf_pos) =
                        insert_byte_to_search_tree(c, *leaf_node, *leaf_pos, &mut tree);
                } else {
                    insert_leaf_key(
                        *id,
                        if k == 0 {
                            SearchTreeLeafKind::Whole
                        } else if *leaf_node != 0 {
                            SearchTreeLeafKind::Suffix
                        } else {
                            continue;
                        },
                        *leaf_node,
                        *leaf_pos,
                        &mut tree,
                    );
                }
            }
            leaves[j].push((0, 0));
        }
    }

    tree
}

fn insert_leaf_key(
    id: u32,
    leaf_kind: SearchTreeLeafKind,
    leaf_node: usize,
    leaf_pos: usize,
    tree: &mut SearchTree,
) {
    let len = tree.nodes[leaf_node].data.len();
    assert!(leaf_pos <= len);
    if len == leaf_pos {
        tree.nodes[leaf_node].leaves.insert(id, leaf_kind);
    } else {
        let new_node_left = tree.nodes.len();
        let d = tree.data[tree.nodes[leaf_node].data.start + leaf_pos];
        let left = SearchTreeBranchNode {
            data: tree.nodes[leaf_node].data.start + leaf_pos + 1..tree.nodes[leaf_node].data.end,
            leaves: mem::take(&mut tree.nodes[leaf_node].leaves),
            branches: mem::take(&mut tree.nodes[leaf_node].branches),
            suffixes_only: false, // not implemented
        };
        tree.nodes[leaf_node].insert_branch(d, new_node_left);
        tree.nodes.push(left);
        tree.nodes[leaf_node].data.end = tree.nodes[leaf_node].data.start + leaf_pos;
        tree.nodes[leaf_node].leaves.insert(id, leaf_kind);
    }
}

fn insert_byte_to_search_tree(
    c: u8,
    leaf_node: usize,
    leaf_pos: usize,
    tree: &mut SearchTree,
) -> (usize, usize) {
    let len = tree.nodes[leaf_node].data.len();
    assert!(leaf_pos <= len);
    if len == leaf_pos {
        if leaf_node != 0
            && tree.nodes[leaf_node].leaves.is_empty()
            && tree.nodes[leaf_node].branches.is_empty()
        {
            if tree.data.get(tree.nodes[leaf_node].data.end) == Some(&c) {
                tree.nodes[leaf_node].data.end += 1;
            } else {
                let start = tree.data.len();
                tree.data
                    .extend_from_within(tree.nodes[leaf_node].data.clone());
                tree.data.push(c);
                tree.nodes[leaf_node].data = start..tree.data.len();
            }
            (leaf_node, leaf_pos + 1)
        } else {
            let new_node = tree.nodes.len();
            match tree.nodes[leaf_node].entry_branch(c) {
                BranchEntry::Occupied(entry) => (entry.get(), 0),
                BranchEntry::Vacant(mut entry) => {
                    entry.insert(new_node);
                    let right = SearchTreeBranchNode {
                        data: 0..0,
                        leaves: SearchTreeLeaves::default(),
                        branches: Vec::new(),
                        suffixes_only: false, // not implemented
                    };
                    tree.nodes[leaf_node].insert_branch(c, new_node);
                    tree.nodes.push(right);
                    (new_node, 0)
                }
            }
        }
    } else if tree.data[tree.nodes[leaf_node].data.start + leaf_pos] == c {
        (leaf_node, leaf_pos + 1)
    } else {
        let new_node_left = tree.nodes.len();
        let new_node_right = tree.nodes.len() + 1;
        let d = tree.data[tree.nodes[leaf_node].data.start + leaf_pos];
        let left = SearchTreeBranchNode {
            data: tree.nodes[leaf_node].data.start + leaf_pos + 1..tree.nodes[leaf_node].data.end,
            leaves: mem::take(&mut tree.nodes[leaf_node].leaves),
            branches: mem::take(&mut tree.nodes[leaf_node].branches),
            suffixes_only: false, // not implemented
        };
        tree.nodes[leaf_node].insert_branch(d, new_node_left);
        tree.nodes.push(left);
        let right = SearchTreeBranchNode {
            data: 0..0,
            leaves: SearchTreeLeaves::default(),
            branches: Vec::new(),
            suffixes_only: false, // not implemented
        };
        tree.nodes[leaf_node].insert_branch(c, new_node_right);
        tree.nodes.push(right);
        tree.nodes[leaf_node].data.end = tree.nodes[leaf_node].data.start + leaf_pos;
        (new_node_right, 0)
    }
}

pub enum CommonPrefix {
    Left,
    Right,
    Equal,
    SplitAt(usize),
}

pub fn common_prefix(left: &[u8], right: &[u8]) -> CommonPrefix {
    for (i, (x, y)) in left.iter().copied().zip(right.iter().copied()).enumerate() {
        if x != y {
            return CommonPrefix::SplitAt(i);
        }
    }
    use std::cmp::Ordering;
    match left.len().cmp(&right.len()) {
        Ordering::Less => CommonPrefix::Left,
        Ordering::Greater => CommonPrefix::Right,
        Ordering::Equal => CommonPrefix::Equal,
    }
}

#[cfg(test)]
mod tests;
