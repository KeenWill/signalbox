//! Cell display, dotted flattening, and box-drawing table rendering.

use crate::parse::Value;

/// Default nesting depth for dotted column flattening: paths reach three
/// segments (`a.b.c`); structs any deeper render as one compact cell.
pub(crate) const DEFAULT_MAX_DEPTH: usize = 3;

/// Column header for rows that are not structs and so carry no field names.
pub(crate) const VALUE_COLUMN: &str = "value";

/// Returns the value inside any number of `Some` wrappers, so options render
/// as their payload. A non-`Option` single-item tuple variant that happens
/// to be named `Some` would unwrap too; that collision is accepted as
/// implausible (see the crate docs). A unit `None` stays a plain atom and
/// renders as the literal text `None` — the grammar cannot prove it is
/// `Option::None` rather than a domain unit variant named `None`.
fn through_some(value: &Value) -> &Value {
    match value {
        Value::Tuple {
            name: Some(name),
            items,
        } if name == "Some" && items.len() == 1 => through_some(&items[0]),
        _ => value,
    }
}

/// Re-renders a parsed value on one line, `Debug`-like, for cells that hold
/// a whole nested value (enum payloads, tuples, depth-limited structs).
fn compact(value: &Value) -> String {
    match value {
        Value::Atom(text) => text.clone(),
        Value::Str(body) => format!("\"{body}\""),
        Value::Char(body) => format!("'{body}'"),
        Value::List(items) => format!("[{}]", compact_items(items)),
        // Derived `Debug` marks a singleton tuple with a trailing comma
        // (`(1,)`); the compact form keeps it. Named one-element tuples
        // (`Wrapped(9)`) carry no trailing comma in the grammar.
        Value::Tuple { name: None, items } if items.len() == 1 => {
            format!("({},)", compact(&items[0]))
        }
        Value::Tuple { name: None, items } => format!("({})", compact_items(items)),
        Value::Tuple {
            name: Some(name),
            items,
        } => format!("{name}({})", compact_items(items)),
        Value::Struct { name, fields } => {
            let fields: Vec<String> = fields
                .iter()
                .map(|(field, value)| format!("{field}: {}", compact(value)))
                .collect();
            format!("{name} {{ {} }}", fields.join(", "))
        }
    }
}

fn compact_items(items: &[Value]) -> String {
    let items: Vec<String> = items.iter().map(compact).collect();
    items.join(", ")
}

/// Renders one whole-cell value under the crate's single escaping story
/// (stated once, in the crate docs): `Some` unwraps, a unit `None` renders
/// as the literal text `None`, string and char literals drop only their
/// surrounding quotes — every escape sequence in the body stays exactly as
/// derived `Debug` emitted it, so `"a\nb"` and `"a\\nb"` render distinctly —
/// and raw control characters escape so a cell is always one physical line.
pub(crate) fn cell(value: &Value) -> String {
    let text = match through_some(value) {
        Value::Str(body) | Value::Char(body) => body.clone(),
        other => compact(other),
    };
    escape_control(&text)
}

