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
         (single A1-address; supports `value` OR `formula` plus an \
         optional `format` block), `set_cells` (2D-array bulk anchored \
         at a top-left cell), `add_sheet`, `delete_sheet`. Format-\
         preserving — styles / formulas / charts in unrelated regions \
         are kept on round-trip.\n\n\
         \
         Formula: `formula: \"=SUM(A1:A10)\"` (leading `=` optional) on \
         `set_cell` writes a formula instead of a literal value. Cell \
         display value updates the next time Excel / Numbers / \
         LibreOffice opens the file.\n\n\
         \
         Format block (set_cell only for v1): `{bold, italic, \
         font_color, fill_color, number_format}`. Colors are hex \
         strings (`\"#RRGGBB\"` or `\"#AARRGGBB\"`); number_format \
         accepts Excel format codes (`\"0.00\"`, `\"$#,##0.00\"`, \
         `\"yyyy-mm-dd\"`, etc.). Existing cell style aspects not \
         named in the block are preserved."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string", "description": "Path to the .xlsx to edit (overwritten in place)."},
                "op":      {"type": "string", "enum": ["set_cell", "set_cells", "add_sheet", "delete_sheet"]},
                "sheet":   {"type": "string", "description": "Sheet name. Default: first sheet for set_cell/set_cells; required for add_sheet/delete_sheet."},
                "cell":    {"type": "string", "description": "A1-style address (set_cell only)."},
                "value":   {"description": "New cell value — string, number, or boolean (set_cell only). Ignored when `formula` is set."},
                "formula": {"type": "string", "description": "Excel formula like `=SUM(A1:A10)` (set_cell only). Leading `=` optional. Takes precedence over `value`."},
                "format":  {
                    "type": "object",
                    "description": "Optional formatting (set_cell only). All fields optional — only the named aspects are changed; existing style is preserved otherwise.",
                    "properties": {
                        "bold":          {"type": "boolean"},
                        "italic":        {"type": "boolean"},
                        "font_color":    {"type": "string", "description": "Hex `#RRGGBB` or `#AARRGGBB`."},
                        "fill_color":    {"type": "string", "description": "Background fill hex `#RRGGBB` or `#AARRGGBB`."},
                        "number_format": {"type": "string", "description": "Excel format code (`0.00`, `$#,##0.00`, `yyyy-mm-dd`, `0%`, etc.)."}
                    }
                },
                "anchor":  {"type": "string", "description": "Top-left A1 anchor for the 2D array (set_cells only). Default A1."},
                "data":    {"description": "JSON 2D-array of typed cells (set_cells only)."}
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
            "set_cell" => {
                let formula = input
                    .get("formula")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let value = input.get("value").cloned();
                if formula.is_none() && value.is_none() {
                    return Err(Error::Tool(
                        "set_cell needs either `value` or `formula`".into(),
                    ));
                }
                let format = input
                    .get("format")
                    .map(|f| parse_cell_format(f))
                    .transpose()?;
                Edit::SetCell {
                    cell: req_str(&input, "cell")?.to_string(),
                    value: value.unwrap_or(Value::Null),
                    formula,
                    format,
                }
            }
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
    SetCell {
        cell: String,
        value: Value,
        formula: Option<String>,
        format: Option<CellFormat>,
    },
    SetCells {
        anchor: String,
        data: Value,
    },
    AddSheet,
    DeleteSheet,
}

/// Parsed view of the user-supplied `format` JSON object. Each field
/// is optional — only the named aspects are applied at edit time, so
/// existing style attributes (font face, font size, borders) survive
/// when the user only wants to e.g. bold one cell.
#[derive(Debug, Default)]
struct CellFormat {
    bold: Option<bool>,
    italic: Option<bool>,
    font_color: Option<String>,
    fill_color: Option<String>,
    number_format: Option<String>,
}

