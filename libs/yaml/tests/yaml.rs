use feos_yaml::{Chomp, ScalarStyle, YamlDocument, YamlError, YamlNode};

fn parse<'a, 's>(input: &'a str, arena: &'s mut [YamlNode<'a>]) -> YamlDocument<'a, 's> {
    YamlDocument::parse(input, arena).unwrap()
}

fn text(doc: &YamlDocument<'_, '_>, id: u32) -> String {
    let mut out = [0u8; 512];
    doc.as_str(id, &mut out).unwrap().to_string()
}

#[test]
fn parses_block_mapping_and_sequence() {
    let mut arena = [YamlNode::EMPTY; 64];
    let doc = parse(
        r#"
# leading comment
name: feos
version: 1
enabled: true
tags:
  - json
  - yaml
  - image
nested:
  key: value
  list: [1, 2, 3]
"#,
        &mut arena,
    );
    let root = doc.root();
    assert_eq!(text(&doc, doc.get(root, "name").unwrap()), "feos");
    assert_eq!(doc.as_i64(doc.get(root, "version").unwrap()), Some(1));
    assert_eq!(doc.as_bool(doc.get(root, "enabled").unwrap()), Some(true));

    let tags = doc.get(root, "tags").unwrap();
    assert!(doc.is_sequence(tags));
    assert_eq!(doc.len(tags), Some(3));
    assert_eq!(text(&doc, doc.at(tags, 1).unwrap()), "yaml");

    let nested = doc.get(root, "nested").unwrap();
    assert_eq!(text(&doc, doc.get(nested, "key").unwrap()), "value");
    let list = doc.get(nested, "list").unwrap();
    assert_eq!(doc.len(list), Some(3));
    assert_eq!(doc.as_i64(doc.at(list, 2).unwrap()), Some(3));
}

#[test]
fn parses_compact_mapping_items() {
    let mut arena = [YamlNode::EMPTY; 64];
    let doc = parse(
        "servers:\n  - name: alpha\n    port: 80\n  - name: beta\n    port: 443\n",
        &mut arena,
    );
    let servers = doc.get(doc.root(), "servers").unwrap();
    assert_eq!(doc.len(servers), Some(2));
    let alpha = doc.at(servers, 0).unwrap();
    assert_eq!(text(&doc, doc.get(alpha, "name").unwrap()), "alpha");
    assert_eq!(doc.as_i64(doc.get(alpha, "port").unwrap()), Some(80));
}

#[test]
fn parses_flow_collections_and_quotes() {
    let mut arena = [YamlNode::EMPTY; 128];
    let doc = parse(
        r#"root: {"a": [1, 2, {b: 'it''s', c: "line\nnext"}], empty: {}, list: []}"#,
        &mut arena,
    );
    let root = doc.get(doc.root(), "root").unwrap();
    let a = doc.get(root, "a").unwrap();
    assert_eq!(doc.len(a), Some(3));
    let object = doc.at(a, 2).unwrap();
    assert_eq!(text(&doc, doc.get(object, "b").unwrap()), "it's");
    assert_eq!(text(&doc, doc.get(object, "c").unwrap()), "line\nnext");
    assert!(doc.is_mapping(doc.get(root, "empty").unwrap()));
    assert!(doc.is_sequence(doc.get(root, "list").unwrap()));
}

#[test]
fn parses_literal_and_folded_scalars() {
    let mut arena = [YamlNode::EMPTY; 32];
    let doc = parse(
        "literal: |\n  one\n  two\nfolded: >-\n  hello\n  world\n",
        &mut arena,
    );
    let literal = doc.get(doc.root(), "literal").unwrap();
    assert_eq!(doc.scalar(literal).unwrap().style(), ScalarStyle::Literal);
    assert_eq!(text(&doc, literal), "one\ntwo\n");

    let folded = doc.get(doc.root(), "folded").unwrap();
    assert_eq!(doc.scalar(folded).unwrap().style(), ScalarStyle::Folded);
    assert_eq!(text(&doc, folded), "hello world");
}

#[test]
fn handles_nulls_and_documents() {
    let mut arena = [YamlNode::EMPTY; 16];
    let doc = parse("---\na: null\nb: ~\nc:\n", &mut arena);
    let root = doc.root();
    assert!(doc.is_null(doc.get(root, "a").unwrap()));
    assert!(doc.is_null(doc.get(root, "b").unwrap()));
    assert!(doc.is_null(doc.get(root, "c").unwrap()));
}

#[test]
fn emits_valid_block_yaml() {
    let mut arena = [YamlNode::EMPTY; 32];
    let doc = parse("name: feos\nitems:\n  - one\n  - two\n", &mut arena);
    let mut out = String::new();
    doc.encode_to(doc.root(), &mut out).unwrap();
    assert!(out.contains("name: feos"));
    assert!(out.contains("- two"));

    let mut arena2 = [YamlNode::EMPTY; 32];
    let reparsed = parse(&out, &mut arena2);
    assert_eq!(
        text(&reparsed, reparsed.get(reparsed.root(), "name").unwrap()),
        "feos"
    );
}

#[test]
fn rejects_invalid_documents() {
    let cases = [
        ("a:\n\tb: 1\n", YamlError::TabIndentation { offset: 3 }),
        ("[1, 2", YamlError::UnterminatedFlow { offset: 5 }),
        (
            "a: 'unterminated",
            YamlError::UnterminatedQuote { offset: 3 },
        ),
        ("---\na: 1\n---\nb: 2\n", YamlError::MultipleDocuments),
    ];
    for (case, expected) in cases {
        let mut arena = [YamlNode::EMPTY; 32];
        assert_eq!(
            YamlDocument::parse(case, &mut arena).err(),
            Some(expected),
            "case {case:?}"
        );
    }
}

#[test]
fn scalar_borrowing_and_styles() {
    let scalar = feos_yaml::YamlScalar::plain("plain");
    assert_eq!(scalar.as_borrowed(), Some("plain"));
    assert_eq!(scalar.style(), ScalarStyle::Plain);
    assert_eq!(
        feos_yaml::YamlScalar::block("  x\n", ScalarStyle::Literal, 2, Chomp::Strip).style(),
        ScalarStyle::Literal
    );
}

/// Value of the root mapping's `a` key.
fn value_of_a(input: &str) -> String {
    let mut arena = [YamlNode::EMPTY; 16];
    let doc = YamlDocument::parse(input, &mut arena).unwrap();
    text(&doc, doc.get(doc.root(), "a").unwrap())
}

fn parse_error(input: &str) -> Option<YamlError> {
    let mut arena = [YamlNode::EMPTY; 32];
    YamlDocument::parse(input, &mut arena).err()
}

#[test]
fn block_scalar_dedent_before_multibyte_text_does_not_panic() {
    // The audit's input. YAML ends the scalar at the shallower line, which then
    // doesn't fit the enclosing mapping (PyYAML rejects it the same way).
    assert_eq!(
        parse_error("a: |\n     x\n    \u{e9}\n"),
        Some(YamlError::InvalidIndentation { offset: 12 })
    );
}

#[test]
fn block_scalar_dedent_never_corrupts_the_value() {
    // The audit's input used to decode to `four\nz\n`.
    assert_eq!(
        parse_error("a: |\n    four\n  xyz\n"),
        Some(YamlError::InvalidIndentation { offset: 14 })
    );
}

#[test]
fn block_scalar_ends_at_a_less_indented_line() {
    let input = "outer:\n  a: |\n      four\n  b: 1\n";
    let mut arena = [YamlNode::EMPTY; 16];
    let doc = YamlDocument::parse(input, &mut arena).unwrap();
    let outer = doc.get(doc.root(), "outer").unwrap();
    assert_eq!(text(&doc, doc.get(outer, "a").unwrap()), "four\n");
    assert_eq!(doc.as_i64(doc.get(outer, "b").unwrap()), Some(1));
    assert_eq!(
        value_of_a("a: |\n    \u{e9}\u{e9}\nb: 1\n"),
        "\u{e9}\u{e9}\n"
    );
}

#[test]
fn apostrophe_in_plain_scalar_does_not_hide_a_comment() {
    assert_eq!(value_of_a("a: don't panic # note\n"), "don't panic");
    assert_eq!(value_of_a("a: rock 'n roll # note\n"), "rock 'n roll");
    assert_eq!(value_of_a("a: 'x # kept' # note\n"), "x # kept");
    // `#` with no space before it is ordinary text.
    assert_eq!(value_of_a("a: issue#42\n"), "issue#42");
}

#[test]
fn sequence_may_sit_at_its_keys_indent() {
    let mut arena = [YamlNode::EMPTY; 16];
    let doc = YamlDocument::parse("tags:\n- a\n- b\nnext: 1\n", &mut arena).unwrap();
    let tags = doc.get(doc.root(), "tags").unwrap();
    assert_eq!(doc.len(tags), Some(2));
    assert_eq!(text(&doc, doc.at(tags, 1).unwrap()), "b");
    assert_eq!(doc.as_i64(doc.get(doc.root(), "next").unwrap()), Some(1));
}

#[test]
fn nested_sequence_on_one_line() {
    let mut arena = [YamlNode::EMPTY; 16];
    let doc = YamlDocument::parse("- - a\n  - b\n- - - c\n", &mut arena).unwrap();
    let root = doc.root();
    assert_eq!(doc.len(root), Some(2));
    let first = doc.at(root, 0).unwrap();
    assert!(doc.is_sequence(first));
    assert_eq!(text(&doc, doc.at(first, 1).unwrap()), "b");
    let inner = doc.at(doc.at(root, 1).unwrap(), 0).unwrap();
    assert_eq!(text(&doc, doc.at(inner, 0).unwrap()), "c");
}

#[test]
fn compact_mapping_uses_the_real_column() {
    let mut arena = [YamlNode::EMPTY; 16];
    let doc = YamlDocument::parse("-   a: 1\n    b: 2\n", &mut arena).unwrap();
    let item = doc.at(doc.root(), 0).unwrap();
    assert_eq!(doc.as_i64(doc.get(item, "b").unwrap()), Some(2));
}

#[test]
fn doubled_quote_in_sequence_item_is_not_a_mapping() {
    let mut arena = [YamlNode::EMPTY; 8];
    let doc = YamlDocument::parse("- 'it''s: x'\n", &mut arena).unwrap();
    assert_eq!(text(&doc, doc.at(doc.root(), 0).unwrap()), "it's: x");
}

#[test]
fn tab_is_content_inside_a_literal_block() {
    assert_eq!(value_of_a("a: |\n  x\n  \ty\n"), "x\n\ty\n");
    assert!(matches!(
        parse_error("a: 1\n\tb: 2\n"),
        Some(YamlError::TabIndentation { .. })
    ));
}

#[test]
fn block_indentation_indicator_is_honoured() {
    assert_eq!(value_of_a("a: |2\n    x\n  y\n"), "  x\ny\n");
    assert_eq!(value_of_a("a: >1-\n  x\n y\n"), " x\ny");
    assert_eq!(value_of_a("a: |-1\n x\n"), "x");
    assert!(matches!(
        parse_error("a: |0\n  x\n"),
        Some(YamlError::UnexpectedToken { byte: b'0', .. })
    ));
    assert!(matches!(
        parse_error("a: |--\n  x\n"),
        Some(YamlError::UnexpectedToken { byte: b'-', .. })
    ));
}

#[test]
fn more_indented_folded_lines_keep_their_breaks() {
    assert_eq!(
        value_of_a("a: >\n  one\n  two\n    code\n  three\n"),
        "one two\n  code\nthree\n"
    );
}

#[test]
fn root_block_scalar_may_start_in_column_zero() {
    let mut arena = [YamlNode::EMPTY; 4];
    let doc = YamlDocument::parse("--- |\nline\n...\n", &mut arena).unwrap();
    assert_eq!(text(&doc, doc.root()), "line\n");
    let mut arena = [YamlNode::EMPTY; 4];
    let doc = YamlDocument::parse("--- plain # note\n", &mut arena).unwrap();
    assert_eq!(text(&doc, doc.root()), "plain");
}

#[test]
fn anchors_tags_and_aliases_are_rejected() {
    for (input, expected) in [
        ("a: &x 1\n", YamlError::UnsupportedAlias { offset: 3 }),
        ("a: *x\n", YamlError::UnsupportedAlias { offset: 3 }),
        ("a: !!str 1\n", YamlError::UnsupportedTag { offset: 3 }),
        ("&k a: 1\n", YamlError::UnsupportedAlias { offset: 0 }),
        ("a: [1, &x 2]\n", YamlError::UnsupportedAlias { offset: 7 }),
        ("a: [!t 1]\n", YamlError::UnsupportedTag { offset: 4 }),
        ("a: {*k: 1}\n", YamlError::UnsupportedAlias { offset: 4 }),
    ] {
        assert_eq!(parse_error(input), Some(expected), "input {input:?}");
    }
}

#[test]
fn repeated_anchor_tokens_do_not_recurse() {
    // Each token used to add a stack frame, with no depth count.
    let mut input = String::from("a: ");
    input.push_str(&"& ".repeat(100_000));
    input.push('x');
    assert_eq!(
        parse_error(&input),
        Some(YamlError::UnsupportedAlias { offset: 3 })
    );
}

#[test]
fn invalid_escapes_are_rejected() {
    assert_eq!(
        parse_error("name: \"\\q\"\n"),
        Some(YamlError::InvalidEscape { offset: 7 })
    );
    assert_eq!(
        parse_error("name: \"ok\\xZZ\"\n"),
        Some(YamlError::InvalidEscape { offset: 9 })
    );
    assert_eq!(
        parse_error("- \"\\ud800\"\n"),
        Some(YamlError::InvalidEscape { offset: 3 })
    );
    assert_eq!(value_of_a("a: \"\\x41\\u20ac\"\n"), "A\u{20ac}");
}

#[test]
fn stray_content_is_not_reported_as_a_second_document() {
    assert_eq!(
        parse_error("- a\nb: 1\n"),
        Some(YamlError::InvalidIndentation { offset: 4 })
    );
    assert_eq!(
        parse_error("a: 1\n- b\n"),
        Some(YamlError::InvalidIndentation { offset: 5 })
    );
    assert_eq!(
        parse_error("a: 1\n...\nb: 2\n"),
        Some(YamlError::MultipleDocuments)
    );
}

#[test]
fn complex_keys_are_rejected() {
    assert!(matches!(
        parse_error("? a\n: b\n"),
        Some(YamlError::UnexpectedToken { byte: b'?', .. })
    ));
}

fn round_trip(input: &str) -> String {
    let mut arena = [YamlNode::EMPTY; 32];
    let doc = YamlDocument::parse(input, &mut arena).unwrap();
    let mut out = String::new();
    doc.encode_to(doc.root(), &mut out).unwrap();
    out
}

#[test]
fn emitter_preserves_scalar_types() {
    assert_eq!(round_trip("a: 'true'\n"), "a: 'true'");
    assert_eq!(round_trip("a: \"12\"\n"), "a: '12'");
    assert_eq!(
        round_trip("a: true\nb: 12\nc: -5\nd: ~\n"),
        "a: true\nb: 12\nc: -5\nd: ~"
    );
    assert_eq!(round_trip("a:\n"), "a: null");
    assert_eq!(round_trip("a: ''\n"), "a: ''");
}

#[test]
fn emitter_quotes_what_would_not_read_back() {
    // A newline can't sit inside this parser's single quotes.
    assert_eq!(round_trip("a: |\n  one\n  two\n"), "a: \"one\\ntwo\\n\"");
    assert_eq!(round_trip("a: \"it's\\ttab\"\n"), "a: \"it's\\ttab\"");
    assert_eq!(round_trip("a: \"it's: x\"\n"), "a: 'it''s: x'");
    assert_eq!(round_trip("a: \"\\x01\"\n"), "a: \"\\u0001\"");
    assert_eq!(round_trip("a: \"back\\\\slash\"\n"), "a: back\\slash");
}

#[test]
fn emitter_refuses_a_scalar_it_cannot_stage() {
    let long = format!("a: \"{}\"\n", "x".repeat(2000));
    let mut arena = [YamlNode::EMPTY; 4];
    let doc = YamlDocument::parse(&long, &mut arena).unwrap();
    let mut out = String::new();
    assert!(doc.encode_to(doc.root(), &mut out).is_err());
    assert!(!out.contains("too long"));
    // A long plain scalar needs no staging.
    let long = format!("a: {}\n", "x".repeat(2000));
    assert_eq!(round_trip(&long), long.trim_end());
}

#[test]
fn emitter_output_reparses_with_the_same_shape() {
    let input = concat!(
        "name: \"it's\"\n",
        "empty_list: []\n",
        "empty_map: {}\n",
        "items:\n",
        "  - one\n",
        "  - []\n",
        "  - {}\n",
        "  - - nested\n",
        "    - seq\n",
        "  - key: value\n",
        "    other: 'true'\n",
        "map:\n",
        "  inner:\n",
        "    deep: 1\n",
    );
    let first = round_trip(input);
    assert_eq!(round_trip(&first), first);

    let mut arena = [YamlNode::EMPTY; 32];
    let doc = YamlDocument::parse(&first, &mut arena).unwrap();
    let items = doc.get(doc.root(), "items").unwrap();
    assert_eq!(doc.len(items), Some(5));
    assert_eq!(doc.is_empty(doc.at(items, 1).unwrap()), Some(true));
    assert!(doc.is_mapping(doc.at(items, 2).unwrap()));
    assert_eq!(doc.len(doc.at(items, 3).unwrap()), Some(2));
    let mapping = doc.at(items, 4).unwrap();
    assert_eq!(text(&doc, doc.get(mapping, "other").unwrap()), "true");
    assert_eq!(
        doc.scalar(doc.get(mapping, "other").unwrap())
            .unwrap()
            .style(),
        ScalarStyle::SingleQuoted
    );
    let inner = doc
        .get(doc.get(doc.root(), "map").unwrap(), "inner")
        .unwrap();
    assert_eq!(doc.as_i64(doc.get(inner, "deep").unwrap()), Some(1));
}