/// Escapes raw control characters with [`char::escape_debug`] — the form
/// derived `Debug` would have emitted (`\n`, `\r`, `\t`, `\u{…}`) — so one
/// logical row is always one physical, correctly padded line. Derived
/// `Debug` never emits raw control characters; only custom `Debug` output
/// reaches this arm.
fn escape_control(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() {
            escaped.extend(character.escape_debug());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

/// Flattens one parsed row into `(column, cell)` pairs: struct fields become
/// dotted columns down to `max_depth` segments; any other value is one
/// [`VALUE_COLUMN`] cell.
pub(crate) fn row_cells(value: &Value, max_depth: usize) -> Vec<(String, String)> {
    match through_some(value) {
        Value::Struct { fields, .. } => {
            let mut cells = Vec::new();
            flatten_fields(fields, "", max_depth.max(1), &mut cells);
            cells
        }
        other => vec![(VALUE_COLUMN.to_string(), cell(other))],
    }
}

fn flatten_fields(
    fields: &[(String, Value)],
    prefix: &str,
    remaining_depth: usize,
    cells: &mut Vec<(String, String)>,
) {
    for (name, value) in fields {
        let column = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        match through_some(value) {
            Value::Struct { fields: inner, .. } if remaining_depth > 1 => {
                flatten_fields(inner, &column, remaining_depth - 1, cells);
            }
            _ => cells.push((column, cell(value))),
        }
    }
}

/// Drops each bare prefix column that is redundant with its dotted
/// descendants: when some `prefix.child` column exists and every cell of
/// the bare `prefix` column is empty or the literal `None`, the bare column
/// shows nothing its descendants do not already show — rows mixing `None`
/// with `Some(Inner { .. })` would otherwise carry both an all-but-empty
/// `nested` column and `nested.x` descendants. A bare column holding any
/// other value (a unit variant, a tuple payload, a compact struct from a
/// differently shaped row) stays. Under a suppressed prefix a `None` row
/// reads as an empty run of descendant cells; in a flat column `None` stays
/// literal — the asymmetry the crate docs state.
pub(crate) fn suppress_redundant_prefix_columns(
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
) -> (Vec<String>, Vec<Vec<String>>) {
    let redundant: Vec<bool> = headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            let dotted = format!("{header}.");
            headers.iter().any(|other| other.starts_with(&dotted))
                && rows
                    .iter()
                    .all(|row| row[column].is_empty() || row[column] == "None")
        })
        .collect();
    if !redundant.contains(&true) {
        return (headers, rows);
    }
    let keep = |cells: Vec<String>| -> Vec<String> {
        cells
            .into_iter()
            .zip(&redundant)
            .filter(|(_, redundant)| !**redundant)
            .map(|(cell, _)| cell)
            .collect()
    };
    let headers = keep(headers);
    let rows = rows.into_iter().map(keep).collect();
    (headers, rows)
}

/// Renders headers and dense rows as one Unicode box-drawing table.
///
/// Columns pad to the widest of header and cells by char count; a column
/// whose non-empty cells all parse as integers or floats is right-aligned
/// (headers stay left-aligned). Every line is right-trimmed and the table
/// ends with one trailing newline, keeping snapshots byte-stable under
/// re-blessing. Iteration order is the given slice order throughout.
pub(crate) fn render(headers: &[String], rows: &[Vec<String>]) -> String {
    if headers.is_empty() {
        return String::new();
    }
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .fold(header.chars().count(), usize::max)
        })
        .collect();
    let right_aligned: Vec<bool> = (0..headers.len())
        .map(|column| {
            let mut cells = rows
                .iter()
                .map(|row| row[column].as_str())
                .filter(|cell| !cell.is_empty());
            let Some(first) = cells.next() else {
                return false;
            };
            is_numeric(first) && cells.all(is_numeric)
        })
        .collect();

    let header_alignment = vec![false; headers.len()];
    let mut lines = Vec::with_capacity(rows.len() + 4);
    lines.push(border(&widths, '┌', '┬', '┐'));
    lines.push(content_row(headers, &widths, &header_alignment));
    lines.push(border(&widths, '├', '┼', '┤'));
    for row in rows {
        lines.push(content_row(row, &widths, &right_aligned));
    }
    lines.push(border(&widths, '└', '┴', '┘'));

    let mut rendered = String::new();
    for line in lines {
        rendered.push_str(line.trim_end());
        rendered.push('\n');
    }
    rendered
}

fn is_numeric(cell: &str) -> bool {
    cell.parse::<i128>().is_ok() || cell.parse::<f64>().is_ok()
}

fn border(widths: &[usize], left: char, junction: char, right: char) -> String {
    let mut line = String::new();
    line.push(left);
    for (column, width) in widths.iter().enumerate() {
        if column > 0 {
            line.push(junction);
        }
        for _ in 0..width + 2 {
            line.push('─');
        }
    }
    line.push(right);
    line
}

fn content_row<S: AsRef<str>>(cells: &[S], widths: &[usize], right_aligned: &[bool]) -> String {
    let mut line = String::new();
    line.push('│');
    for ((cell, width), right) in cells.iter().zip(widths).zip(right_aligned) {
        let cell = cell.as_ref();
        let padding = width - cell.chars().count();
        line.push(' ');
        if *right {
            line.extend(std::iter::repeat_n(' ', padding));
            line.push_str(cell);
        } else {
            line.push_str(cell);
            line.extend(std::iter::repeat_n(' ', padding));
        }
        line.push_str(" │");
    }
    line
}
