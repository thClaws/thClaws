//! `XlsxCreate` — render tabular data to an Excel file via
//! `rust_xlsxwriter`. Two input shapes are accepted:
//!
//! 1. CSV string (single sheet, first row may be headers)
//! 2. JSON 2D array — `[[..row1..], [..row2..]]` — preserves cell types
//!    when values are typed (numbers stay numbers, bools stay bools).
//!
//! Cell-type detection for CSV cells: parse each value as f64; on
//! success, write as number, else as string. Booleans (`true`/`false`)
//! are written as Excel booleans.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use rust_xlsxwriter::{Format, FormatBorder, Workbook, Worksheet};
use serde_json::{json, Value};
use std::path::Path;

pub struct XlsxCreateTool;

#[async_trait]
impl Tool for XlsxCreateTool {
    fn name(&self) -> &'static str {
        "XlsxCreate"
    }

    fn description(&self) -> &'static str {
        "Render tabular data to an Excel (.xlsx) file. `data` accepts \
         either a CSV string (first row is headers when `headers: true`, \
         the default) or a JSON 2D array of typed cells (numbers stay \
         numbers, booleans stay booleans). Single sheet for v1; \
         multi-sheet workbooks land in a follow-up. Numbers in CSV cells \
         are auto-detected and written as numeric cells."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":       {"type": "string", "description": "Output .xlsx path. Parent directories are created if missing."},
                "data":       {"description": "CSV string or JSON 2D array of cells."},
                "sheet_name": {"type": "string", "description": "Sheet name. Default \"Sheet1\". Max 31 chars (Excel limit)."},
                "headers":    {"type": "boolean", "description": "Treat the first row as headers (bold + bottom border). Default true."},
                "auto_width": {"type": "boolean", "description": "Auto-size columns to fit content. Default true."}
            },
            "required": ["path", "data"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let validated = crate::sandbox::Sandbox::check_write(raw_path)?;

        let data = input
            .get("data")
            .ok_or_else(|| Error::Tool("missing field: data".into()))?
            .clone();

        let sheet_name = input
            .get("sheet_name")
            .and_then(|v| v.as_str())
            .unwrap_or("Sheet1")
            .to_string();
        let with_headers = input
            .get("headers")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let auto_width = input
            .get("auto_width")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let rows = parse_data(&data)?;

        if let Some(parent) = Path::new(&*validated.to_string_lossy()).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::Tool(format!("mkdir {}: {}", parent.display(), e)))?;
            }
        }

        let path_clone = validated.clone();

        let (rows_written, cols_written) =
            tokio::task::spawn_blocking(move || -> Result<(usize, usize)> {
                render_xlsx(&path_clone, &rows, &sheet_name, with_headers, auto_width)
            })
            .await
            .map_err(|e| Error::Tool(format!("XLSX worker join failed: {e}")))??;

        Ok(format!(
            "Wrote XLSX to {} ({} rows × {} cols)",
            validated.display(),
            rows_written,
            cols_written
        ))
    }
}

/// In-memory cell representation. We keep this typed instead of just
/// `Vec<Vec<String>>` so JSON 2D-array input preserves number/bool types
/// without lossy stringification.
#[derive(Debug, Clone)]
enum Cell {
    Empty,
    Text(String),
    Number(f64),
    Bool(bool),
}

fn parse_data(data: &Value) -> Result<Vec<Vec<Cell>>> {
    if let Some(arr) = data.as_array() {
        // JSON 2D array: each inner element is a row.
        let mut rows = Vec::with_capacity(arr.len());
        for (ridx, row) in arr.iter().enumerate() {
            let cells = row
                .as_array()
                .ok_or_else(|| Error::Tool(format!("row {ridx} is not an array")))?;
            rows.push(cells.iter().map(value_to_cell).collect());
        }
        Ok(rows)
    } else if let Some(s) = data.as_str() {
        // CSV string.
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(false) // we'll handle the header bolding ourselves
            .flexible(true)
            .from_reader(s.as_bytes());
        let mut rows = Vec::new();
        for rec in rdr.records() {
            let rec = rec.map_err(|e| Error::Tool(format!("CSV parse: {e}")))?;
            rows.push(rec.iter().map(string_to_cell).collect());
        }
        Ok(rows)
    } else {
        Err(Error::Tool(
            "data must be a CSV string or a JSON 2D array".into(),
        ))
    }
}

