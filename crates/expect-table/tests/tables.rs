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

/// `Some` unwraps to its payload and a unit `None` renders as the literal
/// text `None` in a flat column — the grammar cannot tell `Option::None`
/// from a domain unit variant named `None`, so neither is erased. A `None`
/// cell also keeps the column left-aligned: `None` is not a number.
#[test]
fn none_renders_literally_and_some_unwraps_to_its_payload() {
    #[derive(Debug)]
    struct Optional {
        checked: Option<u32>,
        label: Option<&'static str>,
    }

    expect![[r#"
        ┌─────────┬─────────┐
        │ checked │ label   │
        ├─────────┼─────────┤
        │ 7       │ None    │
        │ None    │ present │
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

/// A unit variant named `None` on a non-`Option` enum is a real domain
/// value — signalbox's `TranscriptAncestry::None` is one — and renders as
/// the literal text `None`, never as an erased empty cell.
#[test]
fn non_option_none_variant_renders_literally() {
    #[derive(Debug)]
    enum Ancestry {
        None,
        Forked,
    }

    #[derive(Debug)]
    struct Row {
        label: &'static str,
        ancestry: Ancestry,
    }

    expect![[r#"
        ┌───────┬──────────┐
        │ label │ ancestry │
        ├───────┼──────────┤
        │ root  │ None     │
        │ child │ Forked   │
        └───────┴──────────┘
    "#]]
    .assert_eq(&table([
        Row {
            label: "root",
            ancestry: Ancestry::None,
        },
        Row {
            label: "child",
            ancestry: Ancestry::Forked,
        },
    ]));
}

/// When rows mix `None` with `Some(Inner { .. })`, the dotted descendants
/// carry the data and the redundant bare prefix column — holding only
/// `None` and empty cells — is suppressed. The asymmetry is deliberate and
/// visible in one table: `None` stays literal in the flat `flat` column,
/// while the `None` row under the flattened `nested` prefix reads as an
/// empty run of descendant cells.
#[test]
fn redundant_none_prefix_column_is_suppressed_when_descendants_exist() {
    #[derive(Debug)]
    struct Inner {
        x: u32,
        y: u32,
    }

    #[derive(Debug)]
    struct Row {
        label: &'static str,
        flat: Option<u32>,
        nested: Option<Inner>,
    }

    expect![[r#"
        ┌───────┬──────┬──────────┬──────────┐
        │ label │ flat │ nested.x │ nested.y │
        ├───────┼──────┼──────────┼──────────┤
        │ has   │ None │        7 │       11 │
        │ hasnt │ 3    │          │          │
        └───────┴──────┴──────────┴──────────┘
    "#]]
    .assert_eq(&table([
        Row {
            label: "has",
            flat: None,
            nested: Some(Inner { x: 7, y: 11 }),
        },
        Row {
            label: "hasnt",
            flat: Some(3),
            nested: None,
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

/// A degraded mid-struct atom whose text carries commas inside its own
/// brackets — parenthesized as in `PhantomData<(u32, u32)>` or
/// angle-bracketed only as in `PhantomData<Result<u32, u32>>` — ends at
/// the enclosing field separator, not at an interior comma, so sibling
/// fields survive and the row keeps its columns.
#[test]
fn degraded_atom_with_interior_commas_leaves_siblings_intact() {
    #[derive(Debug)]
    struct HoldsCommaGeneric {
        before: u8,
        tag: PhantomData<(u32, u32)>,
        route: PhantomData<Result<u32, u32>>,
        after: u8,
    }

    expect![[r#"
        ┌────────┬─────────────────────────┬─────────────────────────────────────────────┬───────┐
        │ before │ tag                     │ route                                       │ after │
        ├────────┼─────────────────────────┼─────────────────────────────────────────────┼───────┤
        │      1 │ PhantomData<(u32, u32)> │ PhantomData<core::result::Result<u32, u32>> │     2 │
        └────────┴─────────────────────────┴─────────────────────────────────────────────┴───────┘
    "#]]
    .assert_eq(&table([HoldsCommaGeneric {
        before: 1,
        tag: PhantomData,
        route: PhantomData,
        after: 2,
    }]));
}

/// A custom `Debug` leaf nested in a struct field may print a bare,
/// unbracketed comma (`x, y`); the comma belongs to the leaf — what
/// follows it is not the `field:` grammar — so the leaf degrades alone
/// and every sibling column survives.
#[test]
fn custom_debug_leaf_with_unbracketed_comma_keeps_sibling_columns() {
    struct Pair;

    impl std::fmt::Debug for Pair {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "x, y")
        }
    }

    #[derive(Debug)]
    struct Holds {
        before: u8,
        pair: Pair,
        after: u8,
    }

    expect![[r#"
        ┌────────┬──────┬───────┐
        │ before │ pair │ after │
        ├────────┼──────┼───────┤
        │      1 │ x, y │     2 │
        └────────┴──────┴───────┘
    "#]]
    .assert_eq(&table([Holds {
        before: 1,
        pair: Pair,
        after: 2,
    }]));
}

/// A hostile custom `Debug` leaf whose text mimics the field grammar —
/// `foo, bar: baz` — splits at the comma, because inside a struct body a
/// comma followed by `identifier:` is indistinguishable from a real field
/// boundary. This snapshot pins the observed best-effort degradation; it
/// is not a promise. What the crate does promise is that the enclosing
/// struct keeps its genuine sibling columns (`count` survives).
#[test]
fn hostile_leaf_mimicking_a_field_boundary_splits_best_effort() {
    struct Hostile;

    impl std::fmt::Debug for Hostile {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "foo, bar: baz")
        }
    }

    #[derive(Debug)]
    struct Holds {
        leaf: Hostile,
        count: u8,
    }

    expect![[r#"
        ┌──────┬─────┬───────┐
        │ leaf │ bar │ count │
        ├──────┼─────┼───────┤
        │ foo  │ baz │     4 │
        └──────┴─────┴───────┘
    "#]]
    .assert_eq(&table([Holds {
        leaf: Hostile,
        count: 4,
    }]));
}

/// A custom `Debug` impl emitting a raw newline cannot split a table row:
/// control characters escape at cell rendering (`escape_debug`-style), so
/// one logical row is always one physical, correctly padded line.
#[test]
fn multiline_custom_debug_output_stays_on_one_physical_line() {
    struct Multiline;

    impl std::fmt::Debug for Multiline {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(formatter, "first\nsecond")
        }
    }

    #[derive(Debug)]
    struct Holds {
        block: Multiline,
        count: u8,
    }

    expect![[r#"
        ┌───────────────┬───────┐
        │ block         │ count │
        ├───────────────┼───────┤
        │ first\nsecond │     4 │
        └───────────────┴───────┘
    "#]]
    .assert_eq(&table([Holds {
        block: Multiline,
        count: 4,
    }]));
}

/// A string containing a real newline and one containing a literal
/// backslash-n render distinctly: escape sequences stay exactly as derived
/// `Debug` emits them, minus only the surrounding quotes, so a snapshot
/// can tell the two values apart.
#[test]
fn real_newline_and_literal_backslash_n_strings_render_distinctly() {
    #[derive(Debug)]
    struct Text {
        content: &'static str,
    }

    expect![[r#"
        ┌─────────┐
        │ content │
        ├─────────┤
        │ a\nb    │
        │ a\\nb   │
        └─────────┘
    "#]]
    .assert_eq(&table([
        Text { content: "a\nb" },
        Text { content: "a\\nb" },
    ]));
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

/// Derived `Debug` writes a singleton tuple with a trailing comma —
/// `(1,)` — and the cell keeps that form: the trailing comma is the
/// grammar's own mark of a one-element tuple, not an empty extra item. A
/// one-element tuple struct has no trailing comma in the grammar and
/// renders without one.
#[test]
fn singleton_tuple_cells_keep_the_trailing_comma_form() {
    #[derive(Debug)]
    struct Wrapped(u32);

    #[derive(Debug)]
    struct Holds {
        single: (u32,),
        wrapped: Wrapped,
        count: u8,
    }

    expect![[r#"
        ┌────────┬────────────┬───────┐
        │ single │ wrapped    │ count │
        ├────────┼────────────┼───────┤
        │ (1,)   │ Wrapped(9) │     4 │
        └────────┴────────────┴───────┘
    "#]]
    .assert_eq(&table([Holds {
        single: (1,),
        wrapped: Wrapped(9),
        count: 4,
    }]));
}

/// A struct enum variant is indistinguishable from a nested struct in the
/// derived-`Debug` grammar, so its payload flattens to dotted columns the
/// same way, and a unit-variant row leaves those columns empty. The bare
/// `shape` column stays alongside its descendants because `Unit` is real
/// content — only a bare prefix column holding nothing but `None` and
/// empty cells is suppressed.
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
/// string literal; the quote keeps its `Debug` escape, as every escape
/// sequence in a string body does.
#[test]
fn strings_keep_hostile_content_verbatim_in_cells() {
    expect![[r#"
        ┌─────────────┬───────┐
        │ sensor      │ value │
        ├─────────────┼───────┤
        │ a { b, \" c │     1 │
        └─────────────┴───────┘
    "#]]
    .assert_eq(&table([Reading {
        sensor: "a { b, \" c",
        value: 1,
    }]));
}

/// `cases` renders one `input | output` row per input in input order; a
/// `None` outcome stays literal, so an absent result is never mistaken for
/// an empty one.
#[test]
fn cases_renders_one_input_output_row_per_input() {
    expect![[r#"
        ┌───────┬────────┐
        │ input │ output │
        ├───────┼────────┤
        │     3 │ None   │
        │     5 │ 0      │
        │    12 │ 7      │
        └───────┴────────┘
    "#]]
    .assert_eq(&cases([3u8, 5, 12], |n| n.checked_sub(5)));
}

/// `cases` renders each input before invoking the callback, so a callback
/// mutating its input through interior mutability cannot rewrite the
/// reported input: the `input` column shows the pre-call value.
#[test]
fn cases_reports_inputs_as_rendered_before_the_callback_runs() {
    use std::cell::Cell;

    expect![[r#"
        ┌───────────────────┬────────┐
        │ input             │ output │
        ├───────────────────┼────────┤
        │ Cell { value: 1 } │      1 │
        │ Cell { value: 2 } │      2 │
        └───────────────────┴────────┘
    "#]]
    .assert_eq(&cases([Cell::new(1u32), Cell::new(2)], |cell| {
        let seen = cell.get();
        cell.set(99);
        seen
    }));
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
