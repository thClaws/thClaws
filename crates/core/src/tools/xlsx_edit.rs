//! `XlsxEdit` — in-place edit of an Excel file via `umya-spreadsheet`.
//! umya is purpose-built for round-trip format preservation: load a
//! file, mutate specific cells / sheets, write back without disturbing
//! styles, formulas, charts, conditional formatting, or data
//! validation in unrelated regions. (rust_xlsxwriter is write-only and
//! would reset the workbook on load — unsuitable for edits.)
//!
//! Operations for v1:
//!
//! - `set_cell` — update a single cell at an A1-style address. Auto-
//!   detects type the same way XlsxCreate does (numbers / booleans /
//!   strings).
//! - `set_cells` — bulk update from a JSON 2D-array, anchored at a
//!   given top-left cell (default `"A1"`).
//! - `add_sheet` — append a new sheet by name.
//! - `delete_sheet` — remove a sheet by name (errors if it's the only
//!   sheet, since workbooks must contain at least one).

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use umya_spreadsheet::{reader::xlsx as xlsx_reader, writer::xlsx as xlsx_writer};

pub struct XlsxEditTool;

#[async_trait]
impl Tool for XlsxEditTool {
    fn name(&self) -> &'static str {
        "XlsxEdit"
    }

    fn description(&self) -> &'static str {
        "Edit an Excel file (.xlsx) in place. Operations: `set_cell` \
         (single A1-address), `set_cells` (2D-array bulk anchored at \
         a top-left cell), `add_sheet`, `delete_sheet`. Format-preserving \
         — styles / formulas / charts in unrelated regions are kept on \
         round-trip."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":  {"type": "string", "description": "Path to the .xlsx to edit (overwritten in place)."},
                "op":    {"type": "string", "enum": ["set_cell", "set_cells", "add_sheet", "delete_sheet"]},
                "sheet": {"type": "string", "description": "Sheet name. Default: first sheet for set_cell/set_cells; required for add_sheet/delete_sheet."},
                "cell":  {"type": "string", "description": "A1-style address (set_cell only)."},
                "value": {"description": "New cell value — string, number, or boolean (set_cell only)."},
                "anchor":{"type": "string", "description": "Top-left A1 anchor for the 2D array (set_cells only). Default A1."},
                "data":  {"description": "JSON 2D-array of typed cells (set_cells only)."}
            },
            "required": ["path", "op"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let validated = crate::sandbox::Sandbox::check_write(raw_path)?;
        let op = req_str(&input, "op")?.to_string();
        let sheet_name = input
            .get("sheet")
            .and_then(|v| v.as_str())
            .map(String::from);

        // Decode op-specific args eagerly so JSON errors surface before
        // we do the expensive read+write round-trip.
        let edit = match op.as_str() {
            "set_cell" => Edit::SetCell {
                cell: req_str(&input, "cell")?.to_string(),
                value: input
                    .get("value")
                    .cloned()
                    .ok_or_else(|| Error::Tool("missing field: value".into()))?,
            },
            "set_cells" => {
                let data = input
                    .get("data")
                    .cloned()
                    .ok_or_else(|| Error::Tool("missing field: data".into()))?;
                Edit::SetCells {
                    anchor: input
                        .get("anchor")
                        .and_then(|v| v.as_str())
                        .unwrap_or("A1")
                        .to_string(),
                    data,
                }
            }
            "add_sheet" => Edit::AddSheet,
            "delete_sheet" => Edit::DeleteSheet,
            other => {
                return Err(Error::Tool(format!(
                    "unknown op {other:?}; expected set_cell / set_cells / add_sheet / delete_sheet"
                )))
            }
        };

        let path_clone = validated.clone();
        tokio::task::spawn_blocking(move || apply_edit(&path_clone, sheet_name.as_deref(), &edit))
            .await
            .map_err(|e| Error::Tool(format!("XLSX edit worker: {e}")))?
    }
}

enum Edit {
    SetCell { cell: String, value: Value },
    SetCells { anchor: String, data: Value },
    AddSheet,
    DeleteSheet,
}

fn apply_edit(path: &std::path::Path, sheet: Option<&str>, edit: &Edit) -> Result<String> {
    let mut book = xlsx_reader::read(path)
        .map_err(|e| Error::Tool(format!("read {}: {:?}", path.display(), e)))?;

    let summary = match edit {
        Edit::SetCell { cell, value } => {
            let sheet_name = resolve_sheet_name(&book, sheet)?;
            let ws = book
                .get_sheet_by_name_mut(&sheet_name)
                .ok_or_else(|| Error::Tool(format!("sheet {sheet_name:?} not found")))?;
            apply_value(ws.get_cell_mut(cell.as_str()), value);
            format!("Set {cell} on sheet {sheet_name:?} in {}", path.display())
        }
        Edit::SetCells { anchor, data } => {
            let sheet_name = resolve_sheet_name(&book, sheet)?;
            let (anchor_col, anchor_row) = parse_a1(anchor)?;
            let rows = data
                .as_array()
                .ok_or_else(|| Error::Tool("data must be a JSON 2D array".into()))?;
            let ws = book
                .get_sheet_by_name_mut(&sheet_name)
                .ok_or_else(|| Error::Tool(format!("sheet {sheet_name:?} not found")))?;
            let mut total = 0usize;
            for (ri, row) in rows.iter().enumerate() {
                let row_arr = row
                    .as_array()
                    .ok_or_else(|| Error::Tool(format!("data row {ri} is not an array")))?;
                for (ci, val) in row_arr.iter().enumerate() {
                    let col = anchor_col + ci as u32;
                    let row_n = anchor_row + ri as u32;
                    apply_value(ws.get_cell_mut((col, row_n)), val);
                    total += 1;
                }
            }
            format!(
                "Set {total} cell(s) anchored at {anchor} on sheet {sheet_name:?} in {}",
                path.display()
            )
        }
        Edit::AddSheet => {
            let name = sheet.ok_or_else(|| {
                Error::Tool("add_sheet requires `sheet` argument with the new name".into())
            })?;
            book.new_sheet(name)
                .map_err(|e| Error::Tool(format!("new_sheet {name:?}: {e}")))?;
            format!("Added sheet {name:?} to {}", path.display())
        }
        Edit::DeleteSheet => {
            let name = sheet
                .ok_or_else(|| Error::Tool("delete_sheet requires `sheet` argument".into()))?;
            if book.get_sheet_count() <= 1 {
                return Err(Error::Tool(
                    "cannot delete the only sheet — workbooks must contain at least one".into(),
                ));
            }
            book.remove_sheet_by_name(name)
                .map_err(|e| Error::Tool(format!("remove_sheet {name:?}: {e}")))?;
            format!("Deleted sheet {name:?} from {}", path.display())
        }
    };

    xlsx_writer::write(&book, path)
        .map_err(|e| Error::Tool(format!("write {}: {:?}", path.display(), e)))?;

    Ok(summary)
}

