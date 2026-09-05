use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use signalbox_expect_table::{cases, table};

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
fn ordered_projections_render_unordered_collections_deterministically() {
    let first = HashMap::from([("z", HashSet::from([3, 1])), ("a", HashSet::from([2]))]);
    let second = HashMap::from([("a", HashSet::from([2])), ("z", HashSet::from([1, 3]))]);
    let project = |map: HashMap<_, HashSet<_>>| {
        map.into_iter()
            .map(|(key, values)| (key, values.into_iter().collect::<BTreeSet<_>>()))
            .collect::<BTreeMap<_, _>>()
    };
    let rendered = table([project(first)]);
    assert_eq!(rendered, table([project(second)]));
    assert!(rendered.contains(r#"{"a": {2}, "z": {1, 3}}"#));
}
