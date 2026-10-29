//! `XlsxRead` — extract a worksheet from an Excel file via `calamine`.
//! `format` controls the output shape: `"csv"` (default) returns a flat
//! CSV string; `"json"` returns a JSON 2D array of typed cells (numbers
//! stay numbers, booleans stay booleans, dates serialize as ISO strings).
//! `sheet` selects a sheet by name; default is the first sheet.
//!
//! Supports XLSX/XLSM/XLSB/XLS/ODS — calamine sniffs by extension.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use calamine::{open_workbook_auto, Data, Reader};
use serde_json::{json, Value};

pub struct XlsxReadTool;

#[async_trait]
impl Tool for XlsxReadTool {
    fn name(&self) -> &'static str {
        "XlsxRead"
    }

    fn description(&self) -> &'static str {
        "Extract a worksheet from an Excel file (.xlsx/.xlsm/.xlsb/.xls/.ods). \
         By default returns the first sheet as CSV. Pass `sheet` to pick \
         a different sheet by name, and `format: \"json\"` to get a typed \
         2D-array JSON payload (numbers/booleans preserved) instead of CSV. \
         The first line names the sheet, all sheet names, the used A1 range \
         and the next empty row — use those for XlsxEdit cell addresses."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":   {"type": "string", "description": "Path to the spreadsheet file."},
                "sheet":  {"type": "string", "description": "Sheet name. Default: first sheet."},
                "format": {"type": "string", "enum": ["csv", "json"], "description": "Output format. Default csv."}
            },
            "required": ["path"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_path = req_str(&input, "path")?;
        let validated = crate::sandbox::Sandbox::check(raw_path)?;
        let sheet_arg = input
            .get("sheet")
            .and_then(|v| v.as_str())
            .map(String::from);
        let format = input
            .get("format")
            .and_then(|v| v.as_str())
            .unwrap_or("csv")
            .to_string();

        let path_clone = validated.clone();
        tokio::task::spawn_blocking(move || -> Result<String> {
            let head = sheet_header(&path_clone, sheet_arg.as_deref())?;
            Ok(format!(
                "{head}\n{}",
                extract_xlsx(&path_clone, sheet_arg, &format)?
            ))
        })
        .await
        .map_err(|e| Error::Tool(format!("XLSX worker join failed: {e}")))?
    }
}

pub(crate) fn extract_xlsx(
    path: &std::path::Path,
    sheet: Option<String>,
    format: &str,
) -> Result<String> {
    let mut workbook = open_workbook_auto(path)
        .map_err(|e| Error::Tool(format!("open {}: {}", path.display(), e)))?;

    let names = workbook.sheet_names();
    let sheet_name = match sheet {
        Some(name) => {
            if !names.iter().any(|n| n == &name) {
                return Err(Error::Tool(format!(
                    "sheet {name:?} not found. Available: {names:?}"
                )));
            }
            name
        }
        None => names
            .first()
            .cloned()
            .ok_or_else(|| Error::Tool("workbook has no sheets".into()))?,
    };

    let mut range = workbook
        .worksheet_range(&sheet_name)
        .map_err(|e| Error::Tool(format!("read sheet {sheet_name:?}: {e}")))?;
    // A formula written without a cached value reads back empty; show the
    // formula there so a total row is visible rather than blank.
    if let Ok(formulas) = workbook.worksheet_formula(&sheet_name) {
        let (fr, fc) = formulas.start().unwrap_or((0, 0));
        for (r, c, f) in formulas.cells() {
            if f.is_empty() {
                continue;
            }
            let pos = (fr + r as u32, fc + c as u32);
            let blank = match range.get_value(pos) {
                None | Some(Data::Empty) => true,
                Some(Data::String(v)) => v.is_empty(),
                _ => false,
            };
            if blank {
                let f = if f.starts_with('=') {
                    f.clone()
                } else {
                    format!("={f}")
                };
                range.set_value(pos, Data::String(f));
            }
        }
    }

    match format {
        "csv" => Ok(range_to_csv(&range)?),
        "json" => Ok(range_to_json(&range)?),
        other => Err(Error::Tool(format!(
            "unknown format {other:?}; expected \"csv\" or \"json\""
        ))),
    }
}

