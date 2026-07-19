//! Self-hosted renderer expect-tests: each snapshot is the rendered table
//! itself, so the crate's output contract is reviewed as literal bytes
//! (`docs/testing-style.md`, rules 9, 11, 12, 15).

#![allow(
    dead_code,
    reason = "row fixtures are read only through their Debug derives"
)]

use std::marker::PhantomData;

use expect_test::expect;
use signalbox_expect_table::{Table, cases, table, transposed};

/// A two-field record whose whole story is its literal field values.
#[derive(Debug)]
struct Reading {
    sensor: &'static str,
    value: i32,
}

/// Rows of different variants union their columns in first-appearance
/// order; a field a row does not carry renders as an empty cell.
#[test]
fn column_union_follows_first_appearance_order_across_heterogeneous_rows() {
    #[derive(Debug)]
    enum Row {
        First { left: u32, shared: &'static str },
        Second { shared: &'static str, right: u32 },
    }

    expect![[r#"
        ┌──────┬────────┬───────┐
        │ left │ shared │ right │
        ├──────┼────────┼───────┤
        │    1 │ both   │       │
        │      │ again  │     2 │
        └──────┴────────┴───────┘
    "#]]
    .assert_eq(&table([
        Row::First {
            left: 1,
            shared: "both",
        },
        Row::Second {
            shared: "again",
            right: 2,
        },
    ]));
}

/// Nested struct fields become dotted columns (`origin.x`), one leaf per
/// column.
#[test]
fn nested_structs_flatten_to_dotted_columns() {
    #[derive(Debug)]
    struct Position {
        x: i32,
        y: i32,
    }

    #[derive(Debug)]
    struct Placed {
        label: &'static str,
        origin: Position,
    }

    expect![[r#"
        ┌───────┬──────────┬──────────┐
        │ label │ origin.x │ origin.y │
        ├───────┼──────────┼──────────┤
        │ start │       -3 │       40 │
        │ end   │       12 │        5 │
        └───────┴──────────┴──────────┘
    "#]]
    .assert_eq(&table([
        Placed {
            label: "start",
            origin: Position { x: -3, y: 40 },
        },
        Placed {
            label: "end",
            origin: Position { x: 12, y: 5 },
        },
    ]));
}

/// The `max_depth` knob bounds dotted paths; a struct below the limit
/// renders as one compact cell instead of flattening further.
#[test]
fn max_depth_renders_deeper_structs_as_one_compact_cell() {
    #[derive(Debug)]
    struct Leaf {
        value: u32,
    }

    #[derive(Debug)]
    struct Middle {
        leaf: Leaf,
    }

    #[derive(Debug)]
    struct Outer {
        middle: Middle,
    }

    expect![[r#"
        ┌───────────────────┐
        │ middle.leaf       │
        ├───────────────────┤
        │ Leaf { value: 9 } │
        └───────────────────┘
    "#]]
    .assert_eq(
        &Table::new([Outer {
            middle: Middle {
                leaf: Leaf { value: 9 },
            },
        }])
        .max_depth(2)
        .to_string(),
    );
}

/// A column whose non-empty cells all parse as integers or floats
/// right-aligns; any non-numeric cell keeps the column left-aligned.
#[test]
fn numeric_columns_right_align_and_mixed_columns_left_align() {
    #[derive(Debug)]
    struct Sample {
        count: u64,
        ratio: f64,
        label: &'static str,
    }

    expect![[r#"
        ┌───────┬───────┬─────────┐
        │ count │ ratio │ label   │
        ├───────┼───────┼─────────┤
        │     3 │  0.25 │ short   │
        │ 41556 │ -12.5 │ 9 lives │
        └───────┴───────┴─────────┘
    "#]]
    .assert_eq(&table([
        Sample {
            count: 3,
            ratio: 0.25,
            label: "short",
        },
        Sample {
            count: 41556,
            ratio: -12.5,
            label: "9 lives",
        },
    ]));
}

/// `None` renders as an empty cell and `Some` unwraps to its payload, in
/// both numeric and text columns.
#[test]
fn none_renders_empty_and_some_unwraps_to_its_payload() {
    #[derive(Debug)]
    struct Optional {
        checked: Option<u32>,
        label: Option<&'static str>,
    }

    expect![[r#"
        ┌─────────┬─────────┐
        │ checked │ label   │
        ├─────────┼─────────┤
        │       7 │         │
        │         │ present │
        └─────────┴─────────┘
    "#]]
    .assert_eq(&table([
        Optional {
            checked: Some(7),
            label: None,
        },
        Optional {
            checked: None,
            label: Some("present"),
        },
    ]));
}

/// A custom `Debug` impl the grammar does not cover degrades to one
/// verbatim atomic cell without disturbing sibling fields.
#[test]
fn custom_debug_leaf_renders_verbatim() {
    #[derive(Debug)]
    struct HoldsCustom {
        tag: PhantomData<u32>,
        count: u8,
    }

    expect![[r#"
        ┌──────────────────┬───────┐
        │ tag              │ count │
        ├──────────────────┼───────┤
        │ PhantomData<u32> │     4 │
        └──────────────────┴───────┘
    "#]]
    .assert_eq(&table([HoldsCustom {
        tag: PhantomData,
        count: 4,
    }]));
}

/// Unit and tuple enum variants render compactly in their field's own
/// column: bare name or one-line payload.
#[test]
fn unit_and_tuple_variant_cells_render_compactly() {
    #[derive(Debug)]
    enum Shape {
        Unit,
        Pair(u8, u8),
    }

    #[derive(Debug)]
    struct Holds {
        shape: Shape,
    }

    expect![[r#"
        ┌────────────┐
        │ shape      │
        ├────────────┤
        │ Unit       │
        │ Pair(3, 5) │
        └────────────┘
    "#]]
    .assert_eq(&table([
        Holds { shape: Shape::Unit },
        Holds {
            shape: Shape::Pair(3, 5),
        },
    ]));
}

/// A struct enum variant is indistinguishable from a nested struct in the
/// derived-`Debug` grammar, so its payload flattens to dotted columns the
/// same way, and a unit-variant row leaves those columns empty.
#[test]
fn struct_variant_payloads_flatten_like_nested_structs() {
    #[derive(Debug)]
    enum Shape {
        Unit,
        Sized { width: u16 },
    }

    #[derive(Debug)]
    struct Holds {
        shape: Shape,
    }

    expect![[r#"
        ┌───────┬─────────────┐
        │ shape │ shape.width │
        ├───────┼─────────────┤
        │ Unit  │             │
        │       │           7 │
        └───────┴─────────────┘
    "#]]
    .assert_eq(&table([
        Holds { shape: Shape::Unit },
        Holds {
            shape: Shape::Sized { width: 7 },
        },
    ]));
}

/// Rows that are not structs carry no field names and render in a single
/// `value` column.
#[test]
fn non_struct_rows_render_in_a_single_value_column() {
    expect![[r#"
        ┌───────┐
        │ value │
        ├───────┤
        │    10 │
        │     7 │
        │  1200 │
        └───────┘
    "#]]
    .assert_eq(&table([10, 7, 1200]));
}

/// String content hostile to the grammar — braces, commas, an escaped
/// quote — stays one verbatim cell because the parser reads it as one
/// string literal.
#[test]
fn strings_keep_hostile_content_verbatim_in_cells() {
    expect![[r#"
        ┌────────────┬───────┐
        │ sensor     │ value │
        ├────────────┼───────┤
        │ a { b, " c │     1 │
        └────────────┴───────┘
    "#]]
    .assert_eq(&table([Reading {
        sensor: "a { b, \" c",
        value: 1,
    }]));
}

/// `cases` renders one `input | output` row per input in input order.
#[test]
fn cases_renders_one_input_output_row_per_input() {
    expect![[r#"
        ┌───────┬────────┐
        │ input │ output │
        ├───────┼────────┤
        │     3 │        │
        │     5 │      0 │
        │    12 │      7 │
        └───────┴────────┘
    "#]]
    .assert_eq(&cases([3u8, 5, 12], |n| n.checked_sub(5)));
}

/// `transposed` renders one record with its (dotted) fields as rows.
#[test]
fn transposed_renders_one_record_with_fields_as_rows() {
    #[derive(Debug)]
    struct Position {
        x: i32,
        y: i32,
    }

    #[derive(Debug)]
    struct Placed {
        label: &'static str,
        origin: Position,
    }

    expect![[r#"
        ┌──────────┬───────┐
        │ field    │ value │
        ├──────────┼───────┤
        │ label    │ start │
        │ origin.x │ -3    │
        │ origin.y │ 40    │
        └──────────┴───────┘
    "#]]
    .assert_eq(&transposed(&Placed {
        label: "start",
        origin: Position { x: -3, y: 40 },
    }));
}

/// With no rows there are no columns to infer, so the rendering is empty.
#[test]
fn empty_row_set_renders_the_empty_string() {
    assert_eq!(table(Vec::<u8>::new()), "");
}
