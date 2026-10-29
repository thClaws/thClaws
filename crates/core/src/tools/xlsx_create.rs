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
         three input shapes: (1) a CSV string — single sheet, first row \
         is headers when `headers: true` (default); (2) a JSON 2D array \
         of typed cells — single sheet, types preserved (numbers stay \
         numbers, booleans stay booleans); (3) a JSON array of \
         `{sheet: \"Name\", rows: [[...]]}` or `{sheet: \"Name\", data: \
         \"csv string\"}` objects — multi-sheet workbook with one tab \
         per object. Numbers in CSV cells are auto-detected and written \
         as numeric cells. Sheet names must be ≤31 chars (Excel limit) \
         and unique within the workbook."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":       {"type": "string", "description": "Output .xlsx path. Parent directories are created if missing."},
                "data":       {"description": "Single sheet: CSV string OR JSON 2D array. Multi-sheet: JSON array of {sheet, rows} or {sheet, data} objects."},
                "sheet_name": {"type": "string", "description": "Sheet name (single-sheet inputs only — ignored for the multi-sheet shape, which carries names per object). Default \"Sheet1\". Max 31 chars."},
                "headers":    {"type": "boolean", "description": "Treat the first row as headers (bold + bottom border). Applied to every sheet for the multi-sheet shape. Default true."},
                "auto_width": {"type": "boolean", "description": "Auto-size columns to fit content. Applied to every sheet for the multi-sheet shape. Default true."}
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

        let default_sheet_name = input
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

        let sheets = parse_sheets(&data, &default_sheet_name)?;

        if let Some(parent) = Path::new(&*validated.to_string_lossy()).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::Tool(format!("mkdir {}: {}", parent.display(), e)))?;
            }
        }

        let path_clone = validated.clone();
        let summary = tokio::task::spawn_blocking(move || -> Result<String> {
            render_xlsx(&path_clone, &sheets, with_headers, auto_width)
        })
        .await
        .map_err(|e| Error::Tool(format!("XLSX worker join failed: {e}")))??;

        Ok(format!(
            "Wrote XLSX to {} — {}",
            validated.display(),
            summary
        ))
    }
}

/// One sheet's worth of data — name plus typed rows. Multi-sheet input
/// shapes parse to a `Vec<Sheet>`; the legacy single-sheet shapes
/// (CSV string, 2D array) parse to a 1-element vec carrying the
/// caller's `sheet_name` argument.
struct Sheet {
    name: String,
    rows: Vec<Vec<Cell>>,
}

/// In-memory cell representation. We keep this typed instead of just
/// `Vec<Vec<String>>` so JSON 2D-array input preserves number/bool types
/// without lossy stringification.
#[derive(Debug, Clone)]
pub(crate) enum Cell {
    Empty,
    Text(String),
    Number(f64),
    Bool(bool),
}

