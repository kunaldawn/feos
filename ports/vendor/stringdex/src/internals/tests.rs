#[cfg(all(feature = "test_js", feature = "quickcheck"))]
use std::io::Write as _;
#[cfg(feature = "test_js")]
use std::sync::Mutex;
use std::{collections::BTreeMap, io};
#[cfg(feature = "test_js")]
pub(crate) static LOCK_JS: Mutex<()> = Mutex::new(());

#[cfg(feature = "quickcheck")]
use quickcheck::quickcheck;

#[cfg(feature = "test_js")]
use crate::internals::decode::read_base64_from_bytes;
use crate::internals::{Hasher, read_data_from_column};

use super::fs::Filesystem;

#[cfg(feature = "test_js")]
fn check_js_dictionary_lookup(map: BTreeMap<String, String>, extras: &[&str]) {
    let _lock = LOCK_JS.lock();

    use std::fs;
    use std::io::Write;
    use std::process::{Command, Stdio};
    if fs::exists("js/search.data").unwrap() && fs::remove_dir_all("js/search.data").is_err() {
        std::thread::sleep(std::time::Duration::from_secs(2));
        fs::remove_dir_all("js/search.data").unwrap();
    }
    if fs::exists("js/search.index").unwrap() && fs::remove_dir_all("js/search.index").is_err() {
        std::thread::sleep(std::time::Duration::from_secs(2));
        fs::remove_dir_all("js/search.index").unwrap();
    }
    if fs::exists("js/search-index.js").unwrap() && fs::remove_file("js/search-index.js").is_err() {
        std::thread::sleep(std::time::Duration::from_secs(2));
        fs::remove_file("js/search-index.js").unwrap();
    }
    let mut tree_root_serialized = Vec::new();
    tree_root_serialized.extend_from_slice(br#"rr_('{"name":{"I":""#);
    let tree = crate::internals::tree::encode_search_tree_ukkonen(map.keys().map(|v| v.as_bytes()));
    crate::internals::write_tree_to_disk(&tree, "js/search.index", &mut tree_root_serialized)
        .unwrap();
    tree_root_serialized.extend_from_slice(br#"","#);
    let cols = crate::internals::write_data_to_disk(
        &mut map.values().map(|v| v.as_bytes()),
        "js/search.data/name",
    )
    .unwrap();
    tree_root_serialized.extend_from_slice(&cols);
    tree_root_serialized.extend_from_slice(br#"},"desc":{"#);
    let cols = crate::internals::write_data_to_disk(
        &mut map.values().map(|v| v.as_bytes()),
        "js/search.data/desc",
    )
    .unwrap();
    tree_root_serialized.extend_from_slice(&cols);
    tree_root_serialized.extend_from_slice(br#"}}')"#);
    std::fs::write("js/search-index.js", &tree_root_serialized).unwrap();
    for name in map.keys().map(|s| &s[..]).chain(extras.iter().copied()) {
        let btree_result = map.get(name).cloned().map(String::into);
        let mut results_literal_js = None;
        let mut results_js = Command::new("node")
            .current_dir("js")
            .arg("nodesearch.js")
            .arg("lookup")
            .arg("name")
            .arg("desc")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        results_js
            .stdin
            .take()
            .unwrap()
            .write_all(name.as_bytes())
            .unwrap();
        let results_js = results_js.wait_with_output().unwrap();
        if !results_js.status.success() {
            panic!(
                "name={name:?} stdout={} stderr={}",
                String::from_utf8_lossy(&results_js.stdout),
                String::from_utf8_lossy(&results_js.stderr)
            );
        }
        let results_js = String::from_utf8(results_js.stdout).unwrap();
        for line in results_js.lines() {
            let mut out = Vec::new();
            if read_base64_from_bytes(line.as_bytes(), &mut out).is_err() {
                panic!("{results_js:?}");
            }
            results_literal_js = Some(out);
        }
        assert_eq!(results_literal_js, btree_result, "name lookup={name:?}",);
    }
}

#[cfg(feature = "test_js")]
fn map(it: &[(&str, &str)]) -> BTreeMap<String, String> {
    it.iter()
        .copied()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

#[cfg(feature = "test_js")]
#[test]
fn test_js_dictionary_lookup() {
    check_js_dictionary_lookup(
        map(&[
            ("\u{80}¦§¡\u{98}¤", ""),
            ("\u{81}¯\u{98}\u{90}\u{8a}\u{9d}", ""),
            ("\u{82}\u{89}¤\u{9d}\u{a0}¯¢", ""),
            ("\u{83}\u{89}\u{93}¡", ""),
            ("\u{8a}", ""),
            ("\u{90}", ""),
            ("\u{93}", ""),
            ("\u{a0}", ""),
            ("¢", ""),
            ("¦", ""),
            ("§", ""),
        ]),
        &[],
    );
    check_js_dictionary_lookup(
        map(&[("\0I", ""), ("\0I\0", ""), ("\0I\u{1}", ""), ("\u{1}I", "")]),
        &[],
    );
    check_js_dictionary_lookup(map(&[("a", "b")]), &["x"]);
    check_js_dictionary_lookup(map(&[("ac", "A"), ("dc", "B")]), &["a", "d", "c", "b"]);
    check_js_dictionary_lookup(map(&[("", ""), ("a", "a")]), &[]);
    check_js_dictionary_lookup(map(&[("\u{80}", ""), ("\u{81}", "")]), &[]);
    check_js_dictionary_lookup(map(&[]), &[]);
    #[allow(unused_mut)]
    let mut giant_map = BTreeMap::new();
    #[cfg(feature = "quickcheck")]
    for i in 0..4096 {
        let bytestring: String = std::iter::repeat_n(i as u8, 1024)
            .map(char::from)
            .collect::<String>();
        let key: String = i.to_string();
        giant_map.insert(key, bytestring);
    }
    check_js_dictionary_lookup(giant_map, &[]);
}

#[cfg(feature = "test_js")]
#[cfg(feature = "quickcheck")]
quickcheck! {
    fn test_rand_js_dictionary_lookup(map: BTreeMap<String, String>, extras: Vec<String>) -> quickcheck::TestResult {
        let extras: Vec<&str> = extras.iter().map(|string| &string[..]).collect();
        check_js_dictionary_lookup(map, &extras);
        quickcheck::TestResult::passed()
    }
}

fn check_behavior_column_dumpload(strings_to_insert: &[&[u8]]) {
    let mut fs = InMemFs::no_check();
    let column_root = crate::internals::write_data(&mut strings_to_insert.iter(), &mut fs).unwrap();
    let mut root = Vec::new();
    root.extend_from_slice(br#"rr_('{"mycol":{"#);
    root.extend(column_root);
    root.extend_from_slice(br#"}}')"#);
    for (k, v) in fs.files.iter() {
        eprintln!("k={k} v={}", String::from_utf8_lossy(v));
    }
    eprintln!("{fs:#?}");

    let mut collected_strings = vec![];

    fn loader(
        files: &BTreeMap<String, Vec<u8>>,
    ) -> impl FnMut(&str, &mut Vec<u8>) -> io::Result<()> {
        |filename, buf: &mut Vec<u8>| {
            let d = files
                .get(filename)
                .ok_or(io::Error::from(io::ErrorKind::NotFound))?;
            buf.extend_from_slice(d);
            Ok(())
        }
    }

    read_data_from_column::<()>(
        &root,
        b"mycol",
        &mut loader(&fs.files),
        &mut |id, cell: &[u8]| {
            assert_eq!(usize::try_from(id).unwrap(), collected_strings.len());
            collected_strings.push(cell.to_owned());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        strings_to_insert,
        collected_strings
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>()
    );
}

#[test]
fn test_column_dumpload() {
    check_behavior_column_dumpload(&[]);
    check_behavior_column_dumpload(&[b""]);
    check_behavior_column_dumpload(&[b"x", b""]);
    check_behavior_column_dumpload(&[b"", b"x", b""]);
    check_behavior_column_dumpload(&[&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]]);
    check_behavior_column_dumpload(&[&[1], &[1]]);
    check_behavior_column_dumpload(&[
        &[1],
        &[2],
        &[3],
        &[4],
        &[1],
        &[2],
        &[3],
        &[4],
        &[1],
        &[2],
        &[3],
        &[4],
    ]);
    check_behavior_column_dumpload(&[b"abcdefghijklmnopqrstuv\xffyz", b""]);
}

#[cfg(feature = "quickcheck")]
quickcheck! {
    fn test_rand_column_dumpload(strings_to_check: Vec<Vec<u8>>) -> quickcheck::TestResult {
        let strings_to_insert: Vec<&[u8]> = strings_to_check.iter().map(Vec::as_slice).collect();
        check_behavior_column_dumpload(&strings_to_insert);
        quickcheck::TestResult::passed()
    }
}

fn check_expected_column_dump(
    strings_to_insert: &[&[u8]],
    expected_root: &[u8],
    expected_fs: &[(&str, &[u8])],
) {
    let mut fs = InMemFs::no_check();
    let column_root = crate::internals::write_data(&mut strings_to_insert.iter(), &mut fs).unwrap();
    let mut root = Vec::new();
    root.extend_from_slice(br#"rr_('{"mycol":{"#);
    root.extend(column_root);
    root.extend_from_slice(br#"}}')"#);
    for (k, v) in fs.files.iter() {
        eprintln!("k={k} v={}", String::from_utf8_lossy(v));
    }
    assert_eq!(root, expected_root, "{}", String::from_utf8_lossy(&root));
    assert_eq!(
        &fs.files
            .iter()
            .map(|(name, data)| (&name[..], &data[..]))
            .collect::<Vec<_>>(),
        expected_fs
    );
}

#[test]
fn test_expected_column_dump() {
    // we dynamically switch between escaped strings and base64,
    // depending on which one's smaller.
    check_expected_column_dump(
        &[b"aaaaaaaa"],
        br##"rr_('{"mycol":{"N":"a","E":"OjAAAAAAAAA=","H":"K/UOeTU="}}')"##,
        &[("2bf50e7935", br##"rd_("haaaaaaaa")"##)],
    );
    check_expected_column_dump(
        &[b"aaaaaaa\0"],
        br##"rr_('{"mycol":{"N":"a","E":"OjAAAAAAAAA=","H":"I/BS38U="}}')"##,
        &[("23f052dfc5", br##"rd_("haaaaaaa\x00")"##)],
    );
    check_expected_column_dump(
        &[b"aaaaaa\0\0"],
        br##"rr_('{"mycol":{"N":"a","E":"OjAAAAAAAAA=","H":"t9m0xIk="}}')"##,
        &[("b7d9b4c489", br##"rb_("aGFhYWFhYQAA")"##)],
    );
}

struct InMemFs {
    files: BTreeMap<String, Vec<u8>>,
    check_existing_on_write: bool,
}

impl std::fmt::Debug for InMemFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.files.fmt(f)
    }
}

impl InMemFs {
    fn no_check() -> Self {
        Self {
            check_existing_on_write: false,
            ..Default::default()
        }
    }
}

impl Default for InMemFs {
    fn default() -> Self {
        Self {
            files: Default::default(),
            check_existing_on_write: true,
        }
    }
}

impl Filesystem for InMemFs {
    fn read_file(&mut self, file_name: &str) -> io::Result<Option<Vec<u8>>> {
        Ok(self.files.get(file_name).cloned())
    }

    fn write_file(&mut self, file_name: &str, contents: &[u8]) -> io::Result<()> {
        if self.check_existing_on_write
            && let Some(existing) = self.files.get(file_name)
        {
            assert_eq!(existing, contents);
        }
        self.files.insert(file_name.to_owned(), contents.to_owned());
        Ok(())
    }
}

#[test]
fn test_expected_serialization() {
    fn check(strings_to_insert: &[&[u8]], expected_root: &[u8], expected_fs: &[(&str, &[u8])]) {
        let mut fs = InMemFs::default();
        let tree =
            crate::internals::tree::encode_search_tree_ukkonen(strings_to_insert.iter().copied());
        let actual_root = crate::internals::write_tree(&tree, &mut fs).unwrap();
        assert_eq!(expected_root, actual_root, "{actual_root:#x?}");
        assert_eq!(
            &fs.files
                .iter()
                .map(|(filename, data)| (&filename[..], &data[..]))
                .collect::<Vec<(&str, &[u8])>>(),
            expected_fs,
        );
    }
    #[rustfmt::skip]
    check(
        &[],
        &[
            0x09, // pure suffix only (0x01) | no data or leaves (0x08)
            0x00, // SO children
        ],
        &[],
    );
    #[rustfmt::skip]
    check(
        &[b"a"],
        &[
            0x04, // has branches (0x04)
            0x81, // branch flag (one vowel, "a")
            0xc0, // no leaves (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x10, // branch leaf count (one leaf), data length (0)
            0x00, 0x00, // lower bytes of leaf
        ],
        &[],
    );

    #[rustfmt::skip]
    check(
        &[b"bc"],
        &[
            0x00, 0x80, // compression tag (), data length (0) and no-leaves flag
            0x01, // MHP children
            0x01, // SO children
            0x88, 0x00, 0x00, 0x00, 0x00, // "bc" inlined ID
            0xe0, 0x00, 0x00, 0x00, 0x00, // "c" inlined ID
            0x62, // "b"
            0x63, // "c"
        ],
        &[],
    );
    #[rustfmt::skip]
    check(
        &[&[0x00, 0xc1, 0xc1]],
        &[
            // == node for "c1" ==
            0x15, // leaf count (one leaf), has branch (0x04), suffix only (0x01)
            0x01, // branch count (one branch)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x10, // branch leaf count (one leaf), branch data length (0)
            0x00, 0x00, // lower bytes of branch leaf
            0xc1, // branch 1
            0x00, 0x00, // lower bytes of leaf
            // == node for "0" ==
            0x10, // leaf count (one leaf)
            0xc2, // inline neighbors (0xc0), data length (2)
            0xc1, 0xc1, // data
            0x00, 0x00, // upper bytes of leaf
            0x00, 0x00, // lower bytes of leaf
            // == root node ==
            0xF2, 0x80, // compressed tag (STACK, ALL) and data length and no-leaves flag
            0x01, // MHP children
            0x01, // SO children
            0x00, // child 1
            0xc1, // child 2
        ],
        &[],
    );

    #[rustfmt::skip]
    check(
        &[b"a", b"b", b"c", b"d"],
        &[
            0x04, // leaf count (0) has branch (0x04)
            0xfc, // branch flag (HEX)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x3c, // HEX bitmap 0000 0000 0011 1100 ("a", "b", "c", "d")
            0x00, 0x00, // upper bytes of leaf
            0x10, // branch leaf ("a") count (one leaf), branch data length (0)
            0x00, 0x00, // lower bytes of leaf
            0x10, // branch leaf ("b") count (one leaf), branch data length (0)
            0x01, 0x00, // lower bytes of leaf
            0x10, // branch leaf ("c") count (one leaf), branch data length (0)
            0x02, 0x00, // lower bytes of leaf
            0x10, // branch leaf ("d") count (one leaf), branch data length (0)
            0x03, 0x00, // lower bytes of leaf
        ],
        &[],
    );

    #[rustfmt::skip]
    check(
        &[b"aw", b"bx", b"cy", b"dz"],
        &[
            0x00, 0x80, // data length and no-leaves flag
            0xFC, // MHP children (hex alphabitmap representation)
            0x00, // 0000 (NONE) 0000 (NONE)
            0x3C, // 0011 (NONE) 1100 ("a", "b", "c", "d")
            0xDE, // SO children 1101 1110 (consonant-only alphabitmap representation)
            //                      - --- "w", "x", "y", "z"
            0x00, // 0000 (NONE) 0000 (NONE)
            0x00, // 0000 (NONE) 0000 (NONE)
            0x9c, 0x00, 0x00, 0x00, 0x00, // "a" inlined ID
            0x9d, 0x00, 0x00, 0x00, 0x01, // "b" inlined ID
            0x9e, 0x00, 0x00, 0x00, 0x02, // "c" inlined ID
            0x9f, 0x00, 0x00, 0x00, 0x03, // "d" inlined ID
            0xe0, 0x00, 0x00, 0x00, 0x00, // "w" inlined ID
            0xe0, 0x00, 0x00, 0x00, 0x01, // "x" inlined ID
            0xe0, 0x00, 0x00, 0x00, 0x02, // "y" inlined ID
            0xe0, 0x00, 0x00, 0x00, 0x03, // "z" inlined ID
        ],
        &[],
    );
}

#[test]
fn test_expected_serialization_with_big_id() {
    fn check(
        strings_to_insert: &[&[u8]],
        expected_root: &[u8],
        expected_fs: &[(&str, &[u8])],
        offset: u32,
    ) {
        let mut fs = InMemFs::default();
        let mut tree =
            crate::internals::tree::encode_search_tree_ukkonen(strings_to_insert.iter().copied());
        for node in &mut tree.nodes {
            use crate::internals::data_structures::TinySet;
            node.leaves.leaves_whole = TinySet::Plural(
                node.leaves
                    .leaves_whole
                    .iter()
                    .copied()
                    .map(|id| id + offset)
                    .collect::<Vec<u32>>(),
            );
            node.leaves.leaves_suffix = TinySet::Plural(
                node.leaves
                    .leaves_suffix
                    .iter()
                    .copied()
                    .map(|id| id + offset)
                    .collect::<Vec<u32>>(),
            );
        }
        let actual_root = crate::internals::write_tree(&tree, &mut fs).unwrap();
        assert_eq!(expected_root, actual_root, "{actual_root:#x?}");
        assert_eq!(
            &fs.files
                .iter()
                .map(|(filename, data)| (&filename[..], &data[..]))
                .collect::<Vec<(&str, &[u8])>>(),
            expected_fs,
        );
    }
    // single node can easily fit up to 32 bits (which is the limit of what we support)
    #[rustfmt::skip]
    check(
        &[b"b"],
        &[
            0x04, // leaf count (0) has branches
            0x01, // branch_count (one branch)
            0xc0, // data length (0), neighbors inline flag (0xc0)
            0x00, 0x10, // upper bits of leaf ID
            0x10, // branch data length (0), leaf count (one leaf)
            0x00, 0x00, // lower bits of leaf ID
            0x62, // "b"
        ],
        &[],
        0x10000000,
    );
    #[rustfmt::skip]
    check(
        &[b"cb"],
        &[
            // = root node =
            0x00, 0x80, // data length and no-leaves flag
            0x01, // MHP children
            0x01, // SO children
            0x87, 0x10, 0x00, 0x00, 0x00, // "c" inlined ID
            0xe0, 0x10, 0x00, 0x00, 0x00, // "a" inlined ID
            0x63, // "c"
            0x62, // "b"
        ],
        &[],
        0x10000000,
    );
    // two nodes, however, can only fit 28 bits
    #[rustfmt::skip]
    check(
        &[b"c", b"c", b"bd"],
        &[ // 22 bytes
            // = root node =
            0x00, // no compression
            0x80, // data length and no-leaves flag
            0x02, // mhp length
            0x01, // so length
            0x89, 0x10, 0x00, 0x00, 0x02, // inline "b" (payload "d")
            0xa1, 0x10, 0x00, 0x00, 0x00, // inline "c"
            0xe0, 0x10, 0x00, 0x00, 0x02, // inline "d"
            0x62, // "b"
            0x63, // "c"
            0x64, // "d"
        ],
        &[],
        0x10000000,
    );
    #[rustfmt::skip]
    check(
        &[b"c", b"c", b"bd"],
        &[ // 25 bytes
            // = inline node "c" =
            0x00, 0x40, // no compression, data length 0, no-branches flag
            0xd2, // prefix leaves
            0xff, 0xff, 0xff, 0x0f,
            0x01,
            0x00, // suffix leaves
            // = root node =
            0x22, 0x80, // compressed (2), stack (0x02), data length (0) and no-leaves flag (0x80)
            0x02, // MHP children
            0x01, // SO children
            0x89, 0x10, 0x00, 0x00, 0x01, // "b" inlined ID
            // stack "c"
            0xe0, 0x10, 0x00, 0x00, 0x01, // "d" inlined ID
            0x62, // "b"
            0x63, // "c"
            0x64, // "d"
        ],
        &[],
        0xfffffff,
    );
    #[rustfmt::skip]
    check(
        &[b"b", b"b"],
        &[
            0x04, // leaf count (0), has branches (0x04)
            0x01, // branch_count (one branch)
            0xc0, // data length (0), neighbors inline flag (0xc0)
            0x00, 0x10, // upper bits of leaf ID
            0xa0, // branch data length (0), leaves as run (0x80) leaf count (two leaves)
            0x00, 0x00, // lower bits of leaf ID
            0x62, // "b"
        ],
        &[],
        0x10000000,
    );
    #[rustfmt::skip]
    check(
        &[b"b", b"b"],
        &[
            // = inline node =
            0x00, 0x40, // compressed (), data length (0) and no branches flag (0x40)
            0xd2, // prefix leaves
            0xff, 0xff, 0xff, 0x0f,
            0x01,
            0x00, // suffix leaves
            // = root node =
            0xf2, 0x80, // compressed (ALL), stack compressed (0x02), data length and no-leaves flag
            0x01, // MHP children
            0x00, // SO children
            // stack "a"
            0x62, // "b"
        ],
        &[],
        0xfffffff,
    );
    // two nodes plus one char can only fit 20 bits, not 28
    #[rustfmt::skip]
    check(
        &[b"bx", b"bx"],
        &[
            // = interior node "x" =
            0x01, 0x40, // data length (0), no branches flag (0x40), suffix only (0x01)
            // leaves
            0xd2, 0xff, 0xff, 0xff, 0x0f, 0x01,
            // = internal node "b" =
            0x00, 0x41, // data length, no branches flag (0x40)
            0x78, // x
            // leaves
            0xd2, 0xff, 0xff, 0xff, 0x0f, 0x01,
            0x00,
            // = root node =
            0xf2, 0x80, // compressed (ALL), data length and no-leaves flag
            0x01, // MHP children
            0x01, // SO children
            0x62, // "a"
            0x78, // "x"
        ],
        &[],
        0xfffffff,
    );
    #[rustfmt::skip]
    check(
        &[b"bx", b"bx"],
        &[
            // = interior node "x" =
            0x01, 0x40, // suffix only (0x01), no branches (0x40)
            0xd2, 0xff, 0xff, 0x0f, 0x00, 0x01, // leaves
            // = interior node "b" =
            0x00, 0x41, // no branches (0x40), data length (1)
            0x78, // "x"
            0xd2, 0xff, 0xff, 0x0f, 0x00, 0x01, // leaves
            0x00, // leaves
            // = root node =
            0xf2, 0x80, // compressed (ALL), stack (0x02), data length and no-leaves flag
            0x01, // MHP children
            0x01, // SO children
            0x62, // "b"
            0x78, // "x"
        ],
        &[],
        0xfffff,
    );

    #[rustfmt::skip]
    check(
        &[b"bc", b"b", b"b", b"b"],
        &[
            // = internal node "a" =
            0x00, // not compressed
            0x00, // data length (0)
            0x01, // MHP length (1)
            0x00, // SO length (0)
            0xa0, 0x00, 0xf, 0xff, 0xff, // inlined ID ("bc")
            0x63, // "c"
            0x83, 0x00, 0x00, 0x10, 0x00, // prefix branches (run of three starting at 0x0010_0000)
            0x00, // suffix branches (none)
            // = root node =
            0x12, // compressed (1), stack compressed (0x02)
            0x80, // no leaves (0x80), data length (0)
            0x01, // MHP length (1)
            0x01, // SO length (1)
            0xe0, 0x00, 0x0f, 0xff, 0xff, // inlined ID ("c")
            0x62, // "b"
            0x63, // "c"
        ],
        &[],
        0xfffff,
    );
    let mut strings_to_insert = vec![&b""[..]];
    for _ in 0..0x1_0000 {
        strings_to_insert.push(b"\0");
    }
    strings_to_insert.push(b"");
    #[rustfmt::skip]
    check(
        &strings_to_insert,
        &[
            // == "\0" ==
            0x00, 0x40, // compressed (), data length and no-no flag (0x40)
            0x3b, 0x30, // prefix leaves (roaring bitmap, with runs)
            0x00, 0x00, // countainer count - 1 (one container)
            0x01, // is run flags (1)
            0x00, 0x01, 0xff, 0xff, // key (0x00, 0x01) cardinality - 1 (65536)
            0x01, 0x00, // run count (one run)
            0x00, 0x00, // run start
            0xff, 0xff, // run length
            0x00, // suffix leaves (none)
            // == root ==
            0xf2, 0x00, // compressed (ALL), stack compressed (0x02), data length (0)
            0x01, // MHP children
            0x00, // SO children
            0x00, // b"\0" (child 1)
            0x02, // prefix leaves
            0xff, 0xff, 0xff, 0x00,
            0x00, 0x00, 0x01, 0x01,
            0x00, // suffix leaves
        ],
        &[],
        0xffffff,
    );
}

struct TestHasher;

impl Hasher for TestHasher {
    fn hash(input: &[u8]) -> u64 {
        let mut output = 0u64;
        for &byte in input {
            output = output.wrapping_add(byte as u64) & 0xff;
        }
        output
    }
}

// test the expected serialization with a deliberately weak hash
// the tree serializer has logic to handle hash collisions, and this
// is the easiest way to test that
#[test]
fn test_expected_serialization_with_weakened_hash() {
    fn check(strings_to_insert: &[&[u8]], expected_root: &[u8], expected_fs: &[(&str, &[u8])]) {
        let mut fs = InMemFs::no_check();
        let tree =
            crate::internals::tree::encode_search_tree_ukkonen(strings_to_insert.iter().copied());
        let actual_root =
            crate::internals::write_tree_with_custom_hasher::<TestHasher>(&tree, &mut fs).unwrap();
        assert_eq!(expected_root, actual_root, "{:#x?}", actual_root,);
        assert_eq!(
            &fs.files
                .iter()
                .map(|(filename, data)| (&filename[..], &data[..]))
                .collect::<Vec<(&str, &[u8])>>(),
            expected_fs,
            "{:#x?}",
            &fs.files,
        );
    }
    #[rustfmt::skip]
    check(
        &[b"ABCE", b"ACBE", b"XN", b"XN\0", b"WJ", b"WJ\0"],
        &[
            // = interior node for "N" =
            0x15, // leaf count (1), suffix (0x01), has branches 0x04
            0x01, // branch count (one branch)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x10, // branch leaf count (one leaf), branch data length (0)
            0x03, 0x00, // lower bytes of branch leaf ("\0")
            0x00, // branch 1 ("\0")
            0x02, 0x00, // lower bytes of leaf
            // = interior node for "J" =
            0x15, // leaf count (1), suffix (0x01), has branches (0x04)
            0x01, // branch count (one branch)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x10, // branch leaf count (one leaf), branch data length (0)
            0x05, 0x00, // lower bytes of branch leaf ("\0")
            0x00, // branch 1 ("\0")
            0x04, 0x00, // lower bytes of leaf
            // = interior node for "C" =
            0x05, // leaf count (0), suffix (0x01), has branches (0x04)
            0x02, // branch count (two branches)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x11, // branch leaf count (one leaf), branch data length (1)
            0x01, 0x00, // lower bytes of branch leaf ("B")
            0x10, // branch leaf count (one leaf), branch data length (0)
            0x00, 0x00, // lower bytes of branch leaf ("E")
            0x42, // branch 1 ("B")
            0x45, // branch 2 ("E")
            // = interior node for "B" =
            0x05, // leaf count (0), suffix (0x01), has branches (0x04)
            0x02, // branch count (two branches)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x11, // branch leaf count (one leaf), branch data length (1)
            0x00, 0x00, // lower bytes of branch leaf ("C")
            0x10, // branch leaf count (one leaf), branch data length (0)
            0x01, 0x00, // lower bytes of branch leaf ("E")
            0x43, // child 1 ("C")
            0x45, // child 2 ("E")
            // = interior node for "\0" =
            0x21, // leaf count (2), suffix (0x01)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x03, 0x00, // lower bytes of leaf
            0x05, 0x00, // lower bytes of leaf
            // = interior node for "X" =
            0x14, // leaf count (1), has branches (0x04)
            0x01, // branch count (one branch)
            0xc1, // inline neighbors (0xc0), data length (1)
            0x4e, // data "N"
            0x00, 0x00, // upper bytes of leaf
            0x10, // branch leaf count (one leaf), branch data length (0)
            0x03, 0x00, // lower bytes of branch leaf ("\0")
            0x00, // child 1 ("\0")
            0x02, 0x00, // lower bytes of leaf
            // = interior node for "A" =
            0x04, // leaf count (0), has branches (0x04)
            0x02, // branch count (two branches)
            0xc0, // inline neighbors (0xc0), no leaves (0x80), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x12, // branch leaf ("B") count (one leaf), branch data length (2)
            0x43, // data "C"
            0x45, // data "E"
            0x00, 0x00, // lower bytes of leaf
            0x12, // branch leaf ("C") count (one leaf), branch data length (2)
            0x42, // data "B"
            0x45, // data "E"
            0x01, 0x00, // lower bytes of leaf
            0x42, // branch "B"
            0x43, // branch "C"
            // = root note =
            0xd6, 0x1b, // compressed tag (LONG, STACK, 1, 3, 4, 5, 9, 10, 12)
            0x80, // data length (0) and no-leaves flag (0x80)
            0x03, // MHP children
            0x06, // SO children
            // stack 1 ("A")
            0x00, 0x00, 0x00, 0x00, 0x1, // hash 2 ("W")
            // stack 3 ("X")
            // stack 4 ("\0")
            // stack 5 ("B")
            // stack 6 ("C")
            0xe1, 0x00, 0x00, 0x00, 0x00, // inline 7 ("E")
            // stack 8 ("J")
            // stack 9 ("N")
            0x41, // child 1 ("A")
            0x57, // child 2 ("W")
            0x58, // child 3 ("X")
            0x00, // child 4 ("\0")
            0x42, // child 5 ("B")
            0x43, // child 6 ("C")
            0x45, // child 7 ("E")
            0x4a, // child 8 ("J")
            0x4e, // child 9 ("N")
        ],
        &[
            (
                "0000000001",
                &[
                    // = dummy node =
                    // the whole point of this test case is to assure that we actually generate and use this
                    0x09, // pure suffix only (0x01) | no data or leaves (0x08)
                    0x00, // SO children
                    // = interior node for "W" =
                    0x22, 0x01, // compressed tag (STACK, 2) and data length (1)
                    0x4a, // data "J"
                    0x01, // MHP children
                    0x01, // SO children
                    0xa0, 0x00, 0x00, 0x00, 0x05, // inline 1 "\0"
                    0x00, // child 1 "\0"
                    0x01, // child 2 (dummy)
                    // leaves
                    0x01, 0x04, 0x00, 0x00, 0x00,
                    0x00,
                ],
            ),
        ],
    );
    #[rustfmt::skip]
    check(
        &[b"AABCE", b"AACBE"],
        &[
            // = interior node for "C" =
            0x05, // leaf count (0), suffix only (0x01), has branches (0x04)
            0x02, // branch count (two branches)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x11, // branch leaf ("B") count (one branch), branch data length (1)
            0x01, 0x00, // lower bytes of branch leaf
            0x10, // branch leaf ("E") count (one branch), branch data length (0)
            0x00, 0x00, // lower bytes of branch leaf
            0x42, // child 1 ("B")
            0x45, // child 2 ("E")
            // interior node for "B" =
            0x05, // leaf count (0), suffix only (0x01), has branches (0x04)
            0x02, // branch count (two branches)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x11, // branch leaf ("C") count (one branch), branch data length (1)
            0x00, 0x00, // lower bytes of branch leaf
            0x10, // branch leaf ("E") count (one branch), branch data length (0)
            0x01, 0x00, // lower bytes of branch leaf
            0x43, // branch 1 ("C")
            0x45, // branch 2 ("E")
            // = interior node for "AA" =
            0x04, // leaf count (0), has branches (0x04)
            0x02, // branch count (two branches)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x12, // branch leaf ("B") count (one branch), branch data length (2)
            0x43, // branch data "C"
            0x45, // branch data "E"
            0x00, 0x00, // lower bytes of branch leaf
            0x12, // branch leaf ("C") count (one branch), branch data length (2)
            0x42, // branch data "B"
            0x45, // branch data "E"
            0x01, 0x00, // lower bytes of branch leaf
            0x42, // branch 1 ("B")
            0x43, // branch 2 ("C")
            // = interior node for "A" =
            0x12, 0x80, // compressed tag (1, ALL) and data length (0) and no-leaves flag
            0x01, // MHP children (1)
            0x02, // SO children (2)
            // stack 1 "A"
            0xc1, 0x00, 0x00, 0x00, 0x00, // inline 2 "B"
            0xc1, 0x00, 0x00, 0x00, 0x01, // inline 2 "C"
            0x41, // child 1 "A"
            0x42, // child 2 "B"
            0x43, // child 3 "C"
            // = root node =
            0x72,  0x80, // compressed tag (STACK, 1, 2, 3) and data length (0) and no-leaves flag
            0x01, // MHP children (1)
            0x03, // SO children (3)
            0xe1, 0x00, 0x00, 0x00, 0x00, // inline 3 "E"
            0x41, // child 1 "A"
            0x42, // child 2 "B"
            0x43, // child 3 "C"
            0x45, // child 4 "E"
        ],
        &[],
    );
    #[rustfmt::skip]
    check(
        &[
            b"XR", b"XR\0", b"WJ", b"WJ\0", b"QXR", b"QXR\x02", b"QWJ", b"QWJ\x02",
        ],
        &[
            // = interior node for "R" =
            0x25, // suffix only (0x01), has branches (0x04), leaves count (two leaves)
            0x02, // branch count (two branches)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x10, // branch leaf ("\0") count (one leaf), branch data length (0)
            0x01, 0x00, // lower bytes of branch leaf
            0x10, // branch leaf ("\x02") count (one leaf), branch data length (0)
            0x05, 0x00, // lower bytes of branch leaf
            0x00, // branch 1 ("\0")
            0x02, // branch 2 ("\x02")
            0x00, 0x00, // lower bytes of branch leaf
            0x04, 0x00, // lower bytes of branch leaf
            // = interior node for "J" =
            0x25, // suffix only (0x01), has branches (0x04), leaves count (two leaves)
            0x02, // branch count (two branches)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x10, // branch leaf ("\0") count (one leaf), branch data length (0)
            0x03, 0x00, // lower bytes of branch leaf
            0x10, // branch leaf ("\x02") count (one leaf), branch data length (0)
            0x07, 0x00, // lower bytes of branch leaf
            0x00, // branch 1 ("\0")
            0x02, // branch 2 ("\x02")
            0x02, 0x00, // lower bytes of branch leaf
            0x06, 0x00, // lower bytes of branch leaf
            // = interior node for "\x02" =
            0x21, // leaves count (2), suffix only (0x01)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x05, 0x00, // lower bytes of leaf
            0x07, 0x00, // lower bytes of leaf
            // = interior node for "\0" =
            0x21, // leaves count (2), suffix only (0x01)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x01, 0x00, // lower bytes of leaf
            0x03, 0x00, // lower bytes of leaf
            // = interior node for "X" =
            0x00, 0x01, // compressed tag () and data length (1)
            0x52, // data "R"
            0x01, // MHP children
            0x01, // SO children
            0xa0, 0x00, 0x00, 0x00, 0x01, // inline 1 "\0"
            0xe0, 0x00, 0x00, 0x00, 0x05, // inline 1 "\x02"
            0x00, // child 1 ("\0")
            0x02, // child 2 ("\x02")
            // leaves
            0x01, 0x00, 0x00, 0x00, 0x00,
            0x01, 0x04, 0x00, 0x00, 0x00,
            // = interior node for "QX" =
            0x1c, // dictionary compressed (0x08), has branches (0x04), leaves count (one leaf)
            0x01, // branches count (one branch)
            0xc0, // inline neighbors (0xc0), data offset ("R")
            0x00, 0x00, // upper bytes of leaf
            0x10, // branch leaf ("\x02") count (one leaf), branch data length (0)
            0x05, 0x00, // lower bytes of branch leaf
            0x02, // branch 1 ("\x02")
            0x04, 0x00, // lower bytes of leaf
            // = interior node for "QW" =
            0x14, // has branches (0x04), leaves count (one leaf)
            0x01, // branches count (one branch)
            0xc1, // inline neighbors (0xc0), data length (1)
            0x4a, // data "J"
            0x00, 0x00, // upper bytes of leaf
            0x10, // branch leaf ("\x02") count (one leaf), branch data length (0)
            0x07, 0x00, // lower bytes of branch leaf
            0x02, // branch 1 ("\x02")
            0x06, 0x00, // lower bytes of leaf
            // = interior node for "Q" =
            0xF2, 0x80, // compressed tag (STACK, ALL) and data length (0) and no-leaves flag
            0x02, // MHP children
            0x00, // SO children
            0x57, // child 1 ("w")
            0x58, // child 2 ("x")
            // = root node =
            // 1101 0111
            0xd6, 0x07, // compressed tag (STACK, LONG, 1 3 4 5 6 7)
            0x80, // data length and no-leaves flag
            0x03, // MHP children
            0x04, // SO children
            // stack 1 ("q")
            0x00, 0x00, 0x00, 0x00, 0xee, // hash 2 ("w")
            // stack 3 ("X")
            // stack 4 ("\0")
            // stack 5 ("\x02")
            // stack 6 ("J")
            // stack 7 ("R")
            0x51, // child 1 ("Q")
            0x57, // child 2 ("W")
            0x58, // child 3 ("X")
            0x00, // child 4 ("\0")
            0x02, // child 5 ("\x02")
            0x4a, // child 6 ("J")
            0x52, // child 7 ("R")
        ],
        &[(
            "00000000ee",
            &[
                // = dummy node =
                // the whole point of this test case is to assure that we actually generate and use this
                0x09, // pure suffix only (0x01) | no data or leaves (0x08)
                0x00, // SO children
                // = interior node for "W" =
                0x22, 0x01, // compressed tag (STACK, 2) and data length (1)
                0x4a, // "J"
                0x01, // MHP children
                0x02, // SO children
                0xa0, 0x00, 0x00, 0x00, 0x03, // inline 1 "\0"
                0xe0, 0x00, 0x00, 0x00, 0x07, // inline 3 "\x02"
                0x00, // child 1 ("\0")
                0x01, // child 2 dummy
                0x02, // child 3 ("\x02")
                // leaves
                0x01, 0x02, 0x00, 0x00, 0x00,
                0x01, 0x06, 0x00, 0x00, 0x00,
            ],
        )],
    );
    #[rustfmt::skip]
    check(
        &[b"AABCE", b"AACBE", b"XAA"],
        &[
            // = interior node for "C" =
            0x05, // leaf count (0), suffix only (0x01), has branches (0x04)
            0x02, // branch count (two branches)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x11, // branch leaf ("B") count (one branch), branch data length (1)
            0x01, 0x00, // lower bytes of branch leaf
            0x10, // branch leaf ("E") count (one branch), branch data length (0)
            0x00, 0x00, // lower bytes of branch leaf
            0x42, // child 1 ("B")
            0x45, // child 2 ("E")
            // = interior node for "B" =
            0x05, // leaf count (0), suffix only (0x01), has branches (0x04)
            0x02, // branch count (two branches)
            0xc0, // inline neighbors (0xc0), data length (0)
            0x00, 0x00, // upper bytes of leaf
            0x11, // branch leaf ("C") count (one branch), branch data length (1)
            0x00, 0x00, // lower bytes of branch leaf
            0x10, // branch leaf ("E") count (one branch), branch data length (0)
            0x01, 0x00, // lower bytes of branch leaf
            0x43, // branch 1 ("C")
            0x45, // branch 2 ("E")
            // = interior node for "X" =
            0x10, // leaf count (one leaf)
            0xc2, // inline neighbors (0xc0), data length (2)
            0x41, // data "A"
            0x41, // data "A"
            0x00, 0x00, // upper bytes of leaf
            0x02, 0x00, // lower bytes of leaf
            // = interior node for "AAC" =
            0x10, // leaf count (one leaf)
            0xc2, // inline neighbors (0xc0), data length (2)
            0x42, // data "B"
            0x45, // data "E"
            0x00, 0x00, // upper bytes of leaf
            0x01, 0x00, // lower bytes of leaf
            // = interior node for "AA" =
            0x22, // compressed (2) stack compressed (0x02)
            0x00, // data length (0)
            0x02, // MHP children (2)
            0x00, // SO children (0)
            0x00, 0x00, 0x00, 0x00, 0x95, // hash 1 "B"
            // stack 2 "C"
            0x42, // child 1 "B"
            0x43, // child 2 "C"
            // leaves
            0x00,
            0x01, 0x02, 0x00, 0x00, 0x00,
            // = interior node for "A" =
            0x12, 0x00, // compressed tag (1, ALL) and data length (0)
            0x01, // MHP children (1)
            0x02, // SO children (2)
            // stack 1 "A"
            0xc1, 0x00, 0x00, 0x00, 0x00, // inline 2 "B"
            0xc1, 0x00, 0x00, 0x00, 0x01, // inline 2 "C"
            0x41, // child 1 "A"
            0x42, // child 2 "B"
            0x43, // child 3 "C"
            0x00,
            0x01, 0x02, 0x00, 0x00, 0x00,
            // = root node =
            0xf6, // compressed tag (1, 2, 3, 4), stack compressed (0x02), long compressed (0x04)
            0x00, // compressed tag ()
            0x80, // no-leaves tag (0x80), data length (0)
            0x02, // MHP children (1)
            0x03, // SO children (3)
            // stack 1 "A"
            // stack 2 "X"
            // stack 3 "B"
            // stack 4 "C"
            0xe1, 0x00, 0x00, 0x00, 0x00, // inline 3 "E"
            0x41, // child 1 "A"
            0x58, // child 2 "X"
            0x42, // child 3 "B"
            0x43, // child 4 "C"
            0x45, // child 5 "E"
        ],
        &[(
            "0000000095",
            &[
                // = dummy node =
                // the whole point of this test case is to assure that we actually generate and use this
                0x09, // pure suffix only (0x01) | no data or leaves (0x08)
                0x00, // SO children
                // = interior node for "AAB" =
                0xf2, 0x02, // compressed tag (STACK, ALL) and data length (2)
                0x43, // "C"
                0x45, // "E"
                0x00, // MHP children
                0x01, // SO children
                0x00, // child 1 ("\0")
                // leaves
                0x01, 0x00, 0x00, 0x00, 0x00,
                0x00,
            ],
        )],
    );
}

fn check_serialization_no_dups(strings_to_insert: &[&[u8]]) {
    let strings_to_insert: Vec<&[u8]> = strings_to_insert.iter().map(|x| &x[..]).collect();
    let mut fs = InMemFs::default();
    let tree =
        crate::internals::tree::encode_search_tree_ukkonen(strings_to_insert.iter().copied());
    crate::internals::write_tree(&tree, &mut fs).unwrap();
}

#[test]
fn test_serialization_no_dups() {
    let mut fibonacci_words = vec![];
    for bytelen in [1 << 16] {
        let mut word = Vec::with_capacity(usize::try_from(bytelen).unwrap());
        let mut n = 0i64;
        for _ in 0..bytelen {
            let mut byte = 0u8;
            const PHI: f64 = 1.618033988749894;
            for offset in 0..8 {
                byte |= 1
                    << (offset
                        * (2.0 + f64::floor(n as f64 * PHI) - f64::floor((n + 1) as f64 * PHI))
                            as i32);
                n += 1;
            }
            word.push(byte);
        }
        fibonacci_words.push(word);
    }
    let fibonacci_words: Vec<&[u8]> = fibonacci_words.iter().map(Vec::as_slice).collect();
    check_serialization_no_dups(&fibonacci_words[..]);
}

#[cfg(feature = "quickcheck")]
quickcheck! {
    fn test_rand_serialization_no_dups(strings_to_insert: Vec<Vec<u8>>) -> quickcheck::TestResult {
        let strings_to_insert: Vec<&[u8]> = strings_to_insert.iter().map(|x| &x[..]).collect();
        check_serialization_no_dups(&strings_to_insert);
        quickcheck::TestResult::passed()
    }
}
