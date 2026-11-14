use crate::internals::tree::{
    CommonPrefix, SearchTree, common_prefix, encode_search_tree_naive, encode_search_tree_ukkonen,
};
#[cfg(feature = "quickcheck")]
use quickcheck::quickcheck;
use std::collections::HashSet;

/// Testing tool for the suffix tree implementation.
///
/// The actual, deployed version is in search.js
fn get_from_search_tree(
    name: &[u8],
    find_substrings: bool,
    tree: &SearchTree,
    active_idx: usize,
) -> Vec<u32> {
    match common_prefix(&tree.data[tree.nodes[active_idx].data.clone()], name) {
        CommonPrefix::Equal if !find_substrings => {
            // Tree node is an exact match for the query,
            // and we are looking only for exact matches.
            //
            // Return only exact matches.
            tree.nodes[active_idx]
                .leaves
                .leaves_whole
                .as_slice()
                .to_owned()
        }
        CommonPrefix::Right | CommonPrefix::Equal => {
            // Query is an empty-string-inclusive prefix of the tree node.
            // This is the base case.
            if find_substrings {
                // If this is a substring search, then return everything.
                let mut result = Vec::new();
                fn walk(tree: &SearchTree, active_idx: usize, result: &mut Vec<u32>) {
                    result.extend_from_slice(&tree.nodes[active_idx].leaves.leaves_whole);
                    result.extend_from_slice(&tree.nodes[active_idx].leaves.leaves_suffix);
                    for branch in tree.nodes[active_idx].branch_values() {
                        walk(tree, branch, result);
                    }
                }
                walk(tree, active_idx, &mut result);
                result.sort();
                result.dedup();
                result
            } else {
                // We are looking for only exact matches, and
                // we have not found one.
                Vec::new()
            }
        }
        CommonPrefix::Left => {
            // Tree node is a prefix of the query.
            // This is the recursive case.
            let n = tree.nodes[active_idx].data.len();
            let c = name[n];
            match tree.nodes[active_idx].get_branch(c) {
                // There exists a branch that may contain this substring.
                Some(idx) => stacker::maybe_grow(32 * 1024, 1024 * 1024, || {
                    get_from_search_tree(&name[n + 1..], find_substrings, tree, idx)
                }),
                // This node has no children that start with the queried name.
                None => Vec::new(),
            }
        }
        CommonPrefix::SplitAt(_) => {
            // This node does not contain our query.
            Vec::new()
        }
    }
}

#[cfg(feature = "test_js")]
use std::{io::Write as _, process::Stdio};

#[cfg(feature = "test_js")]
use crate::internals::tests::LOCK_JS;

fn check_behavior(strings_to_insert: &[&[u8]], strings_to_check: &[&[u8]]) {
    check_behavior_offset(strings_to_insert, strings_to_check, 0);
}