/// Detect the input shape and return one or more sheets. Three shapes:
/// 1. CSV string → single sheet with `default_name`.
/// 2. JSON 2D array (each inner element is itself an array of cells)
///    → single sheet with `default_name`.
/// 3. JSON array of objects with at least a `sheet` key plus either
///    `rows` (2D array) or `data` (CSV string) → multi-sheet workbook.
///
/// The shape is decided by inspecting the FIRST element of the array:
/// if it's an object, treat the whole array as the multi-sheet shape;
/// otherwise treat it as a 2D array. A mixed array of objects + raw
/// row arrays is rejected (ambiguous).
fn parse_sheets(data: &Value, default_name: &str) -> Result<Vec<Sheet>> {
    if let Some(parsed) = stringified_json_array(data) {
        return parse_sheets(&parsed, default_name);
    }
    if let Some(s) = data.as_str() {
        // Shape 1: CSV string.
        return Ok(vec![Sheet {
            name: default_name.to_string(),
            rows: parse_csv_rows(s)?,
        }]);
    }
    let arr = match data.as_array() {
        Some(a) => a,
        None => {
            return Err(Error::Tool(
                "data must be a CSV string, a JSON 2D array, or a JSON array of \
                 {sheet, rows} / {sheet, data} objects"
                    .into(),
            ))
        }
    };
    if arr.is_empty() {
        return Ok(vec![Sheet {
            name: default_name.to_string(),
            rows: Vec::new(),
        }]);
    }
    // Discriminate by first element shape.
    if arr[0].is_object() {
        // Shape 3: multi-sheet array.
        let mut sheets = Vec::with_capacity(arr.len());
        let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (idx, entry) in arr.iter().enumerate() {
            let obj = entry.as_object().ok_or_else(|| {
                Error::Tool(format!(
                    "data[{idx}] must be a {{sheet, rows}} or {{sheet, data}} object \
                     (mixed array elements not allowed)"
                ))
            })?;
            let name = obj
                .get("sheet")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::Tool(format!("data[{idx}] missing required `sheet` key")))?
                .to_string();
            if name.chars().count() > 31 {
                return Err(Error::Tool(format!(
                    "data[{idx}] sheet name is {} chars (Excel limit is 31): {name:?}",
                    name.chars().count()
                )));
            }
            if !seen_names.insert(name.clone()) {
                return Err(Error::Tool(format!(
                    "data[{idx}] duplicate sheet name {name:?} — every sheet must have a unique name"
                )));
            }
            // Accept either `rows` (2D array) or `data` (CSV string).
            let rows = match (obj.get("rows"), obj.get("data")) {
                (Some(rows_val), _) => parse_2d_array(rows_val, idx)?,
                (None, Some(data_val)) => match data_val.as_str() {
                    Some(s) => parse_csv_rows(s)?,
                    None => {
                        return Err(Error::Tool(format!(
                            "data[{idx}].data must be a CSV string"
                        )))
                    }
                },
                (None, None) => {
                    return Err(Error::Tool(format!(
                        "data[{idx}] needs either `rows` (2D array) or `data` (CSV string)"
                    )))
                }
            };
            sheets.push(Sheet { name, rows });
        }
        Ok(sheets)
    } else {
        // Shape 2: legacy 2D array.
        Ok(vec![Sheet {
            name: default_name.to_string(),
            rows: parse_2d_array(data, 0)?,
        }])
    }
}

/// Some models (qwen) send a nested array argument as a JSON string:
/// `"data": "[[\"a\",1]]"`. Taken as the array it spells when it parses as
/// one; a CSV whose first cell happens to start with `[` fails to parse and
/// stays CSV.
pub(crate) fn stringified_json_array(v: &Value) -> Option<Value> {
    let s = v.as_str()?.trim();
    if !s.starts_with('[') {
        return None;
    }
    serde_json::from_str::<Value>(s)
        .ok()
        .filter(Value::is_array)
}

/// Parse a CSV body into typed rows. Same posture as the prior single-
/// sheet path — flexible (uneven row widths allowed), no header
/// special-casing here (header bolding is applied at render time).
fn parse_csv_rows(s: &str) -> Result<Vec<Vec<Cell>>> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(s.as_bytes());
    let mut rows = Vec::new();
    for rec in rdr.records() {
        let rec = rec.map_err(|e| Error::Tool(format!("CSV parse: {e}")))?;
        rows.push(rec.iter().map(string_to_cell).collect());
    }
    Ok(rows)
}

/// Parse a JSON 2D array (each inner element is a row of cells).
/// `sheet_idx` is used purely for the error message context.
fn parse_2d_array(data: &Value, sheet_idx: usize) -> Result<Vec<Vec<Cell>>> {
    if let Some(parsed) = stringified_json_array(data) {
        return parse_2d_array(&parsed, sheet_idx);
    }
    let arr = data.as_array().ok_or_else(|| {
        Error::Tool(format!(
            "data[{sheet_idx}] rows must be a 2D array (got {})",
            value_kind(data)
        ))
    })?;
    let mut rows = Vec::with_capacity(arr.len());
    for (ridx, row) in arr.iter().enumerate() {
        let cells = row
            .as_array()
            .ok_or_else(|| Error::Tool(format!("data[{sheet_idx}] row {ridx} is not an array")))?;
        rows.push(cells.iter().map(value_to_cell).collect());
    }
    Ok(rows)
}