/// Parse the JSON `format` block. Validates each color string up front
/// (must be `#RRGGBB` or `#AARRGGBB`) so a typo surfaces before we do
/// the expensive read+write round-trip.
fn parse_cell_format(v: &Value) -> Result<CellFormat> {
    let obj = v
        .as_object()
        .ok_or_else(|| Error::Tool("format must be an object".into()))?;
    let mut fmt = CellFormat::default();
    if let Some(b) = obj.get("bold").and_then(|x| x.as_bool()) {
        fmt.bold = Some(b);
    }
    if let Some(i) = obj.get("italic").and_then(|x| x.as_bool()) {
        fmt.italic = Some(i);
    }
    if let Some(c) = obj.get("font_color").and_then(|x| x.as_str()) {
        fmt.font_color = Some(normalize_argb(c, "font_color")?);
    }
    if let Some(c) = obj.get("fill_color").and_then(|x| x.as_str()) {
        fmt.fill_color = Some(normalize_argb(c, "fill_color")?);
    }
    if let Some(n) = obj.get("number_format").and_then(|x| x.as_str()) {
        fmt.number_format = Some(n.to_string());
    }
    Ok(fmt)
}

/// Normalise a user-supplied hex color into Excel's 8-char ARGB.
/// Accepts `#RRGGBB` (alpha defaulted to FF) and `#AARRGGBB`. Case-
/// insensitive on the hex digits.
fn normalize_argb(raw: &str, field: &str) -> Result<String> {
    let s = raw.trim();
    let hex = s.strip_prefix('#').unwrap_or(s);
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Tool(format!(
            "{field}: {raw:?} contains non-hex characters"
        )));
    }
    let upper = hex.to_ascii_uppercase();
    match upper.len() {
        6 => Ok(format!("FF{upper}")),
        8 => Ok(upper),
        n => Err(Error::Tool(format!(
            "{field}: {raw:?} has {n} hex digits; expected 6 (#RRGGBB) or 8 (#AARRGGBB)"
        ))),
    }
}