fn check_behavior_offset(strings_to_insert: &[&[u8]], strings_to_check: &[&[u8]], offset: u32) {
    let naive_array: Vec<Vec<u8>> = strings_to_insert
        .iter()
        .map(|&str| str.to_owned())
        .collect();
    fn get_from_naive_array(
        name: &[u8],
        find_substrings: bool,
        naive_array: &[Vec<u8>],
        offset: u32,
    ) -> Vec<u32> {
        let mut result = Vec::new();
        for (i, entry) in naive_array.iter().enumerate() {
            let id = i as u32;
            let matches = if find_substrings {
                let mut found = false;
                for sub in 0..=entry.len() {
                    if entry[sub..].starts_with(name) {
                        found = true;
                    }
                }
                found
            } else {
                entry == name
            };
            if matches {
                result.push(id + offset);
            }
        }
        result.sort();
        result.dedup();
        result
    }
    let mut tree = encode_search_tree_ukkonen(strings_to_insert.iter().map(|string| &string[..]));
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
    for name in HashSet::<&&[u8]>::from_iter(strings_to_check.iter().chain(strings_to_insert)) {
        let results_literal = get_from_search_tree(name, false, &tree, 0);
        if results_literal != get_from_naive_array(name, false, &naive_array, offset) {
            println!("{tree:#?}");
        }
        assert_eq!(
            get_from_naive_array(name, false, &naive_array, offset),
            results_literal,
            "name naive whole={name:?}",
        );
        let results_substring = get_from_search_tree(name, true, &tree, 0);
        if results_substring != get_from_naive_array(name, true, &naive_array, offset) {
            println!("{tree:#?}");
        }
        assert_eq!(
            get_from_naive_array(name, true, &naive_array, offset),
            results_substring,
            "name naive substring={name:?}",
        );
        #[cfg(feature = "test_js")]
        {
            let _lock = LOCK_JS.lock();
            use std::fs;
            use std::process::Command;
            if fs::exists("js/search.index").unwrap()
                && fs::remove_dir_all("js/search.index").is_err()
            {
                std::thread::sleep(std::time::Duration::from_secs(2));
                fs::remove_dir_all("js/search.index").unwrap();
            }
            if fs::exists("js/search.data").unwrap()
                && fs::remove_dir_all("js/search.data").is_err()
            {
                std::thread::sleep(std::time::Duration::from_secs(2));
                fs::remove_dir_all("js/search.data").unwrap();
            }
            if fs::exists("js/search-index.js").unwrap()
                && fs::remove_file("js/search-index.js").is_err()
            {
                std::thread::sleep(std::time::Duration::from_secs(2));
                fs::remove_file("js/search-index.js").unwrap();
            }
            let mut tree_root_serialized = Vec::new();
            tree_root_serialized.extend_from_slice(br#"rr_('{"name":{"I":""#);
            crate::internals::write_tree_to_disk(
                &tree,
                "js/search.index",
                &mut tree_root_serialized,
            )
            .unwrap();
            tree_root_serialized.extend_from_slice(br#"","#);
            let cols = crate::internals::write_data_to_disk(
                std::iter::repeat_n(&b""[..], offset as usize)
                    .chain(strings_to_insert.iter().copied()),
                "js/search.data/name",
            )
            .unwrap();
            tree_root_serialized.extend_from_slice(&cols);
            tree_root_serialized.extend_from_slice(br#"}}')"#);
            std::fs::write("js/search-index.js", &tree_root_serialized).unwrap();
            let mut results_literal_js = Vec::new();
            let mut results_js = Command::new("node")
                .current_dir("js")
                .arg("nodesearch.js")
                .arg("exact")
                .arg("name")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            results_js.stdin.take().unwrap().write_all(name).unwrap();
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
                if line.is_empty() {
                    continue;
                }
                eprintln!("{line}");
                if let Ok(num) = line.parse::<u32>() {
                    results_literal_js.push(num);
                } else {
                    let mut f = std::fs::File::create("f").unwrap();
                    for x in strings_to_insert {
                        f.write_all(&[
                            ((x.len() >> 24) & 0xff) as u8,
                            ((x.len() >> 16) & 0xff) as u8,
                            ((x.len() >> 8) & 0xff) as u8,
                            (x.len() & 0xff) as u8,
                        ])
                        .unwrap();
                        f.write_all(&x[..]).unwrap();
                    }
                    f.write_all(&[0xff, 0xff, 0xff, 0xff]).unwrap();
                    for x in strings_to_check {
                        f.write_all(&[
                            ((x.len() >> 24) & 0xff) as u8,
                            ((x.len() >> 16) & 0xff) as u8,
                            ((x.len() >> 8) & 0xff) as u8,
                            (x.len() & 0xff) as u8,
                        ])
                        .unwrap();
                        f.write_all(&x[..]).unwrap();
                    }
                    std::mem::drop(f);
                    panic!("literal match out {results_js}");
                }
            }
            assert_eq!(results_literal_js, results_literal, "name exact={name:?}",);
            let mut results_substring_js = Vec::new();
            let mut results_js = Command::new("node")
                .current_dir("js")
                //.arg("--inspect")
                .arg("nodesearch.js")
                .arg("substring")
                .arg("name")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            results_js.stdin.take().unwrap().write_all(name).unwrap();
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
                if line.is_empty() {
                    continue;
                }
                eprintln!("{line}");
                if let Ok(num) = line.parse::<u32>() {
                    results_substring_js.push(num);
                } else {
                    panic!("literal match out {results_js}");
                }
            }
            assert_eq!(
                results_substring_js, results_substring,
                "name substring={name:?}",
            );
        }
    }
}

#[rustfmt::skip]
#[test]
fn test_search_tree() {
    {
        check_behavior(
            &[
                &[3], &[3], &[3],
                &[0],
                &[3], &[3], &[3],
            ],
            &[&[3]],
        );
        check_behavior(
            &[
                &[3], &[3], &[3],
                &[0],
                &[3], &[3], &[3],
                &[0],
                &[3], &[3], &[3],
            ],
            &[&[3]],
        );
    }
    check_behavior(
        &[&[33], &(0..32).collect::<Vec<u8>>()],
        &[&[32]],
    );
    check_behavior(
        &[&[3], &[0, 1]],
        &[&[2]],
    );
    {
        // corner case where we have two a domain with fewer than 16 entries,
        // but that can't use either of the special reprs
        let mut v = vec![];
        for _ in 0..0x0f {
            v.push(&b"ab"[..]);
            v.extend(std::iter::repeat_n(&b""[..], 0xff_ff));
        }
        check_behavior_offset(
            &v,
            &[b"a", b"b"],
            0xfffff,
        );
    }
    check_behavior_offset(
        &[b"ab", b"a", b"a", b"a"],
        &[b"b"],
        0xfffff,
    );
    {
        // corner case where two-item inlining might be possible, but
        // the difference between the first and second item is too big
        let mut v = vec![&b"ab"[..], b"ab"];
        v.extend(std::iter::repeat_n(&b""[..], 4096));
        v.extend([&b"ab"[..], b"c"].into_iter());
        check_behavior_offset(
            &v,
            &[b"a", b"b"],
            0xfffff,
        );
        v.swap(1, 4097);
        check_behavior_offset(
            &v,
            &[b"a", b"b"],
            0xfffff,
        );
    }
    check_behavior_offset(
        &[b"ab", b"a", b"a", b"a"],
        &[b"b"],
        0xfffff,
    );
    {
        // partially filled short bitmap
        // 0110 1000 0000 0100
        let v: &[&[u8]] = &[
            b"c",

            b"l",
            b"n", &b"o"[..],
        ][..];
        check_behavior(v, &[b"p"]);
    }
    {
        {
            // long alphabitmap in suffix position
            let v: &[&[u8]] = &[
                b"ab1thisrequiresmetowriteastringwithatleastfourtyeightcharacters",
                b"ab2thisrequiresmetowriteastringwithatleastfourtyeightcharacters",
                b"ab3thisrequiresmetowriteastringwithatleastfourtyeightcharacters",
                b"ab4thisrequiresmetowriteastringwithatleastfourtyeightcharacters",
                b"ab5thisrequiresmetowriteastringwithatleastfourtyeightcharacters",
                b"ab6thisrequiresmetowriteastringwithatleastfourtyeightcharacters",
                b"abathisrequiresmetowriteastringwithatleastfourtyeightcharacters",
                b"abbthisrequiresmetowriteastringwithatleastfourtyeightcharacters",
                b"abcthisrequiresmetowriteastringwithatleastfourtyeightcharacters",
                b"abdthisrequiresmetowriteastringwithatleastfourtyeightcharacters",
                b"abethisrequiresmetowriteastringwithatleastfourtyeightcharacters",
                &b"abfthisrequiresmetowriteastringwithatleastfourtyeightcharacters"[..],
            ][..];
            check_behavior(v, &[b"1", b"2", b"3", b"4", b"5", b"6"]);
        }
        // corner case where a node with >= 32 children also has data dictionary compression
        let v: &[&[u8]] = &[
            &b"acde\x01"[..],
            b"bcde\x01",
            b"acde\x01\x02",
            b"bcde\x01\x03",
            b"acde\x01\x04",
            b"bcde\x01\x05",
            b"acde\x01\x06",
            b"bcde\x01\x07",
            b"acde\x01\x08",
            b"bcde\x01\x09",
            b"acde\x01\x0a",
            b"bcde\x01\x0b",
            b"acde\x01\x0c",
            b"bcde\x01\x0d",
            b"acde\x01\x0e",
            b"bcde\x01\x0f",
            b"acde\x01\x10",
            b"bcde\x01\x11",
            b"acde\x01\x12",
            b"bcde\x01\x13",
            b"acde\x01\x14",
            b"bcde\x01\x15",
            b"acde\x01\x16",
            b"bcde\x01\x17",
            b"acde\x01\x18",
            b"bcde\x01\x19",
            b"acde\x01\x1a",
            b"bcde\x01\x1b",
            b"acde\x01\x1c",
            b"bcde\x01\x1d",
            b"acde\x01\x1e",
            b"bcde\x01\x1f",
            b"acde\x01\x20",
            b"bcde\x01\x21",
            b"acde\x01\x22",
            b"bcde\x01\x23",
            b"acde\x01\x24",
            b"bcde\x01\x25",
            b"acde\x01\x26",
            b"bcde\x01\x27",
            b"acde\x01\x28",
            b"bcde\x01\x29",
            b"acde\x01\x2a",
            b"bcde\x01\x2b",
            b"acde\x01\x2c",
            b"bcde\x01\x2d",
            b"acde\x01\x2e",
            b"bcde\x01\x2f",
            b"acde\x01\x30",
            b"bcde\x01\x31",
            b"acde\x01\x32",
            b"bcde\x01\x33",
            b"acde\x01\x34",
            b"bcde\x01\x35",
            b"acde\x01\x36",
            b"bcde\x01\x37",
            b"acde\x01\x38",
            b"bcde\x01\x39",
            b"acde\x01\x3a",
            b"bcde\x01\x3b",
            b"acde\x01\x3c",
            b"bcde\x01\x3d",
            b"acde\x01\x3e",
            b"bcde\x01\x3f",
        ][..];
        check_behavior(v, &[]);
    }
    {
        // full short alphabitmap
        let v: &[&[u8]] = &[
            b"a", b"b", b"c", b"d",
            b"e", b"f", b"g", b"h",
            b"i", b"j", b"k", b"l",
            b"m", b"n", b"o", b"p",
            b"r", b"s", b"t", b"u",
            b"w", b"x", b"y", &b"z"[..],
        ][..];
        check_behavior(v, &[]);
    }
    {
        // full long alphabitmap
        let v: &[&[u8]] = &[
            b"a", b"b", b"c", b"d",
            b"e", b"f", b"g", b"h",
            b"i", b"j", b"k", b"l",
            b"m", b"n", b"o", b"p",
            b"q", b"r", b"s", b"t",
            b"u", b"v", b"w", b"x",
            b"y", b"z", b"1", b"2",
            b"3", b"4", b"5", &b"6"[..],
        ][..];
        check_behavior(v, &[]);
    }
    {
        // these are a corner case for suffix trees
        let mut fibonacci_words = vec![];
        for bytelen in [0, 8, 256, 1024, 65536] {
            let mut word = vec![];
            let mut n = 0;
            for _ in 0..bytelen {
                let mut byte = 0u8;
                const PHI: f64 = 1.618033988749894;
                for offset in 0..8 {
                    byte |= 1 << (offset
                            * (2.0 + f64::floor(n as f64 * PHI) - f64::floor((n + 1) as f64 * PHI))
                                as i32);
                    n += 1;
                }
                word.push(byte);
            }
            fibonacci_words.push(word);
        }
        let fibonacci_words: Vec<&[u8]> = fibonacci_words.iter().map(Vec::as_slice).collect();
        check_behavior(&fibonacci_words[..], &[]);
    }
    {
        // this is a corner case for our generator
        // found by mutation testing
        let mut long_words = vec![];
        for (i, bytelen) in [0xff_00].iter().copied().enumerate() {
            let mut word = vec![i as u8];
            word.resize(1 + bytelen, 0xff);
            long_words.push(word);
        }
        let mut long_words_check = vec![];
        for (i, bytelen) in [0xff_01].iter().copied().enumerate() {
            let mut word = vec![i as u8];
            word.resize(1+bytelen,0xff);
            long_words_check.push(word);
            let word = vec![0xff;bytelen];
            long_words_check.push(word);
        }
        let long_words: Vec<&[u8]> = long_words.iter().map(Vec::as_slice).collect();
        let long_words_check: Vec<&[u8]> = long_words_check.iter().map(Vec::as_slice).collect();
        check_behavior(&long_words[..], &long_words_check[..]);
    }
    // generated by manual construction and by property-based testing
    check_behavior(&[&[0], &[0, 0, 162]], &[&[162]]);
    check_behavior(&[b"x"], &[b"", b"x", b"xx", b"xxx", b"b", b"bb", b"bbb"]);
    check_behavior(&[b"x", b"y"], &[b""]);
    check_behavior(&[b"xx"], &[b"", b"x", b"xx", b"xxx", b"b", b"bb", b"bbb"]);
    check_behavior(&[b"xa"], &[b"", b"x", b"xx", b"a", b"aa"]);
    check_behavior(&[b"7x7a"], &[b"", b"7", b"x", b"a", b"v"]);
    check_behavior(
        &[b"abbaa"],
        &[
            b"", b"a", b"aa", b"aaa", b"b", b"bb", b"bbb", b"c", b"cc", b"ccc",
        ],
    );
    check_behavior(
        &[b"abac"],
        &[
            b"", b"a", b"aa", b"aaa", b"b", b"bb", b"bbb", b"c", b"cc", b"ccc",
        ],
    );
    check_behavior(
        &["x\u{1F600}x\u{1F601}".as_bytes()],
        &[
            b"",
            b"x",
            b"xx",
            b"xxx",
            "\u{1F601}".as_bytes(),
            "\u{1F601}\u{1F601}".as_bytes(),
            "\u{1F601}\u{1F601}\u{1F601}".as_bytes(),
        ],
    );
    check_behavior(&[b"aa", b"ba"], &[]);
    check_behavior(&[b"x", b"aa"], &[]);
    check_behavior(&[b"\\"], &[b"a", b"\"", b"\\"]);
    check_behavior(&[b"\""], &[b"a", b"\"", b"\\"]);
    check_behavior(&[b" ", b" "], &[]);
    check_behavior(
        &["\u{13}\u{3000}".as_bytes(), "\u{13}\u{3000}\0".as_bytes()],
        &[],
    );
    check_behavior(&[b"___ "], &[]);
    check_behavior(
        &[&[
            207, 249, 0, 153, 77, 127, 21, 233, 64, 54, 95, 73, 181, 19, 34, 113, 85, 130, 95, 13,
            1, 205, 145, 231, 4, 97, 143, 172, 125, 47, 99, 160, 246, 225, 1, 1, 74, 157, 43, 156,
            25, 87, 195, 92, 40, 179, 68,
        ]],
        &[],
    );
    check_behavior(&[&[0, 45], &[0, 45]], &[]);
    check_behavior(&[b"ac", b"dc"], &[b"a", b"d", b"c", b"b"]);
    check_behavior(&[b"a", b"ac", b"dc"], &[b"a", b"d", b"c", b"b"]);
    check_behavior(
        &[&[0], &[0, 1, 203], &[0, 1, 203]],
        &[],
    );
    check_behavior(
        &[
            &[51, 45, 165, 53, 6, 228, 4, 217, 107, 65, 197, 41, 38, 124, 114, 217, 177, 91, 53, 3, 168, 108, 122, 230, 231][..],
            &[0, 215, 217, 199, 212, 1, 127, 138, 29, 117][..],
            &[1, 210, 117][..],
            &[88, 33, 120, 67, 225, 206, 218, 72, 183, 180, 139, 13, 130, 68, 243, 96,  152][..],
            &[253, 234, 31, 250, 226, 18, 252, 135, 121, 173, 174, 134, 128, 193, 19, 224, 48, 233, 30, 104, 232][..],
            &[205, 116, 49, 111, 248, 109, 235, 42, 54, 201, 198, 81, 73, 219, 2, 23, 223, 50, 188, 8, 28, 56, 146, 85, 247, 27, 249, 255, 76,  86][..],
            &[244, 74, 24, 156, 158, 7, 43, 246, 44, 35, 102, 166, 90, 63, 113, 125, 70, 147, 132, 161, 196, 82][..],
            &[11, 200, 194, 71, 178, 171, 40, 203, 169, 179, 170, 220, 14, 22, 149, 242, 15, 77, 207, 214, 131, 126, 75, 251][..],
            &[93, 59, 47, 143, 153, 150][..],
            &[110, 175, 227, 176, 254, 209, 167, 239][..],
            &[52, 185, 133, 189, 97, 98, 94, 137][..],
            &[145, 237, 191, 163, 241, 216, 144, 60, 213, 118, 21, 172, 100][..],
            &[142, 187, 238, 78, 16, 129, 12, 17, 160, 208, 32, 195, 62, 204, 159, 87, 101, 112, 64, 155, 184, 164, 181][..],
            &[103, 140, 229, 99, 202, 190, 136, 79, 66, 61, 211, 25, 89, 192, 157, 105, 240, 58, 55, 39, 84, 10, 236, 20][..],
            &[36, 95, 154, 115, 83, 5, 141, 123, 37, 119, 151, 92, 106, 162][..],
            &[138, 222, 221, 182, 57, 34, 9, 46, 186, 80, 26, 245, 148, 69][..],
        ],
        &[],
    );
    check_behavior(&[
        b"localwakerfromnonlocal",
        b"localwakerfromnonlocal",
    ], &[b"wakerfrom", b"my"]);
}

#[cfg(feature = "quickcheck")]
quickcheck! {
    fn test_rand_search_tree(strings_to_insert: Vec<Vec<u8>>, strings_to_check: Vec<Vec<u8>>) -> quickcheck::TestResult {
        if strings_to_insert.iter().any(|name| name.is_empty()) {
            return quickcheck::TestResult::discard();
        }
        if strings_to_insert.len() < 2 {
            return quickcheck::TestResult::discard();
        }
        check_behavior(
            &strings_to_insert.iter().map(|x| &x[..]).collect::<Vec<_>>(),
            &strings_to_check.iter().map(|x| &x[..]).collect::<Vec<_>>(),
        );
        quickcheck::TestResult::passed()
    }
}

fn check_behavior_ukkonen_encode_vs_naive_encode(string_to_insert: &[u8], string_to_check: &[u8]) {
    let naively_constructed_tree = encode_search_tree_naive([string_to_insert].iter().copied());
    let chv = string_to_insert.to_vec();
    let chv_chk = string_to_check.to_vec();
    let ukkonen_constructed_tree =
        crate::internals::tree::encode_search_tree_ukkonen([string_to_insert].iter().copied());
    for st in [chv, chv_chk] {
        for i in (1..st.len().min(4)).chain(std::iter::once(st.len())) {
            for wnd in st.windows(i) {
                let wnd: Vec<u8> = wnd.to_vec();
                if get_from_search_tree(&wnd, false, &naively_constructed_tree, 0)
                    != get_from_search_tree(&wnd, false, &ukkonen_constructed_tree, 0)
                {
                    println!("{:#?}", naively_constructed_tree);
                    println!("{:#?}", ukkonen_constructed_tree);
                }
                assert_eq!(
                    get_from_search_tree(&wnd, false, &naively_constructed_tree, 0),
                    get_from_search_tree(&wnd, false, &ukkonen_constructed_tree, 0),
                    "search window = {wnd:?}"
                );
                if get_from_search_tree(&wnd, true, &naively_constructed_tree, 0)
                    != get_from_search_tree(&wnd, true, &ukkonen_constructed_tree, 0)
                {
                    println!("{:#?}", naively_constructed_tree);
                    println!("{:#?}", ukkonen_constructed_tree);
                }
                assert_eq!(
                    get_from_search_tree(&wnd, true, &naively_constructed_tree, 0),
                    get_from_search_tree(&wnd, true, &ukkonen_constructed_tree, 0),
                    "search window substring = {wnd:?}"
                );
            }
        }
    }
}

#[test]
fn test_ukkonen_encode_vs_naive_encode() {
    check_behavior_ukkonen_encode_vs_naive_encode(b"xxx", b"x");
    check_behavior_ukkonen_encode_vs_naive_encode(b"a1a1", b"x");
    check_behavior_ukkonen_encode_vs_naive_encode(b"a1a", b"x");
    check_behavior_ukkonen_encode_vs_naive_encode(b"nn", b"x");
    check_behavior_ukkonen_encode_vs_naive_encode(b"\0xx", b"\0");
}

#[cfg(feature = "quickcheck")]
quickcheck! {
    fn test_rand_ukkonen_encode_vs_naive_encode(string_to_insert: Vec<u8>, string_to_check: Vec<u8>) -> quickcheck::TestResult {
        if string_to_insert.is_empty() || string_to_check.is_empty() {
            return quickcheck::TestResult::discard();
        }
        check_behavior_ukkonen_encode_vs_naive_encode(&string_to_insert, &string_to_check);
        quickcheck::TestResult::passed()
    }
}

#[test]
fn test_scan_column_in_root() {
    use crate::internals::scan_column_in_root;
    assert_eq!(scan_column_in_root(br#""test":{"stuff"}"#, b"test"), None,);
    assert_eq!(
        scan_column_in_root(br#"rr_('{"test":{"stuff"}"#, b"test"),
        None,
    );
    assert_eq!(
        scan_column_in_root(br#"rr_('{"test":{"stuff"}')"#, b"test"),
        None,
    );
    assert_eq!(
        scan_column_in_root(br#"rr_('{"test":{"stuff"}}')"#, b"test"),
        Some(&br#""stuff""#[..]),
    );
    assert_eq!(
        scan_column_in_root(br#"rr_('{"tes\t":{"stuff"}}')"#, br"tes\t"),
        None,
    );
    assert_eq!(scan_column_in_root(br#"rr_('{"":{"stuff"}}')"#, br""), None,);
    assert_eq!(
        scan_column_in_root(
            br#"rr_('{"testing":{"whatever"},"test":{"stuff"}}')"#,
            b"test"
        ),
        Some(&br#""stuff""#[..]),
    );
    assert_eq!(
        scan_column_in_root(
            br#"rr_('{"testing":{"whatever"}"test":{"stuff"}}')"#,
            b"test"
        ),
        None,
    );
}
