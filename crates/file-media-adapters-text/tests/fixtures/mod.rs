pub(crate) fn utf8_text() -> Vec<u8> {
    "alpha\nβeta\n".as_bytes().to_vec()
}

pub(crate) fn truncated_utf8() -> Vec<u8> {
    vec![b'a', 0xe2, 0x82]
}

pub(crate) fn json_document() -> Vec<u8> {
    br#"{"name":"fixture","values":[1,2,3]}"#.to_vec()
}

pub(crate) fn json_document_value() -> serde_json::Value {
    serde_json::json!({"name":"fixture","values":[1,2,3]})
}

pub(crate) fn arbitrary_precision_json() -> Vec<u8> {
    br#"{"decimal":1.2345678901234567890123456789,"integer":18446744073709551616}"#.to_vec()
}

pub(crate) fn truncated_json() -> Vec<u8> {
    br#"{"name":"fixture""#.to_vec()
}

pub(crate) fn duplicate_member_json() -> Vec<u8> {
    br#"{"role":"user","role":"admin"}"#.to_vec()
}

pub(crate) fn pretty_json_document() -> Vec<u8> {
    b"{\n  \"name\": \"fixture\",\n  \"values\": [1, 2, 3]\n}\n".to_vec()
}

pub(crate) fn bracket_prefixed_prose() -> Vec<u8> {
    b"[section]\nbody".to_vec()
}

pub(crate) fn json_token_prefixed_prose() -> Vec<u8> {
    b"[todo]\nbody".to_vec()
}

pub(crate) fn json_at_structured_depth() -> Vec<u8> {
    format!("{}0{}", "[".repeat(64), "]".repeat(64)).into_bytes()
}

pub(crate) fn json_beyond_structured_depth() -> Vec<u8> {
    format!("{}0{}", "[".repeat(65), "]".repeat(65)).into_bytes()
}

pub(crate) fn json_beyond_serde_recursion_limit() -> Vec<u8> {
    format!("{{\"value\":{}0{}}}", "[".repeat(128), "]".repeat(128)).into_bytes()
}

pub(crate) fn bracketed_numeric_csv() -> Vec<u8> {
    b"[1,2\n3,4\n".to_vec()
}

pub(crate) fn complete_json_arrays_as_csv() -> Vec<u8> {
    b"[1,2]\n[3,4]\n".to_vec()
}

pub(crate) fn complete_json_array_followed_by_prose() -> Vec<u8> {
    b"[1,2]\nbody".to_vec()
}

pub(crate) fn complete_json_prefix_followed_outside_probe() -> Vec<u8> {
    let mut bytes = b"[1,2]".to_vec();
    bytes.extend_from_slice(&vec![b' '; 4_091]);
    bytes.extend_from_slice(b"body");
    bytes
}

pub(crate) fn json_with_scalar_split_at_probe_boundary() -> Vec<u8> {
    let mut bytes = br#"{"padding":""#.to_vec();
    bytes.extend_from_slice(&vec![b'a'; 4_095 - bytes.len()]);
    bytes.extend_from_slice("β\",\"value\":1}".as_bytes());
    bytes
}

pub(crate) fn deeply_nested_json_within_source_ceiling() -> Vec<u8> {
    format!("{}0{}", "[".repeat(60_000), "]".repeat(60_000)).into_bytes()
}

pub(crate) fn json_beyond_container_entry_ceiling() -> Vec<u8> {
    let mut bytes = b"[".to_vec();
    bytes.extend_from_slice(b"0,".repeat(10_000).as_slice());
    bytes.extend_from_slice(b"0]");
    bytes
}

pub(crate) fn csv_table() -> Vec<u8> {
    b"name,value\nalpha,1\nbeta,2\n".to_vec()
}

pub(crate) fn csv_table_value() -> serde_json::Value {
    serde_json::json!({
        "headers":["name","value"],
        "rows":[["alpha","1"],["beta","2"]]
    })
}

pub(crate) fn one_column_csv() -> Vec<u8> {
    b"header\nvalue\n".to_vec()
}

pub(crate) fn header_only_csv() -> Vec<u8> {
    b"name,value\n".to_vec()
}

pub(crate) fn truncated_csv() -> Vec<u8> {
    b"name,value\nalpha,\"unterminated\n".to_vec()
}

pub(crate) fn csv_with_quotes_inside_unquoted_field() -> Vec<u8> {
    b"h1,h2\nab\"cd\"ef,x\n".to_vec()
}

pub(crate) fn csv_with_blank_record() -> Vec<u8> {
    b"h1,h2\nv1,v2\n\nv3,v4\n".to_vec()
}

pub(crate) fn prose_with_comma_and_newline() -> Vec<u8> {
    b"Hello, world\nnext line".to_vec()
}

pub(crate) fn csv_with_partial_third_probe_record() -> Vec<u8> {
    let mut bytes = b"name,value\n".to_vec();
    bytes.extend_from_slice(&vec![b'a'; 4_060]);
    bytes.extend_from_slice(b",1\nthird,\"");
    bytes.extend_from_slice(&[b'b'; 100]);
    bytes.extend_from_slice(b"\"\n");
    bytes
}

pub(crate) fn csv_with_scalar_split_at_probe_boundary() -> Vec<u8> {
    let mut bytes = b"h1,h2\nv1,v2\nthird,".to_vec();
    bytes.extend_from_slice(&vec![b'a'; 4_095 - bytes.len()]);
    bytes.extend_from_slice("β\n".as_bytes());
    bytes
}

pub(crate) fn row_bomb_csv() -> Vec<u8> {
    let mut bytes = b"name,value\n".to_vec();
    for _ in 0..10_001 {
        bytes.extend_from_slice(b"a,1\n");
    }
    bytes
}

pub(crate) fn oversized(fill: u8) -> Vec<u8> {
    vec![fill; signalbox_file_media_adapters_text::MAX_TEXT_FAMILY_BYTES as usize + 1]
}
