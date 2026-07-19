//! Table rendering for [`expect-test`] snapshots from any `Debug` value.
//!
//! Snapshot tables in this workspace follow `docs/testing-style.md` rules
//! 9–12: deterministic ordering, relevant fields only, right-trimmed lines
//! that stay byte-stable under re-blessing. This crate renders such tables
//! from plain `T: Debug` rows — no serde, no derive, no annotations: each row
//! is formatted with `{:?}` and the derived-`Debug` grammar is parsed back
//! into a value tree by a hand-written parser that never fails (an
//! unrecognized region, such as a custom `Debug` impl's output, degrades to
//! one verbatim atomic cell).
//!
//! Rendering rules:
//!
//! - Columns are struct fields, unioned across rows in first-appearance
//!   order; the top-level struct or variant name itself is not rendered.
//! - Nested structs flatten to dotted columns (`origin.position`) up to
//!   three path segments by default ([`Table::max_depth`] adjusts); deeper
//!   values render as one compact cell.
//! - `None` renders as an empty cell and `Some` unwraps to its payload;
//!   missing fields render empty; string and char cells drop their quotes.
//! - Unit and tuple enum variants render compactly (`Variant`,
//!   `Variant(payload)`). A struct variant is indistinguishable from a
//!   nested struct in the derived-`Debug` grammar, so its payload flattens
//!   to dotted columns the same way; below the depth limit it renders
//!   compactly as `Variant { field: payload }`.
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
            let output = f(&input);
            vec![whole_cell(&input), whole_cell(&output)]
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
        .map(|(field, cell)| vec![field, cell])
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
        let per_row: Vec<Vec<(String, String)>> = self
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

        let rows: Vec<Vec<String>> = per_row
            .iter()
            .map(|cells| {
                headers
                    .iter()
                    .map(|header| {
                        cells
                            .iter()
                            .find(|(column, _)| column == header)
                            .map(|(_, cell)| cell.clone())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .collect();

        formatter.write_str(&render::render(&headers, &rows))
    }
}