/// One line telling the model where the data sits, so an edit addresses
/// the right cells: without it, a model guessed the sheet name ("Sheet1")
/// and wrote its total row over the last data row (dev-plan/69).
fn sheet_header(path: &std::path::Path, sheet: Option<&str>) -> Result<String> {
    let mut workbook = open_workbook_auto(path)
        .map_err(|e| Error::Tool(format!("open {}: {}", path.display(), e)))?;
    let names = workbook.sheet_names();
    let name = match sheet {
        Some(s) => s.to_string(),
        None => names.first().cloned().unwrap_or_default(),
    };
    let range = workbook
        .worksheet_range(&name)
        .map_err(|e| Error::Tool(format!("read sheet {name:?}: {e}")))?;
    let formulas: Vec<String> = workbook
        .worksheet_formula(&name)
        .map(|f| {
            f.cells()
                .filter(|(_, _, v)| !v.is_empty())
                .take(MAX_FORMULAS_LISTED)
                .map(|(r, c, v)| {
                    let (r0, c0) = f.start().unwrap_or((0, 0));
                    let v = if v.starts_with('=') {
                        v.clone()
                    } else {
                        format!("={v}")
                    };
                    format!("{}{} {v}", col_letters(c0 + c as u32), r0 + r as u32 + 1)
                })
                .collect()
        })
        .unwrap_or_default();
    let formulas = if formulas.is_empty() {
        String::new()
    } else {
        format!(" · formulas: {}", formulas.join(", "))
    };
    Ok(match (range.start(), range.end()) {
        (Some((r0, c0)), Some((r1, c1))) => format!(
            "[sheet {name:?} of {names:?} · range {}{}:{}{} · {} rows × {} columns · next empty row {}]",
            col_letters(c0),
            r0 + 1,
            col_letters(c1),
            r1 + 1,
            r1 - r0 + 1,
            c1 - c0 + 1,
            r1 + 2
        ) + &formulas,
        _ => format!("[sheet {name:?} of {names:?} · empty · next empty row 1]"),
    })
}

/// Formula cells written without a cached value read back empty, so the
/// header lists them; otherwise the model goes to Python to check its SUM.
const MAX_FORMULAS_LISTED: usize = 20;

fn col_letters(mut c: u32) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (c % 26) as u8);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

fn range_to_csv(range: &calamine::Range<Data>) -> Result<String> {
    let mut wtr = csv::WriterBuilder::new()
        .quote_style(csv::QuoteStyle::Necessary)
        .from_writer(Vec::new());

    for row in range.rows() {
        let cells: Vec<String> = row.iter().map(data_to_csv_cell).collect();
        wtr.write_record(&cells)
            .map_err(|e| Error::Tool(format!("csv write: {e}")))?;
    }
    let bytes = wtr
        .into_inner()
        .map_err(|e| Error::Tool(format!("csv finalize: {e}")))?;
    String::from_utf8(bytes).map_err(|e| Error::Tool(format!("csv utf-8: {e}")))
}

fn range_to_json(range: &calamine::Range<Data>) -> Result<String> {
    let rows: Vec<Vec<Value>> = range
        .rows()
        .map(|row| row.iter().map(data_to_json_value).collect())
        .collect();
    serde_json::to_string_pretty(&rows).map_err(|e| Error::Tool(format!("json serialize: {e}")))
}

fn data_to_csv_cell(d: &Data) -> String {
    match d {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Int(n) => n.to_string(),
        Data::Float(f) => {
            // Whole numbers should round-trip as ints, not "42.0", so the
            // CSV stays human-readable. Anything that loses precision via
            // f64::trunc keeps its decimal form.
            if f.fract() == 0.0 && f.abs() < 1e15 {
                (*f as i64).to_string()
            } else {
                f.to_string()
            }
        }
        Data::Bool(b) => b.to_string(),
        Data::DateTime(dt) => dt.to_string(),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("#ERR:{e:?}"),
    }
}

