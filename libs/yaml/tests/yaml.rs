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
