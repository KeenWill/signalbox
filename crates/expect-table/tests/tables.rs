use signalbox_expect_table::{cases, table, transposed};

#[test]
fn table_keeps_debug_content_opaque() {
    #[derive(Debug)]
    struct Row {
        value: i32,
    }

    let row = Row { value: 7 };
    assert_eq!(row.value, 7);
    let rendered = table([row]);

    assert!(rendered.contains("Row { value: 7 }"));
    assert!(!rendered.contains("│ value │\n"));
}

#[test]
fn cases_keeps_input_and_output_columns() {
    let rendered = cases([2], |value| value * 3);

    assert!(rendered.contains("│ input │ output │"));
    assert!(rendered.contains("│ 2     │ 6      │"));
}

#[test]
fn transposed_does_not_infer_fields() {
    let rendered = transposed(&(1, 2));

    assert!(rendered.contains("│ field │ value  │"));
    assert!(rendered.contains("│       │ (1, 2) │"));
}
