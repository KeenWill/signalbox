//! Table rendering for [`expect-test`] snapshots from any `Debug` value.
//!
//! Snapshot tables in this workspace follow `docs/agents/testing-style.md` rules
//! 9–12: deterministic ordering, relevant fields only, right-trimmed lines
//! that stay byte-stable under re-blessing. This crate renders such tables
//! from plain `T: Debug` rows — no serde, no derive, no annotations: each row
//! is formatted with `{:?}` and the derived-`Debug` grammar is parsed back
//! into a value tree by a hand-written parser that never fails (an
//! unrecognized region, such as a custom `Debug` impl's output, degrades to
//! one verbatim atomic cell).
//!
//! Degradation is best-effort and asymmetric about bare commas. In a
//! struct body a custom `Debug` leaf that prints an unbracketed comma
//! (`x, y`) stays one cell and keeps its sibling columns, because a real
//! field boundary — `identifier:` (never `::`) or the closing brace — is
//! recognizable ahead of the comma. A map value degrades the same way: a
//! comma ends the entry only when a map-entry boundary — the next key
//! region reaching its `": "` colon-space separator, or the map's
//! closing brace — follows, so a comma-printing custom value keeps its
//! key/value association, sibling entries keep parsing, and entry
//! sorting still applies over intact entries. Inside tuples, lists, and
//! sets no such signal exists, so every depth-zero comma separates items
//! and a comma-printing custom leaf splits there. A hostile leaf whose
//! text itself mimics the boundary grammar may still split: `foo, bar:
//! baz` inside a struct, a map value printing `x: y, z: w` — which
//! splits a phantom entry, sorted with the rest — or a custom map key
//! whose own text contains `": "`, indistinguishable from the key/value
//! separator, are best-effort observed behavior, not promises. Field names using non-ASCII XID-continue characters
//! that are not alphanumeric — combining marks such as in `x́` — stop the
//! field grammar and degrade the row to the single `value` column: full
//! Unicode XID tables would require a dependency this zero-dependency
//! crate deliberately omits, and the degradation is local and verbatim.
//!
//! Rendering rules:
//!
//! - Columns are struct fields, unioned across rows in first-appearance
//!   order; the top-level struct or variant name itself is not rendered.
//! - Nested structs flatten to dotted columns (`origin.position`) up to
//!   three path segments by default ([`Table::max_depth`] adjusts); deeper
//!   values render as one compact cell.
//! - `Some` unwraps to its payload; a unit `None` renders as the literal
//!   text `None`, because the derived-`Debug` grammar cannot tell
//!   `Option::None` from a domain unit variant named `None` and erasing a
//!   domain value is the worse failure. (A non-`Option` single-item tuple
//!   variant named `Some` would unwrap too; that collision is accepted as
//!   implausible.) Missing fields render empty.
//! - When dotted descendant columns exist for a prefix, a bare prefix
//!   column is suppressed as redundant only when every cell in it is
//!   structurally absent — the row does not carry the field, or the field
//!   is a unit `None` leaf — so rows mixing `None` with
//!   `Some(Inner { .. })` render descendants only. Suppression consults
//!   provenance carried from the parse tree, never rendered text: an
//!   observed `""` or the literal string `"None"` is a value and keeps
//!   its column. The asymmetry is deliberate: `None` stays literal in a
//!   flat column but reads as an empty run of descendant cells under a
//!   flattened prefix.
//! - Cells follow one escaping story: string and char cells drop only
//!   their surrounding quotes and keep every derived-`Debug` escape
//!   sequence in the body verbatim, so a real newline (rendered `\n`) and
//!   a literal backslash-n (rendered `\\n`) stay distinct. Quotes drop
//!   only when a body remains: an observed empty string renders
//!   quotes-kept as `""`, never as an empty cell a missing field could be
//!   confused with. Raw control characters — reachable only through
//!   custom `Debug` output — are escaped `escape_debug`-style, so one
//!   logical row is always one physical line. An unterminated literal —
//!   custom `Debug` output opening a quote it never closes — is not a
//!   literal: it degrades verbatim, quotes kept, never quote-stripped.
//! - Braced map `Debug` output (`{1: "a"}`) parses entry by entry, and
//!   set output (`{"a", "b"}` — entries without `: `) likewise; a braced
//!   region following neither grammar degrades to one verbatim atom.
//!   Keys split from values only at the `": "` colon-space separator
//!   compact `DebugMap` output emits, so atomic key text with interior
//!   colons (an `Ipv6Addr` key renders `{2001:db8::1: 10}`) stays whole
//!   and an `Ipv6Addr` set stays a set.
//!   Entries render in sorted-by-rendered-text order — maps by key text
//!   (value text tie-breaking), sets by entry text — not iteration
//!   order: a deliberate normalization so `HashMap`- and
//!   `HashSet`-bearing rows render byte-identically across processes.
//!   `BTreeMap` output with single-token keys typically renders
//!   unchanged, but textual order is not `Ord` order: key `10` sorts
//!   before key `2`.
//! - Unit and tuple enum variants render compactly (`Variant`,
//!   `Variant(payload)`). A struct variant is indistinguishable from a
//!   nested struct in the derived-`Debug` grammar, so its payload flattens
//!   to dotted columns the same way; below the depth limit it renders
//!   compactly as `Variant { field: payload }`.
//! - A `finish_non_exhaustive` struct keeps its `..` marker in compact
//!   cells (`Redacted { shown: 1, .. }`; a fieldless `Secret { .. }` stays
//!   one verbatim leaf). As a row or flattened prefix its shown fields
//!   still become columns, and the marker — naming no field — contributes
//!   no column.
//! - Borders are Unicode box-drawing characters, cells pad by char count, a
//!   column whose non-empty cells all parse as integers or floats is
//!   right-aligned, every line is right-trimmed, and the table ends with one
//!   trailing newline. Ordering never comes from a `HashMap`.
//!
//! Prior art: Jane Street's [expectable] (`print`, `print_cases`,
//! `print_record_transposed`) rendered through OCaml `Ascii_table`-style
//! boxes, hosted here on [`expect-test`] snapshots.
//!
//! [expectable]: https://github.com/janestreet/expectable
//! [`expect-test`]: https://github.com/rust-analyzer/expect-test

