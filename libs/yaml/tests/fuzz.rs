//! Deterministic byte-mutation fuzzing for the YAML parser and emitter.

#[path = "../../compress/tests/support/fuzz.rs"]
mod fuzz;

use feos_yaml::{NodeId, YamlDocument, YamlNode};
use fuzz::{fuzz_bytes, run_constrained, LimitAllocator};

#[global_allocator]
static ALLOCATOR: LimitAllocator = LimitAllocator;

const ITERATIONS: usize = 3000;
const MAX_INPUT: usize = 8 * 1024;

const SEEDS: [&str; 6] = [
    "name: feos\nversion: 1\nenabled: true\nnested:\n  key: value\n  list: [1, 2, {a: b}]\ntags:\n- x\n- y\n",
    "literal: |\n  one\n   two\n\n  three\nfolded: >-\n  a\n  b\n    more\n  c\nkeep: |+2\n    x\n\nnext: 1\n",
    "- - a\n  - b\n- k: v\n  j: 'it''s: x'\n- \"esc\\t\\u20ac\\x41\"\n-   deep: 1\n    deeper:\n      - |\n        text\n",
    "# comment\n---\na: don't panic # note\nb: \"#no\" # yes\nc: {x: [1, {y: z}], 'q': \"r\"}\n...\n",
    "--- |\nroot text\n\u{e9}\u{e9}\n...\n",
    "init:\n  restart: \"always\"\n  max_restarts: 5\n  autostart:\n    - terminal\n    - files\nwindow: {width: 640, height: 420}\n",
];

fn walk(doc: &YamlDocument<'_, '_>, id: NodeId, budget: &mut usize) {
    if *budget == 0 {
        return;
    }
    *budget -= 1;
    let mut text = [0u8; 256];
    let _ = doc.as_str(id, &mut text);
    let _ = doc.as_bool(id);
    let _ = doc.as_i64(id);
    let _ = doc.as_f64(id);
    let _ = doc.is_null(id);
    let _ = doc.get(id, "name");
    if let Some(items) = doc.sequence_iter(id) {
        for child in items {
            walk(doc, child, budget);
        }
    }
    if let Some(entries) = doc.mapping_iter(id) {
        for (_, child) in entries {
            walk(doc, child, budget);
        }
    }
}

fn check(input: &[u8]) {
    let Ok(input) = core::str::from_utf8(input) else {
        return;
    };
    let mut arena = vec![YamlNode::EMPTY; 256];
    let Ok(doc) = YamlDocument::parse(input, &mut arena) else {
        return;
    };
    let mut budget = 512;
    walk(&doc, doc.root(), &mut budget);

    // What the emitter writes must parse, and emitting that must be stable.
    let mut first = String::new();
    if doc.encode_to(doc.root(), &mut first).is_err() {
        return;
    }
    let mut again = vec![YamlNode::EMPTY; 512];
    let reparsed = YamlDocument::parse(&first, &mut again)
        .unwrap_or_else(|error| panic!("{input:?} emitted {first:?}: {error}"));
    let mut second = String::new();
    reparsed.encode_to(reparsed.root(), &mut second).unwrap();
    assert_eq!(first, second, "input {input:?}");
}

#[test]
fn parser_and_emitter_survive_mutated_documents() {
    run_constrained(|| {
        let seeds: Vec<Vec<u8>> = SEEDS.iter().map(|seed| seed.as_bytes().to_vec()).collect();
        fuzz_bytes(21, &seeds, ITERATIONS, MAX_INPUT, check);
    });
}

#[test]
fn deep_nesting_is_refused_within_the_stack() {
    run_constrained(|| {
        for unit in ["- ", "[", "{a: ", "a:\n", "& ", "! ", "* "] {
            let input = unit.repeat(4096);
            let mut arena = vec![YamlNode::EMPTY; 8192];
            let _ = YamlDocument::parse(&input, &mut arena);
        }
        let mut indented = String::new();
        for level in 0..4096 {
            indented.push_str(&" ".repeat(level));
            indented.push_str("a:\n");
        }
        let mut arena = vec![YamlNode::EMPTY; 8192];
        let _ = YamlDocument::parse(&indented, &mut arena);
    });
}
