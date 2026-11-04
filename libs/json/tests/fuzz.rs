//! Deterministic byte-mutation fuzzing for the JSON parser and writer.

#[path = "../../compress/tests/support/fuzz.rs"]
mod fuzz;

use feos_json::{write_json, JsonDocument, JsonNode};
use fuzz::{fuzz_bytes, run_constrained, LimitAllocator};

#[global_allocator]
static ALLOCATOR: LimitAllocator = LimitAllocator;

const ITERATIONS: usize = 6000;
const MAX_INPUT: usize = 8 * 1024;

const SEEDS: [&str; 3] = [
    r#"{"name":"feos","list":[1,2.5,-3e4,true,false,null],"nested":{"a":{"b":[{}, []]}}}"#,
    r#"["esc\"\\\/\b\f\n\r\t\u0041\ud83d\ude00", "plain", 0, 1e308, -0.0]"#,
    "  [ { \"k\" : \"v\" } , [ ] ]  ",
];

fn check(input: &[u8]) {
    let Ok(input) = core::str::from_utf8(input) else {
        return;
    };
    let mut arena = vec![JsonNode::EMPTY; 256];
    let Ok(doc) = JsonDocument::parse(input, &mut arena) else {
        return;
    };
    let mut out = String::new();
    write_json(&doc, doc.root(), &mut out).unwrap();

    // What the writer produces must parse back to the same text.
    let mut again = vec![JsonNode::EMPTY; 256];
    let reparsed = JsonDocument::parse(&out, &mut again).unwrap();
    let mut second = String::new();
    write_json(&reparsed, reparsed.root(), &mut second).unwrap();
    assert_eq!(out, second);
}

#[test]
fn parser_and_writer_survive_mutated_documents() {
    run_constrained(|| {
        let seeds: Vec<Vec<u8>> = SEEDS.iter().map(|seed| seed.as_bytes().to_vec()).collect();
        fuzz_bytes(31, &seeds, ITERATIONS, MAX_INPUT, check);
    });
}

#[test]
fn deep_nesting_is_refused_within_the_stack() {
    run_constrained(|| {
        for unit in ["[", "{\"a\":"] {
            let input = unit.repeat(100_000);
            let mut arena = vec![JsonNode::EMPTY; 1024];
            assert!(JsonDocument::parse(&input, &mut arena).is_err());
        }
    });
}