mod parse;
mod render;

use std::fmt::{self, Debug, Display};

/// Renders rows as one table, one row per item, columns inferred from
/// struct fields with the crate's default options.
///
/// Accepts owned rows or references — `&Vec<T>` and `slice.iter()` work
/// because `&T: Debug` wherever `T: Debug`.
///
/// An empty row set renders as the empty string: with no rows there are no
/// columns to infer.
///
/// ```
/// #[derive(Debug)]
/// struct Reading {
///     sensor: &'static str,
///     value: i32,
/// }
///
/// let rendered = signalbox_expect_table::table([
///     Reading { sensor: "left", value: 12 },
///     Reading { sensor: "right", value: 7 },
/// ]);
/// assert_eq!(rendered, "\
/// ┌────────┬───────┐
/// │ sensor │ value │
/// ├────────┼───────┤
/// │ left   │    12 │
/// │ right  │     7 │
/// └────────┴───────┘
/// ");
/// ```
pub fn table<T: Debug>(rows: impl IntoIterator<Item = T>) -> String {
    Table::new(rows).to_string()
}

/// Renders one `input | output` row per input, applying `f` to each: the
/// two-column shape of expectable's `print_cases`.
///
/// Cells render whole-value compact (no field flattening); an empty input
/// set renders as the empty string.
pub fn cases<I: Debug, O: Debug>(
    inputs: impl IntoIterator<Item = I>,
    mut f: impl FnMut(&I) -> O,
) -> String {
    let rows: Vec<Vec<String>> = inputs
        .into_iter()
        .map(|input| {
            // The input renders before the callback runs, so interior
            // mutability inside `f` cannot rewrite the reported input.
            let input_cell = whole_cell(&input);
            let output = f(&input);
            vec![input_cell, whole_cell(&output)]
        })
        .collect();
    if rows.is_empty() {
        return String::new();
    }
    render::render(&["input".to_string(), "output".to_string()], &rows)
}

/// Renders one record with fields as `field | value` rows: the transposed
/// shape of expectable's `print_record_transposed`.
///
/// Fields flatten to dotted paths exactly as [`table`] columns do; a
/// non-struct value renders as one `value` row.
pub fn transposed<T: Debug>(value: &T) -> String {
    let parsed = parse::parse(&format!("{value:?}"));
    let rows: Vec<Vec<String>> = render::row_cells(&parsed, render::DEFAULT_MAX_DEPTH)
        .into_iter()
        .map(|(field, cell)| vec![field, cell.text])
        .collect();
    render::render(&["field".to_string(), "value".to_string()], &rows)
}

fn whole_cell<T: Debug>(value: &T) -> String {
    render::cell(&parse::parse(&format!("{value:?}")))
}

/// A table over parsed rows with adjustable rendering options; [`table`] is
/// the one-call shorthand for the defaults.
///
/// Options stay deliberately few — this is a snapshot renderer, not a
/// formatting framework.
#[must_use]
pub struct Table {
    rows: Vec<parse::Value>,
    max_depth: usize,
}

impl Table {
    /// Parses each row's `Debug` rendering; render with [`Display`] or
    /// `to_string`.
    pub fn new<T: Debug>(rows: impl IntoIterator<Item = T>) -> Self {
        Self {
            rows: rows
                .into_iter()
                .map(|row| parse::parse(&format!("{row:?}")))
                .collect(),
            max_depth: render::DEFAULT_MAX_DEPTH,
        }
    }

    /// Sets how many dotted path segments a flattened column may have;
    /// structs below the limit render as one compact cell. Values below 1
    /// are treated as 1.
    pub fn max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth.max(1);
        self
    }
}

impl Display for Table {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let per_row: Vec<Vec<(String, render::RenderedCell)>> = self
            .rows
            .iter()
            .map(|row| render::row_cells(row, self.max_depth))
            .collect();

        let mut headers: Vec<String> = Vec::new();
        for cells in &per_row {
            for (column, _) in cells {
                if !headers.contains(column) {
                    headers.push(column.clone());
                }
            }
        }

        // The dense grid keeps each cell's provenance — missing field,
        // unit `None`, or observed value — so suppression below decides
        // structurally, never from rendered text.
        let rows: Vec<Vec<render::RenderedCell>> = per_row
            .iter()
            .map(|cells| {
                headers
                    .iter()
                    .map(|header| {
                        cells
                            .iter()
                            .find(|(column, _)| column == header)
                            .map(|(_, cell)| cell.clone())
                            .unwrap_or_else(render::RenderedCell::missing)
                    })
                    .collect()
            })
            .collect();

        let (headers, rows) = render::suppress_redundant_prefix_columns(headers, rows);
        formatter.write_str(&render::render(&headers, &rows))
    }
}