fn data_to_json_value(d: &Data) -> Value {
    match d {
        Data::Empty => Value::Null,
        Data::String(s) => Value::String(s.clone()),
        Data::Int(n) => Value::Number((*n).into()),
        Data::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Data::Bool(b) => Value::Bool(*b),
        Data::DateTime(dt) => Value::String(dt.to_string()),
        Data::DateTimeIso(s) => Value::String(s.clone()),
        Data::DurationIso(s) => Value::String(s.clone()),
        Data::Error(e) => Value::String(format!("#ERR:{e:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[tokio::test]
    async fn round_trip_xlsx_create_then_read_csv() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rt.xlsx");

        crate::tools::XlsxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "data": "name,age,score\nAlice,30,95.5\nสมชาย,25,87.2\nBob,40,72.1"
            }))
            .await
            .unwrap();

        let csv = XlsxReadTool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();
        assert!(csv.contains("Alice"), "got: {csv:?}");
        assert!(csv.contains("สมชาย"), "Thai missing: {csv:?}");
        assert!(csv.contains("95.5"), "got: {csv:?}");
    }

    #[tokio::test]
    async fn round_trip_xlsx_create_then_read_json() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("typed.xlsx");

        crate::tools::XlsxCreateTool
            .call(json!({
                "path": path.to_string_lossy(),
                "data": [["name","score","ok"],["Alice",95.5,true],["Bob",72.1,false]]
            }))
            .await
            .unwrap();

        let s = XlsxReadTool
            .call(json!({
                "path": path.to_string_lossy(),
                "format": "json"
            }))
            .await
            .unwrap();
        let (head, body) = s.split_once('\n').unwrap();
        assert!(head.contains("range A1:C3"), "header: {head}");
        assert!(head.contains("next empty row 4"), "header: {head}");
        assert!(!head.contains("formulas"), "header: {head}");
        let v: Value = serde_json::from_str(body).unwrap();
        let rows = v.as_array().unwrap();
        assert_eq!(rows.len(), 3);
        // Numbers came back as numbers (not strings).
        assert!(rows[1][1].is_number(), "score not number: {:?}", rows[1][1]);
        // Booleans came back as bools.
        assert!(rows[1][2].is_boolean(), "bool not bool: {:?}", rows[1][2]);
    }

    #[tokio::test]
    async fn header_lists_formulas_without_cached_values() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("sum.xlsx");
        crate::tools::XlsxCreateTool
            .call(json!({"path": path.to_string_lossy(), "data": "item,amount\na,6000000\nb,4500000"}))
            .await
            .unwrap();
        crate::tools::XlsxEditTool
            .call(json!({"path": path.to_string_lossy(), "op": "set_cell", "cell": "B4", "formula": "=SUM(B2:B3)"}))
            .await
            .unwrap();
        let out = XlsxReadTool
            .call(json!({"path": path.to_string_lossy()}))
            .await
            .unwrap();
        let head = out.lines().next().unwrap();
        assert!(head.contains("B4 =SUM(B2:B3)"), "header: {head}");
        assert!(out.lines().nth(4).unwrap().contains("=SUM(B2:B3)"), "{out}");
        assert!(head.contains("next empty row 5"), "header: {head}");
    }

    #[test]
    fn column_letters() {
        assert_eq!(col_letters(0), "A");
        assert_eq!(col_letters(25), "Z");
        assert_eq!(col_letters(26), "AA");
        assert_eq!(col_letters(701), "ZZ");
        assert_eq!(col_letters(702), "AAA");
    }

    #[test]
    // 3.14 is a familiar decimal for a formatting assertion, not an
    // approximation of PI — the test is about "does a fractional float
    // keep its fraction", and any value would do.
    #[allow(clippy::approx_constant)]
    fn whole_floats_serialize_as_ints() {
        assert_eq!(data_to_csv_cell(&Data::Float(42.0)), "42");
        assert_eq!(data_to_csv_cell(&Data::Float(3.14)), "3.14");
    }
}
