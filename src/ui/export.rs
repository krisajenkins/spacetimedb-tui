//! Dependency-free CSV / JSON export for query results.
//!
//! We keep this module outside of `src/api/` because it is a pure UI
//! concern (it reads the [`QueryResult`] and writes a local file when
//! the user presses `e` / `E` on a data-grid tab).
//!
//! Both serialisers are intentionally simple:
//! - **CSV**: RFC 4180-style quoting. A cell is wrapped in `"…"` when
//!   it contains a comma, quote, CR or LF; internal quotes are doubled.
//!   Values are rendered via [`crate::ui::tabs::tables::value_to_display`]
//!   so we get the same compact representation the UI already shows.
//! - **JSON**: an array of objects keyed by column name, with the
//!   original `serde_json::Value`s preserved (no double-encoding).
//!
//! Files are written to `./exports/` in the current working directory
//! so that a user running `spacetimedb-tui` from a project folder
//! doesn't have to configure anything to get at the output.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::api::types::QueryResult;

/// Which serialisation format to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
}

impl ExportFormat {
    fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
        }
    }
}

/// Serialise `qr` into a byte vector in the chosen `format`.
///
/// Kept as a pure function so tests can exercise the serialiser
/// without touching the filesystem.
pub fn serialise(qr: &QueryResult, format: ExportFormat) -> Vec<u8> {
    match format {
        ExportFormat::Csv => serialise_csv(qr).into_bytes(),
        ExportFormat::Json => serialise_json(qr).into_bytes(),
    }
}

/// Serialise `qr` to the configured `format` and write it under
/// `./exports/<label>-<timestamp>.<ext>`. Returns the path written.
pub fn write_export(qr: &QueryResult, format: ExportFormat, label: &str) -> Result<PathBuf> {
    let dir = PathBuf::from("exports");
    fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create export dir {}", dir.display()))?;

    // Sanitise the label so weird table / db names don't produce bad
    // filenames (spaces, slashes, etc.).
    let safe_label: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let filename = format!("{safe_label}-{timestamp}.{}", format.extension());
    let path = dir.join(filename);

    let bytes = serialise(qr, format);
    let mut f =
        fs::File::create(&path).with_context(|| format!("Failed to create {}", path.display()))?;
    f.write_all(&bytes)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    f.flush()
        .with_context(|| format!("Failed to flush {}", path.display()))?;

    Ok(path)
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

fn serialise_csv(qr: &QueryResult) -> String {
    let mut out = String::new();

    // Header row
    let headers: Vec<&str> = qr.schema.iter().map(|c| c.name.as_str()).collect();
    write_csv_row(&mut out, headers.iter().copied());

    // Data rows
    for cells in crate::ui::tabs::tables::display_rows(qr) {
        write_csv_row(&mut out, cells.iter().map(String::as_str));
    }
    out
}

fn write_csv_row<'a, I>(out: &mut String, cells: I)
where
    I: IntoIterator<Item = &'a str>,
{
    let mut first = true;
    for cell in cells {
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&csv_escape(cell));
    }
    out.push_str("\r\n");
}

fn csv_escape(s: &str) -> String {
    let needs_quoting = s
        .chars()
        .any(|c| c == ',' || c == '"' || c == '\n' || c == '\r');
    if needs_quoting {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

fn serialise_json(qr: &QueryResult) -> String {
    let headers: Vec<&str> = qr.schema.iter().map(|c| c.name.as_str()).collect();
    let records: Vec<Value> = qr
        .rows
        .iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (i, h) in headers.iter().enumerate() {
                let v = row.get(i).cloned().unwrap_or(Value::Null);
                obj.insert((*h).to_string(), v);
            }
            Value::Object(obj)
        })
        .collect();

    // Pretty-print so the file is human-inspectable.
    serde_json::to_string_pretty(&json!({
        "row_count": qr.rows.len(),
        "columns": headers,
        "rows": records,
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{QueryResult, SchemaElement};
    use serde_json::json;

    fn sample_qr() -> QueryResult {
        QueryResult {
            schema: vec![
                SchemaElement {
                    name: "id".to_string(),
                    algebraic_type: json!({"U64": []}),
                },
                SchemaElement {
                    name: "name".to_string(),
                    algebraic_type: json!({"String": []}),
                },
            ],
            rows: vec![
                vec![json!(1), json!("Alice")],
                vec![json!(2), json!("Bob, Jr.")], // triggers CSV quoting
                vec![json!(3), json!("quote\"inside")],
            ],
            total_duration_micros: 0,
        }
    }

    #[test]
    fn csv_quotes_cells_with_commas_and_quotes() {
        let csv = serialise_csv(&sample_qr());
        // Header + 3 rows
        let lines: Vec<&str> = csv.split("\r\n").filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "id,name");
        assert_eq!(lines[1], "1,Alice");
        assert_eq!(lines[2], "2,\"Bob, Jr.\"");
        assert_eq!(lines[3], "3,\"quote\"\"inside\"");
    }

    #[test]
    fn json_has_row_count_and_records() {
        let j = serialise_json(&sample_qr());
        let parsed: Value = serde_json::from_str(&j).unwrap();
        assert_eq!(parsed["row_count"], json!(3));
        assert_eq!(parsed["columns"], json!(["id", "name"]));
        assert_eq!(parsed["rows"][0]["name"], json!("Alice"));
        assert_eq!(parsed["rows"][1]["name"], json!("Bob, Jr."));
    }

    #[test]
    fn csv_escape_no_quote_for_plain() {
        assert_eq!(csv_escape("hello"), "hello");
        assert_eq!(csv_escape("hello,world"), "\"hello,world\"");
        assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_escape("line\nbreak"), "\"line\nbreak\"");
    }
}
