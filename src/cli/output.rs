//! Output helpers for CLI subcommands: aligned tables and JSON.

use serde::Serialize;
use std::io::Write;

const FALLBACK_TERMINAL_WIDTH: usize = 120;

/// Best-effort terminal width for human table formatting.
pub fn terminal_width() -> usize {
    if let Some(width) = terminal_width_from_env() {
        return width;
    }
    terminal_width_from_stdout().unwrap_or(FALLBACK_TERMINAL_WIDTH)
}

fn terminal_width_from_env() -> Option<usize> {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|w| *w > 0)
}

#[cfg(unix)]
fn terminal_width_from_stdout() -> Option<usize> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    if rc == 0 && size.ws_col > 0 {
        Some(size.ws_col as usize)
    } else {
        None
    }
}

#[cfg(not(unix))]
fn terminal_width_from_stdout() -> Option<usize> {
    None
}

/// Print rows as a simple aligned table.
///
/// Column widths are computed from the headers plus all cell contents.
/// Columns are separated by a two-space gutter, and a separator line of
/// dashes is printed between the header row and the data rows.
pub fn print_table<W: Write>(
    out: &mut W,
    headers: &[&str],
    rows: &[Vec<String>],
) -> std::io::Result<()> {
    let ncols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            if cell.len() > widths[i] {
                widths[i] = cell.len();
            }
        }
    }

    write_row(out, headers.iter().copied(), &widths)?;
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    write_row(out, sep.iter().map(|s| s.as_str()), &widths)?;
    for row in rows {
        write_row(
            out,
            (0..ncols).map(|i| row.get(i).map(|s| s.as_str()).unwrap_or("")),
            &widths,
        )?;
    }
    Ok(())
}

fn write_row<'a, W: Write, I: Iterator<Item = &'a str>>(
    out: &mut W,
    cells: I,
    widths: &[usize],
) -> std::io::Result<()> {
    let cells: Vec<&str> = cells.collect();
    let last = cells.len().saturating_sub(1);
    for (i, cell) in cells.iter().enumerate() {
        if i == last {
            // Don't pad the last column.
            write!(out, "{}", cell)?;
        } else {
            write!(out, "{:<width$}  ", cell, width = widths[i])?;
        }
    }
    writeln!(out)
}

/// Print a serializable value as pretty JSON, followed by a newline.
pub fn print_json<W: Write, T: Serialize>(out: &mut W, value: &T) -> anyhow::Result<()> {
    let s = serde_json::to_string_pretty(value)?;
    out.write_all(s.as_bytes())?;
    out.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_rows_produces_headers_and_separator() {
        let mut buf: Vec<u8> = Vec::new();
        print_table(&mut buf, &["A", "BB"], &[]).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 2, "got: {:?}", lines);
        assert!(lines[0].starts_with("A "));
        assert!(lines[0].contains("BB"));
        assert!(lines[1].starts_with("-"));
        assert!(lines[1].contains("--"));
    }

    #[test]
    fn column_widths_grow_to_longest_cell() {
        let mut buf: Vec<u8> = Vec::new();
        let rows = vec![
            vec!["short".to_string(), "x".to_string()],
            vec!["a-very-long-cell".to_string(), "y".to_string()],
        ];
        print_table(&mut buf, &["K", "V"], &rows).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        // First column width should be at least len("a-very-long-cell") = 16.
        // Header line has "K" left-padded to 16, then two spaces, then "V".
        assert!(
            lines[0].starts_with("K               "),
            "header pad wrong: {:?}",
            lines[0]
        );
        // Separator first column should be 16 dashes.
        assert!(
            lines[1].starts_with(&"-".repeat(16)),
            "sep wrong: {:?}",
            lines[1]
        );
        // Data row 1: "short" padded to 16.
        assert!(lines[2].starts_with("short           "));
        assert!(lines[3].starts_with("a-very-long-cell  y"));
    }

    #[test]
    fn json_output_is_valid_json() {
        #[derive(Serialize)]
        struct X {
            a: u32,
            b: Vec<String>,
        }
        let mut buf: Vec<u8> = Vec::new();
        let x = X {
            a: 7,
            b: vec!["hi".into(), "there".into()],
        };
        print_json(&mut buf, &x).unwrap();
        let s = String::from_utf8(buf).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["a"], 7);
        assert_eq!(v["b"][1], "there");
    }
}
