//! Box-drawing table rendering.

pub(crate) fn render(headers: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
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
    let mut rendered = String::new();
    rendered.push_str(&border(&widths, '┌', '┬', '┐'));
    rendered.push('\n');
    rendered.push_str(&content_row(headers.iter().copied(), &widths));
    rendered.push('\n');
    rendered.push_str(&border(&widths, '├', '┼', '┤'));
    rendered.push('\n');
    for row in rows {
        rendered.push_str(&content_row(row.iter().map(String::as_str), &widths));
        rendered.push('\n');
    }
    rendered.push_str(&border(&widths, '└', '┴', '┘'));
    rendered.push('\n');
    rendered
}

fn border(widths: &[usize], left: char, junction: char, right: char) -> String {
    let mut line = String::new();
    line.push(left);
    for (column, width) in widths.iter().enumerate() {
        if column > 0 {
            line.push(junction);
        }
        line.extend(std::iter::repeat_n('─', width + 2));
    }
    line.push(right);
    line
}

fn content_row<'a>(cells: impl Iterator<Item = &'a str>, widths: &[usize]) -> String {
    let mut line = String::new();
    line.push('│');
    for (cell, width) in cells.zip(widths) {
        line.push(' ');
        line.push_str(cell);
        line.extend(std::iter::repeat_n(' ', width - cell.chars().count()));
        line.push_str(" │");
    }
    line
}
