//! #970: `serde_json/preserve_order` is docling-core's default-on
//! `preserve-order` feature, not an unconditional dependency flag.

use docling_core::DoclingDocument;

/// Default build: docling's field order survives the export.
#[cfg(feature = "preserve-order")]
#[test]
fn default_export_keeps_docling_field_order() {
    let json = DoclingDocument::new("x").export_to_json();
    let schema = json.find("\"schema_name\"").expect("schema_name");
    let version = json.find("\"version\"").expect("version");
    let body = json.find("\"body\"").expect("body");
    assert!(schema < version && version < body, "{json}");
}

/// `default-features = false`: the consumer's own maps stay sorted.
#[cfg(not(feature = "preserve-order"))]
#[test]
fn opt_out_leaves_consumer_json_sorted() {
    let _ = DoclingDocument::new("x").export_to_json();
    let mut object = serde_json::Map::new();
    object.insert("z".into(), serde_json::json!(0));
    object.insert("a".into(), serde_json::json!({"z": 0, "a": 1}));
    assert_eq!(
        serde_json::to_string(&object).unwrap(),
        r#"{"a":{"a":1,"z":0},"z":0}"#
    );
}
