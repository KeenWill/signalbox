//! Opaque [`Debug`] rendering for [`expect-test`] snapshot tables.
//!
//! Values are never parsed or normalized. Each input becomes one `value` cell
//! containing its exact `Debug` rendering, with raw control characters escaped
//! so a logical row cannot create additional physical table rows.
//! Callers must supply deterministic `Debug` projections, including nested
//! values: convert `HashMap`/`HashSet` to ordered collections or sorted rows.
//!
//! [`expect-test`]: https://github.com/rust-analyzer/expect-test

mod render;

use std::fmt::{self, Debug, Display};

/// Renders one opaque `value` column, one row per item.
/// Rows must have deterministic [`Debug`] output; project unordered collections first.
pub fn table<T: Debug>(rows: impl IntoIterator<Item = T>) -> String {
    Table::new(rows).to_string()
}

/// Renders one opaque `input | output` row per input, applying `f` to each.
/// Both inputs and outputs must have deterministic [`Debug`] output.
pub fn cases<I: Debug, O: Debug>(
    inputs: impl IntoIterator<Item = I>,
    mut f: impl FnMut(&I) -> O,
) -> String {
    let rows: Vec<Vec<String>> = inputs
        .into_iter()
        .map(|input| {
            let input_cell = opaque_cell(&input);
            let output = f(&input);
            vec![input_cell, opaque_cell(&output)]
        })
        .collect();
    render::render(&["input", "output"], &rows)
}

fn opaque_cell<T: Debug>(value: &T) -> String {
    let raw = format!("{value:?}");
    let mut escaped = String::with_capacity(raw.len());
    for character in raw.chars() {
        if character.is_control() {
            escaped.extend(character.escape_debug());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

/// An opaque table over [`Debug`] rows.
#[must_use]
pub struct Table {
    rows: Vec<String>,
}

impl Table {
    /// Captures each row's exact [`Debug`] rendering.
    /// Rows must have deterministic [`Debug`] output; project unordered collections first.
    pub fn new<T: Debug>(rows: impl IntoIterator<Item = T>) -> Self {
        Self {
            rows: rows.into_iter().map(|row| opaque_cell(&row)).collect(),
        }
    }
}

impl Display for Table {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rows: Vec<Vec<String>> = self.rows.iter().map(|row| vec![row.clone()]).collect();
        formatter.write_str(&render::render(&["value"], &rows))
    }
}
