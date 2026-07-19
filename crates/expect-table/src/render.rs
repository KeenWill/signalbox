//! Cell display, dotted flattening, and box-drawing table rendering.

use crate::parse::Value;

/// Default nesting depth for dotted column flattening: paths reach three
/// segments (`a.b.c`); structs any deeper render as one compact cell.
pub(crate) const DEFAULT_MAX_DEPTH: usize = 3;

/// Column header for rows that are not structs and so carry no field names.
pub(crate) const VALUE_COLUMN: &str = "value";

/// Returns the value inside any number of `Some` wrappers, so options render
/// as their payload (and `None`, an atom, renders as an empty cell).
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

/// Renders one whole-cell value: `None` is empty, `Some` unwraps, string and
/// char literals drop their quotes (keeping non-quote escapes visible, so a
/// cell can never contain a control character), everything else is compact.
pub(crate) fn cell(value: &Value) -> String {
    match through_some(value) {
        Value::Atom(text) if text == "None" => String::new(),
        Value::Str(body) | Value::Char(body) => unescape_quotes(body),
        other => compact(other),
    }
}

/// Unescapes only `\"`, `\'`, and `\\`; every other escape stays visible as
/// written, so `Debug`-escaped control characters cannot break a table line.
fn unescape_quotes(body: &str) -> String {
    let mut unescaped = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(next) = chars.next() {
        if next != '\\' {
            unescaped.push(next);
            continue;
        }
        match chars.next() {
            Some(escaped @ ('"' | '\'' | '\\')) => unescaped.push(escaped),
            Some(other) => {
                unescaped.push('\\');
                unescaped.push(other);
            }
            None => unescaped.push('\\'),
        }
    }
    unescaped
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