fn value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
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
///
/// M6.23 BUG XT1: only coerce to Number when the f64 round-trip is
/// byte-identical to the input. Pre-fix `parse::<f64>().is_ok()` was
/// the sole gate, which silently corrupted:
///   - Leading-zero IDs ("00123" → 123, lost zeros)
///   - Phone-like strings ("+15551234567" → 15551234567, lost +)
///   - Trailing-zero decimals ("3.14000" → 3.14, lost precision shown)
///   - Scientific notation ("1e10" → 10000000000, lost notation)
/// The byte-identical check catches all these by comparing the f64's
/// canonical Display form against the trimmed input. Side effect:
/// "5.0" stays as Text (round-trips as "5"), which is intentional —
/// users who want "5.0" to render numeric should use the JSON
/// 2D-array path with `5.0` as a typed value.
pub(crate) fn string_to_cell(s: &str) -> Cell {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Cell::Empty;
    }
    if let Ok(n) = trimmed.parse::<f64>() {
        // Byte-identical round-trip check
        if format!("{n}") == trimmed {
            return Cell::Number(n);
        }
        // Preserve original string representation
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "true" => Cell::Bool(true),
        "false" => Cell::Bool(false),
        _ => Cell::Text(s.to_string()),
    }
}

fn render_xlsx(
    path: &Path,
    sheets: &[Sheet],
    with_headers: bool,
    auto_width: bool,
) -> Result<String> {
    let mut workbook = Workbook::new();

    let header_format = Format::new()
        .set_bold()
        .set_border_bottom(FormatBorder::Thin);

    let mut summary_parts: Vec<String> = Vec::with_capacity(sheets.len());

    for sheet in sheets {
        let worksheet = workbook.add_worksheet();
        worksheet
            .set_name(&sheet.name)
            .map_err(|e| Error::Tool(format!("set sheet name {:?}: {e}", sheet.name)))?;

        let mut max_cols = 0usize;
        for (r, row) in sheet.rows.iter().enumerate() {
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
        if with_headers && !sheet.rows.is_empty() {
            // Freeze the top row so headers stay visible during
            // scroll — standard expectation for tabular data.
            let _ = worksheet.set_freeze_panes(1, 0);
        }

        summary_parts.push(format!(
            "{:?} ({} rows × {} cols)",
            sheet.name,
            sheet.rows.len(),
            max_cols
        ));
    }

    workbook
        .save(path)
        .map_err(|e| Error::Tool(format!("save XLSX: {e}")))?;

    if sheets.len() == 1 {
        // Preserve the legacy "(N rows × M cols)" message shape so
        // existing user-facing call sites that grep it still work.
        Ok(summary_parts.into_iter().next().unwrap_or_default())
    } else {
        Ok(format!(
            "{} sheets — {}",
            sheets.len(),
            summary_parts.join("; ")
        ))
    }
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

    /// qwen stringifies nested arguments (session sess-18e06d2c16d0d9c0):
    /// the whole table landed on one row as CSV.
    #[test]
    fn stringified_2d_array_is_parsed_as_rows() {
        let data = json!("[[\"รายการ\",\"จำนวนเงิน (บาท)\"],[\"ค่าใบอนุญาตซอฟต์แวร์\",6000000]]");
        let sheets = parse_sheets(&data, "Sheet1").unwrap();
        assert_eq!(sheets.len(), 1);
        let rows = &sheets[0].rows;
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(matches!(&rows[0][1], Cell::Text(t) if t == "จำนวนเงิน (บาท)"));
        assert!(matches!(&rows[1][0], Cell::Text(t) if t == "ค่าใบอนุญาตซอฟต์แวร์"));
        assert!(matches!(rows[1][1], Cell::Number(n) if n == 6000000.0));
    }

    #[test]
    fn csv_starting_with_a_bracket_stays_csv() {
        let data = json!("[draft] item,amount\nชา,40");
        let rows = &parse_sheets(&data, "Sheet1").unwrap()[0].rows;
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0][0], Cell::Text(t) if t == "[draft] item"));
        assert!(matches!(rows[1][1], Cell::Number(n) if n == 40.0));
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

    #[tokio::test]
    async fn writes_multi_sheet_workbook() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("multi.xlsx");
        let msg = XlsxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "data": [
                    {"sheet": "Summary", "rows": [["Total"], [100]]},
                    {"sheet": "Detail", "rows": [["Name", "Amount"], ["Alice", 50], ["Bob", 50]]}
                ]
            }))
            .await
            .unwrap();
        assert!(msg.contains("2 sheets"), "msg: {msg}");
        assert!(msg.contains("Summary"), "msg: {msg}");
        assert!(msg.contains("Detail"), "msg: {msg}");
        // File exists + is non-trivially sized.
        assert!(std::fs::metadata(&path).unwrap().len() > 1500);
    }

    #[tokio::test]
    async fn multi_sheet_accepts_csv_per_sheet() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mixed.xlsx");
        let _ = XlsxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "data": [
                    {"sheet": "Q1", "data": "name,amount\nAlice,100\nBob,200"},
                    {"sheet": "Q2", "rows": [["name", "amount"], ["Alice", 150], ["Bob", 250]]}
                ]
            }))
            .await
            .unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 1500);
    }

    #[tokio::test]
    async fn multi_sheet_rejects_duplicate_names() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("dup.xlsx");
        let err = XlsxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "data": [
                    {"sheet": "Foo", "rows": [["a"]]},
                    {"sheet": "Foo", "rows": [["b"]]}
                ]
            }))
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("duplicate sheet name"),
            "err: {err}"
        );
    }

    #[tokio::test]
    async fn multi_sheet_rejects_too_long_name() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("long.xlsx");
        let too_long = "a".repeat(32);
        let err = XlsxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "data": [
                    {"sheet": too_long, "rows": [["a"]]}
                ]
            }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("31"), "err: {err}");
    }

    /// Backward compat: the existing single-sheet 2D-array shape still
    /// works untouched. The shape detector branches on the first
    /// element of the array — a row-array → legacy path; an object →
    /// new multi-sheet path.
    #[tokio::test]
    async fn single_sheet_2d_array_still_works() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.xlsx");
        let _ = XlsxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "data": [
                    ["name", "score"],
                    ["Alice", 95.5]
                ],
                "sheet_name": "Custom"
            }))
            .await
            .unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > 1000);
    }

    /// M6.23 BUG XT1: byte-identical round-trip for number coercion.
    /// Pre-fix, any string that parsed as f64 became a Number — losing
    /// leading zeros / + signs / scientific notation / trailing zeros.
    /// New behavior: keep as Text unless the f64 round-trips identically.
    #[test]
    fn cell_inference_preserves_lossy_strings_as_text() {
        // Leading-zero IDs — must NOT be coerced (would lose leading zeros)
        assert!(matches!(string_to_cell("00123"), Cell::Text(_)));
        assert!(matches!(string_to_cell("007"), Cell::Text(_)));

        // Phone-like with leading + — must NOT be coerced
        assert!(matches!(string_to_cell("+15551234567"), Cell::Text(_)));

        // Scientific notation — Display formats as plain decimal
        assert!(matches!(string_to_cell("1e10"), Cell::Text(_)));
        assert!(matches!(string_to_cell("1.5e3"), Cell::Text(_)));

        // Trailing zeros after decimal — Display strips them
        assert!(matches!(string_to_cell("3.14000"), Cell::Text(_)));
        // Note: "5.0" round-trips as "5" so it's now Text. Documented
        // regression — JSON 2D-array path with `5.0` typed value
        // preserves the float intent.
        assert!(matches!(string_to_cell("5.0"), Cell::Text(_)));

        // Zero-prefix decimal IS round-trip safe
        assert!(matches!(string_to_cell("0.5"), Cell::Number(_)));
        // Negative integers round-trip
        assert!(matches!(string_to_cell("-42"), Cell::Number(_)));
        // Small decimals round-trip
        assert!(matches!(string_to_cell("3.14"), Cell::Number(_)));
        // Plain integers still work
        assert!(matches!(string_to_cell("42"), Cell::Number(_)));
        assert!(matches!(string_to_cell("0"), Cell::Number(_)));
    }
}
