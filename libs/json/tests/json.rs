use feos_json::{write_json, write_json_pretty, JsonDocument, JsonError, JsonNode, Kind};

fn doc<'a, 's>(input: &'a str, arena: &'s mut [JsonNode<'a>]) -> JsonDocument<'a, 's> {
    JsonDocument::parse(input, arena).unwrap()
}

#[test]
fn parses_all_value_kinds() {
    let mut arena = [JsonNode::EMPTY; 64];
    let text = r#"{
        "null": null,
        "true": true,
        "false": false,
        "number": -12.5e+3,
        "string": "hello",
        "array": [1, "two", null, {"x": 3}],
        "object": {"nested": []}
    }"#;
    let doc = doc(text, &mut arena);
    let root = doc.root();

    assert!(doc.is_null(doc.get(root, "null").unwrap()));
    assert_eq!(doc.as_bool(doc.get(root, "true").unwrap()), Some(true));
    assert_eq!(doc.as_bool(doc.get(root, "false").unwrap()), Some(false));
    let number = doc.get(root, "number").unwrap();
    assert_eq!(doc.number(number), Some("-12.5e+3"));
    assert_eq!(doc.as_f64(number), Some(-12500.0));
    assert_eq!(
        doc.as_str_raw(doc.get(root, "string").unwrap()),
        Some("hello")
    );

    let array = doc.get(root, "array").unwrap();
    assert_eq!(doc.len(array), Some(4));
    assert_eq!(doc.as_i64(doc.at(array, 0).unwrap()), Some(1));
    assert_eq!(doc.as_str_raw(doc.at(array, 1).unwrap()), Some("two"));
    assert!(doc.is_null(doc.at(array, 2).unwrap()));

    let object = doc.get(root, "object").unwrap();
    let nested = doc.get(object, "nested").unwrap();
    assert!(doc.is_array(nested));
    assert_eq!(doc.len(nested), Some(0));
}

#[test]
fn decodes_escaped_strings_and_keys() {
    let mut arena = [JsonNode::EMPTY; 32];
    let doc = doc(
        r#"{"a\u0062":"line\nquote\"slash\\","emoji":"\ud83d\ude00"}"#,
        &mut arena,
    );
    let root = doc.root();

    let value = doc.get(root, "ab").unwrap();
    let mut out = [0u8; 64];
    assert_eq!(doc.as_str(value, &mut out), Some("line\nquote\"slash\\"));
    assert_eq!(
        doc.as_str(doc.get(root, "emoji").unwrap(), &mut out),
        Some("😀")
    );
    assert_eq!(
        doc.get(root, "ab").and_then(|id| doc.string(id)),
        Some(feos_json::JsonString::new(r#"line\nquote\"slash\\"#, true))
    );
}

#[test]
fn iterators_cover_children_in_order() {
    let mut arena = [JsonNode::EMPTY; 32];
    let doc = doc(r#"{"a":1,"b":[2,3],"c":true}"#, &mut arena);
    let root = doc.root();

    let keys: Vec<&str> = doc
        .object_iter(root)
        .unwrap()
        .map(|(key, _)| key.as_borrowed().unwrap())
        .collect();
    assert_eq!(keys, ["a", "b", "c"]);

    let values: Vec<i64> = doc
        .array_iter(doc.get(root, "b").unwrap())
        .unwrap()
        .map(|id| doc.as_i64(id).unwrap())
        .collect();
    assert_eq!(values, [2, 3]);
}

#[test]
fn round_trips_through_writer() {
    let mut arena = [JsonNode::EMPTY; 64];
    let text = r#"{"a":[1,2,{"b":"x"}],"c":null}"#;
    let doc = doc(text, &mut arena);
    let mut compact = String::new();
    write_json(&doc, doc.root(), &mut compact).unwrap();
    assert_eq!(compact, text);

    let mut pretty = String::new();
    write_json_pretty(&doc, doc.root(), &mut pretty).unwrap();
    assert!(pretty.contains("\n  \"a\": ["));

    let mut arena2 = [JsonNode::EMPTY; 64];
    let reparsed = JsonDocument::parse(&compact, &mut arena2).unwrap();
    let mut again = String::new();
    write_json(&reparsed, reparsed.root(), &mut again).unwrap();
    assert_eq!(again, compact);
}

#[test]
fn rejects_malformed_input() {
    let cases = [
        "",
        "{",
        "[1,]",
        r#"{"a":}"#,
        r#"{"a" 1}"#,
        "01",
        "1.",
        "1e",
        r#""\uD800""#,
        r#""\uDC00""#,
        r#""\q""#,
        "\"line\nbreak\"",
        "true false",
    ];

    for case in cases {
        let mut arena = [JsonNode::EMPTY; 64];
        assert!(
            JsonDocument::parse(case, &mut arena).is_err(),
            "accepted {case:?}"
        );
    }
}

#[test]
fn enforces_depth_and_node_limits() {
    let mut deep = String::new();
    for _ in 0..feos_json::MAX_DEPTH + 6 {
        deep.push('[');
    }
    deep.push('0');
    for _ in 0..feos_json::MAX_DEPTH + 6 {
        deep.push(']');
    }
    let mut arena = [JsonNode::EMPTY; 256];
    assert_eq!(
        JsonDocument::parse(&deep, &mut arena).err(),
        Some(JsonError::DepthLimit)
    );

    let mut small = [JsonNode::EMPTY; 2];
    assert_eq!(
        JsonDocument::parse("[1,2,3]", &mut small).err(),
        Some(JsonError::NodeLimit)
    );
}

#[test]
fn kinds_are_queryable() {
    let mut arena = [JsonNode::EMPTY; 16];
    let doc = doc(
        r#"{"s":"v","n":1,"b":true,"z":null,"a":[],"o":{}}"#,
        &mut arena,
    );
    let root = doc.root();
    assert!(matches!(doc.kind(root), Some(Kind::Object)));
    assert!(matches!(
        doc.kind(doc.get(root, "s").unwrap()),
        Some(Kind::String(_))
    ));
    assert!(matches!(
        doc.kind(doc.get(root, "n").unwrap()),
        Some(Kind::Number("1"))
    ));
    assert!(matches!(
        doc.kind(doc.get(root, "b").unwrap()),
        Some(Kind::Bool(true))
    ));
    assert!(matches!(
        doc.kind(doc.get(root, "z").unwrap()),
        Some(Kind::Null)
    ));
}