fn resolve_sheet_name(
    book: &umya_spreadsheet::Spreadsheet,
    explicit: Option<&str>,
) -> Result<String> {
    if let Some(name) = explicit {
        if book.get_sheet_by_name(name).is_some() {
            return Ok(name.to_string());
        }
        return Err(Error::Tool(format!("sheet {name:?} not found")));
    }
    // Default: first sheet by index 0.
    book.get_sheet(&0)
        .map(|ws| ws.get_name().to_string())
        .ok_or_else(|| Error::Tool("workbook has no sheets".into()))
}

fn apply_value(cell: &mut umya_spreadsheet::Cell, value: &Value) {
    match value {
        Value::Null => {
            cell.set_value("");
        }
        Value::Bool(b) => {
            cell.set_value_bool(*b);
        }
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                cell.set_value_number(f);
            }
        }
        Value::String(s) => {
            cell.set_value_string(s.clone());
        }
        other => {
            // Arrays / objects fall back to a JSON-stringified value so
            // we never lose the data; user can normalize upstream if they
            // want a typed cell.
            cell.set_value_string(other.to_string());
        }
    }
}

/// Parse an A1-style address like `"B7"` into (col, row) where col + row
/// are 1-indexed (matching umya-spreadsheet's `(u32, u32)` cell access).
fn parse_a1(s: &str) -> Result<(u32, u32)> {
    let bytes = s.as_bytes();
    let mut split = 0;
    while split < bytes.len() && bytes[split].is_ascii_alphabetic() {
        split += 1;
    }
    if split == 0 || split == bytes.len() {
        return Err(Error::Tool(format!("invalid A1 address: {s:?}")));
    }
    let col_str = &s[..split];
    let row_str = &s[split..];

    let mut col: u32 = 0;
    for c in col_str.chars() {
        col = col * 26 + (c.to_ascii_uppercase() as u32 - 'A' as u32 + 1);
    }
    let row: u32 = row_str
        .parse()
        .map_err(|_| Error::Tool(format!("invalid row in {s:?}")))?;
    if row == 0 {
        return Err(Error::Tool(format!("row must be 1-indexed in {s:?}")));
    }
    Ok((col, row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn a1_parses_simple_addresses() {
        assert_eq!(parse_a1("A1").unwrap(), (1, 1));
        assert_eq!(parse_a1("B7").unwrap(), (2, 7));
        assert_eq!(parse_a1("Z1").unwrap(), (26, 1));
        assert_eq!(parse_a1("AA1").unwrap(), (27, 1));
        assert!(parse_a1("1A").is_err());
        assert!(parse_a1("A0").is_err());
    }

    #[tokio::test]
    async fn round_trip_create_edit_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rt.xlsx");

        crate::tools::XlsxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "data": "name,age,score\nAlice,30,95\nBob,25,87"
            }))
            .await
            .unwrap();

        // Edit B2 (Alice's age cell) — A1 is "name", A2 is "Alice", B2 is 30.
        let r = XlsxEditTool
            .call(json!({
                "path": path.to_string_lossy(),
                "op": "set_cell",
                "cell": "B2",
                "value": 31
            }))
            .await
            .unwrap();
        assert!(r.contains("Set B2"));

        // Add a new sheet with Thai name + populate.
        XlsxEditTool
            .call(json!({
                "path": path.to_string_lossy(),
                "op": "add_sheet",
                "sheet": "ภาษาไทย"
            }))
            .await
            .unwrap();
        XlsxEditTool
            .call(json!({
                "path": path.to_string_lossy(),
                "op": "set_cells",
                "sheet": "ภาษาไทย",
                "anchor": "A1",
                "data": [["ชื่อ", "อายุ"], ["สมชาย", 25]]
            }))
            .await
            .unwrap();

        // Read it back and verify the edits landed.
        let csv = crate::tools::XlsxReadTool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();
        assert!(csv.contains("Alice,31"), "edited age missing: {csv:?}");

        let thai_csv = crate::tools::XlsxReadTool
            .call(json!({
                "path": path.to_string_lossy(),
                "sheet": "ภาษาไทย"
            }))
            .await
            .unwrap();
        assert!(
            thai_csv.contains("สมชาย"),
            "Thai cell missing: {thai_csv:?}"
        );
        assert!(thai_csv.contains("ชื่อ"), "Thai header missing: {thai_csv:?}");
    }
}