fn value_to_cell(v: &Value) -> Cell {
    match v {
        Value::Null => Cell::Empty,
        Value::Bool(b) => Cell::Bool(*b),
        Value::Number(n) => n
            .as_f64()
            .map(Cell::Number)
            .unwrap_or(Cell::Text(n.to_string())),
        Value::String(s) => string_to_cell(s),
        other => Cell::Text(other.to_string()),
    }
}

/// Per-cell type inference for CSV strings. Try number first, then
/// boolean (case-insensitive), else fall through to text.
fn string_to_cell(s: &str) -> Cell {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Cell::Empty;
    }
    if let Ok(n) = trimmed.parse::<f64>() {
        return Cell::Number(n);
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "true" => Cell::Bool(true),
        "false" => Cell::Bool(false),
        _ => Cell::Text(s.to_string()),
    }
}

fn render_xlsx(
    path: &Path,
    rows: &[Vec<Cell>],
    sheet_name: &str,
    with_headers: bool,
    auto_width: bool,
) -> Result<(usize, usize)> {
    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();
    worksheet
        .set_name(sheet_name)
        .map_err(|e| Error::Tool(format!("set sheet name: {e}")))?;

    let header_format = Format::new()
        .set_bold()
        .set_border_bottom(FormatBorder::Thin);

    let mut max_cols = 0usize;
    for (r, row) in rows.iter().enumerate() {
        max_cols = max_cols.max(row.len());
        for (c, cell) in row.iter().enumerate() {
            let r32 = u32::try_from(r).map_err(|_| Error::Tool("row index overflow".into()))?;
            let c16 = u16::try_from(c).map_err(|_| Error::Tool("col index overflow".into()))?;
            let is_header = with_headers && r == 0;
            write_cell(
                worksheet,
                r32,
                c16,
                cell,
                is_header.then_some(&header_format),
            )?;
        }
    }

    if auto_width {
        worksheet.autofit();
    }
    if with_headers && !rows.is_empty() {
        // Freeze the top row so headers stay visible during scroll —
        // standard expectation for tabular data.
        let _ = worksheet.set_freeze_panes(1, 0);
    }

    workbook
        .save(path)
        .map_err(|e| Error::Tool(format!("save XLSX: {e}")))?;

    Ok((rows.len(), max_cols))
}

fn write_cell(
    ws: &mut Worksheet,
    row: u32,
    col: u16,
    cell: &Cell,
    fmt: Option<&Format>,
) -> Result<()> {
    let result = match (cell, fmt) {
        (Cell::Empty, _) => return Ok(()),
        (Cell::Text(s), Some(f)) => ws.write_string_with_format(row, col, s, f).map(|_| ()),
        (Cell::Text(s), None) => ws.write_string(row, col, s).map(|_| ()),
        (Cell::Number(n), Some(f)) => ws.write_number_with_format(row, col, *n, f).map(|_| ()),
        (Cell::Number(n), None) => ws.write_number(row, col, *n).map(|_| ()),
        (Cell::Bool(b), Some(f)) => ws.write_boolean_with_format(row, col, *b, f).map(|_| ()),
        (Cell::Bool(b), None) => ws.write_boolean(row, col, *b).map(|_| ()),
    };
    result.map_err(|e| Error::Tool(format!("write cell ({row},{col}): {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn writes_xlsx_from_csv() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data.xlsx");
        let msg = XlsxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "data": "name,age,active\nAlice,30,true\nBob,25,false\nสมชาย,40,true"
            }))
            .await
            .unwrap();
        assert!(msg.contains("Wrote XLSX to"));
        assert!(msg.contains("4 rows"));
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            bytes.starts_with(b"PK"),
            "output should be a ZIP/OOXML file"
        );
    }

    #[tokio::test]
    async fn writes_xlsx_from_json_array() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("typed.xlsx");
        let _ = XlsxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "data": [
                    ["name", "score"],
                    ["Alice", 95.5],
                    ["Bob", 87.2]
                ],
                "headers": true
            }))
            .await
            .unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 1000);
    }

    #[test]
    fn cell_inference() {
        assert!(matches!(string_to_cell("42"), Cell::Number(_)));
        assert!(matches!(string_to_cell("3.14"), Cell::Number(_)));
        assert!(matches!(string_to_cell("hello"), Cell::Text(_)));
        assert!(matches!(string_to_cell("True"), Cell::Bool(true)));
        assert!(matches!(string_to_cell("false"), Cell::Bool(false)));
        assert!(matches!(string_to_cell(""), Cell::Empty));
    }
}