fn apply_edit(path: &std::path::Path, sheet: Option<&str>, edit: &Edit) -> Result<String> {
    let mut book = xlsx_reader::read(path)
        .map_err(|e| Error::Tool(format!("read {}: {:?}", path.display(), e)))?;

    let summary = match edit {
        Edit::SetCell {
            cell,
            value,
            formula,
            format,
        } => {
            let sheet_name = resolve_sheet_name(&book, sheet)?;
            let ws = book
                .get_sheet_by_name_mut(&sheet_name)
                .ok_or_else(|| Error::Tool(format!("sheet {sheet_name:?} not found")))?;
            let target = ws.get_cell_mut(cell.as_str());
            if let Some(f) = formula {
                // Strip leading `=` if present — umya's API takes the
                // formula body without it.
                let body = f.strip_prefix('=').unwrap_or(f);
                target.set_formula(body);
            } else {
                apply_value(target, value);
            }
            if let Some(fmt) = format {
                apply_format(target, fmt);
            }
            let what = if formula.is_some()
                || value
                    .as_str()
                    .is_some_and(|v| v.trim_start().starts_with('=') && v.trim().len() > 1)
            {
                "formula"
            } else {
                "value"
            };
            let fmt_note = if format.is_some() { " + format" } else { "" };
            format!(
                "Set {cell} {what}{fmt_note} on sheet {sheet_name:?} in {}",
                path.display()
            )
        }
        Edit::SetCells { anchor, data } => {
            let sheet_name = resolve_sheet_name(&book, sheet)?;
            let (anchor_col, anchor_row) = parse_a1(anchor)?;
            let parsed = super::xlsx_create::stringified_json_array(data);
            let rows = parsed
                .as_ref()
                .unwrap_or(data)
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

/// Apply per-cell formatting (bold / italic / colors / number format).
/// Reads the cell's existing style and mutates only the named aspects,
/// so unrelated style attributes (font face, font size, borders set by
/// upstream code) are preserved.
fn apply_format(cell: &mut umya_spreadsheet::Cell, fmt: &CellFormat) {
    let style = cell.get_style_mut();
    if let Some(b) = fmt.bold {
        style.get_font_mut().set_bold(b);
    }
    if let Some(i) = fmt.italic {
        style.get_font_mut().set_italic(i);
    }
    if let Some(argb) = &fmt.font_color {
        let mut color = umya_spreadsheet::Color::default();
        color.set_argb(argb);
        style.get_font_mut().set_color(color);
    }
    if let Some(argb) = &fmt.fill_color {
        let mut pattern = umya_spreadsheet::PatternFill::default();
        pattern.set_pattern_type(umya_spreadsheet::PatternValues::Solid);
        let mut fg = umya_spreadsheet::Color::default();
        fg.set_argb(argb);
        pattern.set_foreground_color(fg);
        let mut fill = umya_spreadsheet::Fill::default();
        fill.set_pattern_fill(pattern);
        style.set_fill(fill);
    }
    if let Some(code) = &fmt.number_format {
        let mut nf = umya_spreadsheet::NumberingFormat::default();
        nf.set_format_code(code.clone());
        style.set_numbering_format(nf);
    }
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
        // "6000000" stored as text makes `=SUM` read 0; same byte-identical
        // round-trip rule XlsxCreate applies to CSV input.
        // Models put formulas in `value` as often as in `formula`; written
        // as text, "=SUM(E2:E6)" shows the formula and computes nothing.
        Value::String(s) if s.trim_start().starts_with('=') && s.trim().len() > 1 => {
            cell.set_formula(s.trim_start().trim_start_matches('='));
        }
        Value::String(s) => match super::xlsx_create::string_to_cell(s) {
            super::xlsx_create::Cell::Number(n) => {
                cell.set_value_number(n);
            }
            _ => {
                cell.set_value_string(s.clone());
            }
        },
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

    #[test]
    fn argb_normalisation() {
        // 6-digit hex gets FF alpha prefixed.
        assert_eq!(normalize_argb("#FF0000", "x").unwrap(), "FFFF0000");
        assert_eq!(normalize_argb("ff0000", "x").unwrap(), "FFFF0000");
        // 8-digit ARGB passed through (uppercased).
        assert_eq!(normalize_argb("#80ff00ff", "x").unwrap(), "80FF00FF");
        // Bad inputs surface a clear error naming the field.
        assert!(normalize_argb("not-hex", "fill_color")
            .unwrap_err()
            .to_string()
            .contains("fill_color"));
        assert!(normalize_argb("#abc", "x")
            .unwrap_err()
            .to_string()
            .contains("3 hex digits"));
    }

    /// Formula round-trip: write `=A1+10`, verify the cell carries a
    /// formula on re-read. We can't check the computed result without
    /// a calc engine, but the formula string survival is the contract.
    #[tokio::test]
    async fn round_trip_formula_survives() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("f.xlsx");

        crate::tools::XlsxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "data": [["a", "b"], [10, 20]],
                "headers": false
            }))
            .await
            .unwrap();

        // C1 = A1 + B1 (literal value: blank rows above; just exercise
        // the formula path on a cell adjacent to existing data).
        let r = XlsxEditTool
            .call(json!({
                "path": path.to_string_lossy(),
                "op": "set_cell",
                "cell": "C1",
                "formula": "=A1+B1"
            }))
            .await
            .unwrap();
        assert!(r.contains("formula"), "msg should mention formula: {r}");

        // Re-read via umya and inspect the cell's formula attribute
        // directly. The sheet read tool flattens to text so it can't
        // round-trip the formula; this asserts the OOXML preserved it.
        let book = umya_spreadsheet::reader::xlsx::read(&path).unwrap();
        let ws = book.get_sheet(&0).unwrap();
        let cell = ws.get_cell("C1").unwrap();
        assert!(
            cell.get_formula().contains("A1+B1"),
            "C1 should carry the formula; got: {:?}",
            cell.get_formula()
        );
    }

    /// Format application: bold + fill_color + number_format land in
    /// the cell's style block. Use umya's reader to inspect the
    /// resulting Style after round-trip.
    #[tokio::test]
    async fn round_trip_format_lands_in_style() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("fmt.xlsx");

        crate::tools::XlsxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "data": [["amount"], [1234.5]]
            }))
            .await
            .unwrap();

        XlsxEditTool
            .call(json!({
                "path": path.to_string_lossy(),
                "op": "set_cell",
                "cell": "A2",
                "value": 1234.5,
                "format": {
                    "bold": true,
                    "fill_color": "#FFFF00",
                    "number_format": "$#,##0.00"
                }
            }))
            .await
            .unwrap();

        // Inspect the cell's style after re-read.
        let book = umya_spreadsheet::reader::xlsx::read(&path).unwrap();
        let ws = book.get_sheet(&0).unwrap();
        let cell = ws.get_cell("A2").unwrap();
        let style = cell.get_style();
        assert!(
            style.get_font().is_some_and(|f| *f.get_bold()),
            "A2 should be bold; style: {:?}",
            style
        );
        // The format code we set should round-trip in the style block.
        let nf_code = style
            .get_number_format()
            .map(|nf| nf.get_format_code().to_string())
            .unwrap_or_default();
        assert!(
            nf_code.contains("$") || nf_code.contains("#"),
            "number format should carry the dollar/hash code; got: {nf_code:?}"
        );
    }

    /// set_cell without value AND without formula errors clearly
    /// instead of writing an empty cell silently.
    #[tokio::test]
    async fn set_cell_requires_value_or_formula() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("e.xlsx");
        crate::tools::XlsxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "data": [["a"], [1]]
            }))
            .await
            .unwrap();
        let err = XlsxEditTool
            .call(json!({
                "path": path.to_string_lossy(),
                "op": "set_cell",
                "cell": "B1"
            }))
            .await
            .unwrap_err();
        assert!(
            format!("{err}").contains("value` or `formula"),
            "err: {err}"
        );
    }

    #[tokio::test]
    async fn numeric_strings_and_stringified_rows_are_typed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("budget.xlsx");
        crate::tools::XlsxCreateTool
            .call(json!({"path": path.to_string_lossy(), "data": [["x"]], "headers": false}))
            .await
            .unwrap();

        XlsxEditTool
            .call(json!({
                "path": path.to_string_lossy(),
                "op": "set_cells",
                "anchor": "A1",
                "data": "[[\"รายการ\",\"จำนวนเงิน (บาท)\"],[\"ค่าใบอนุญาตซอฟต์แวร์\",6000000]]"
            }))
            .await
            .unwrap();
        for (cell, value) in [("B3", json!("6000000")), ("B4", json!("00123"))] {
            XlsxEditTool
                .call(json!({
                    "path": path.to_string_lossy(),
                    "op": "set_cell",
                    "cell": cell,
                    "value": value
                }))
                .await
                .unwrap();
        }
        XlsxEditTool
            .call(json!({
                "path": path.to_string_lossy(),
                "op": "set_cell",
                "cell": "B5",
                "formula": "=SUM(B2:B3)"
            }))
            .await
            .unwrap();

        let book = umya_spreadsheet::reader::xlsx::read(&path).unwrap();
        let ws = book.get_sheet(&0).unwrap();
        assert_eq!(ws.get_cell("A2").unwrap().get_value(), "ค่าใบอนุญาตซอฟต์แวร์");
        assert_eq!(ws.get_cell("B1").unwrap().get_value(), "จำนวนเงิน (บาท)");
        let b2 = ws.get_cell("B2").unwrap();
        assert_eq!(b2.get_data_type(), "n");
        assert_eq!(b2.get_value_number(), Some(6000000.0));
        let b3 = ws.get_cell("B3").unwrap();
        assert_eq!(
            b3.get_data_type(),
            "n",
            "a numeric string must land as a number"
        );
        assert_eq!(b3.get_value_number(), Some(6000000.0));
        let b4 = ws.get_cell("B4").unwrap();
        assert_eq!(b4.get_data_type(), "s", "leading zeros stay text");
        assert!(ws
            .get_cell("B5")
            .unwrap()
            .get_formula()
            .contains("SUM(B2:B3)"));
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

    #[tokio::test]
    async fn formula_in_value_is_written_as_a_formula() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.xlsx");
        crate::tools::XlsxCreateTool
            .call(serde_json::json!({"path": path.to_string_lossy(), "data": "a,b\nx,1\ny,2"}))
            .await
            .unwrap();
        let msg = XlsxEditTool
            .call(serde_json::json!({"path": path.to_string_lossy(), "op": "set_cell", "cell": "B4", "value": "=SUM(B2:B3)"}))
            .await
            .unwrap();
        assert!(msg.contains("formula"), "{msg}");
        let book = umya_spreadsheet::reader::xlsx::read(&path).unwrap();
        let c = book.get_sheet(&0).unwrap().get_cell("B4").unwrap();
        assert_eq!(c.get_formula(), "SUM(B2:B3)");
    }
}
