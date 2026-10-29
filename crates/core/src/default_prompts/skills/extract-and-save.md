---
name: extract-and-save
short_description: Read a file, extract structured info, save to another file
description: Read a source file (image, PDF, DOCX, PPTX, XLSX, markdown, plain text), extract structured information from it, and save the result to a target file in the format the user wants — Excel for tabular data, Word for prose / memos, Markdown for notes, JSON for downstream processing, PowerPoint for slide-shaped output. Use when the user has a file and wants the meaningful content captured into a different file (not just summarized in chat). Examples — namecard photo → contacts.xlsx; receipt photo → expense report .docx; contract PDF → key-terms .md; invoice → line-items .json; meeting notes screenshot → followup .docx.
---

# Extract & Save

Generic file-to-file extraction. The user has a source file and wants the useful content distilled into a target file in their format of choice. You read, you extract, you confirm, you write.

## When to invoke

Trigger when the user combines an **input artifact** with an **explicit save-to-file** intent:

- "Pull the line items from this receipt into expense.xlsx"
- "Extract the key terms from this contract PDF into terms.md"
- "Take the names from these business cards and add them to contacts.xlsx"
- "Read this meeting screenshot and draft a followup memo in followup.docx"
- "อ่านใบเสร็จในรูปนี้ แล้วเขียน expense report เป็น Word"

**Don't** invoke when the user just asks to read or summarize ("what's in this PDF?", "summarize this image"). The skill is specifically the file→file workflow; pure-conversation Q&A doesn't need it.

## The workflow

1. **Identify the input(s).** Filename(s) the user mentioned, dragged in, or pointed at via path. If the request implies multiple files (a folder, a list), confirm scope before reading them all.
2. **Identify the output.** Format and filename. Ask if missing — "Should I save to a `.xlsx`, `.docx`, or just markdown?" Don't guess silently. Default to **append** when an existing file matches; **create** when nothing's there.
3. **Identify what to extract.** Often obvious from the request ("line items", "names and emails", "key dates"). When not, ask: "Which fields do you want me to capture?" — fewer round-trips than guessing wrong.
4. **Read the source** with the right tool:
   - Images (JPG / PNG / HEIC / WebP / GIF): `Read` — the model sees the pixels, vision-driven OCR
   - PDF: `PdfRead` (text-extraction; scanned pages are OCR'd automatically — `ocr: "auto"`, the default — and `ocr: "vision"` forces the model to read the rendered pages)
   - DOCX: `DocxRead`
   - PPTX: `PptxRead`
   - XLSX / XLS / ODS: `XlsxRead`
   - Old or OpenDocument files (.doc / .ppt / .odt / .odp): `DocConvert` to docx / pptx first, then read the result
   - Markdown / plain text / source code: `Read`
5. **Extract structured fields.** Skip blanks rather than inventing values. Preserve original-script text for multilingual content (e.g. capture both English and Thai names).
6. **Confirm before writing.** Show the user a preview of what you extracted as a markdown bullet list (or table for tabular data). One round-trip beats silent corruption — names, phone digits, monetary amounts, and dates are the most-likely-to-misread fields.
7. **Write the output** with the right tool:
   - XLSX: `XlsxCreate` for new file (set the header row), `XlsxEdit` to append rows or update cells
   - DOCX: `DocxCreate` for new file, `DocxEdit` to add to an existing one
   - PPTX: `PptxCreate` / `PptxEdit`
   - PDF: `PdfCreate` (no `PdfEdit` — for an existing PDF, write a new file alongside)
   - Markdown / text: `Write` for new, `Edit` for in-place changes
   - JSON: `Write` (build the structure first, validate with `python3 -c "import json; json.load(...)"` if you're worried about syntax)
8. **Report back** with one line: file path, what was added (e.g. "1 row appended" / "3 sections added"), and how to verify (e.g. "open `contacts.xlsx` to review").

## Tool quick-reference

| Need to do | Use this | Notes |
|---|---|---|
| Read an image (jpg / png / heic / webp) | `Read` | Vision-driven OCR; works on photos, screenshots, dual-language cards |
| Read a PDF (text-based) | `PdfRead` | Returns selectable text + structure |
| Read a PDF (scanned / image-based) | `PdfRead` (`ocr: "auto"` default, or `ocr: "vision"`) | OCR first; low-confidence pages go to vision on the rendered pages |
| Read .doc / .ppt / .odt | `DocConvert` then `DocxRead` / `PptxRead` | Converts on the document service; nothing to install |
| Read DOCX / PPTX / XLSX | `DocxRead` / `PptxRead` / `XlsxRead` | Native Office format readers |
| Create new XLSX with header | `XlsxCreate` | Specify columns + first row |
| Append rows to existing XLSX | `XlsxEdit` | Insert at last-row + 1 |
| Create / edit DOCX | `DocxCreate` / `DocxEdit` | Body markdown supported in Create |
| Write markdown / JSON / plain text | `Write` | Standard filesystem writer |
| Append to an existing text file | `Edit` | For markdown logs, journal-style outputs |

## Worked examples

### Example 1 — namecard photo → `contacts.xlsx`

User: "Add this card to contacts.xlsx"

```
1. Read the image → vision sees the card.
2. Extract: name, name_th, title, company, email, phone, mobile,
   address, website, linkedin, notes.
3. Confirm with the user (markdown bullet list).
4. XlsxCreate contacts.xlsx if missing; XlsxEdit to append the row.
   Header: Date | Name | Name (Thai) | Title | Company | Email |
           Phone | Mobile | Address | Website | LinkedIn | Notes
5. Report: "Row 4 added — total 4 contacts now in contacts.xlsx".
```

### Example 2 — receipt photo → `expense.docx`

User: "Make an expense report from this receipt"

```
1. Read the receipt image.
2. Extract: vendor, date, total, line items (description + qty +
   unit price + line total), tax, payment method, currency.
3. Confirm.
4. DocxCreate expense.docx if missing; otherwise DocxEdit to append
   a "## <date> — <vendor> · <total>" section with the line items
   as a sub-list.
5. Report: "Section 'Expense — 2026-05-07' added to expense.docx
   (12 line items)".
```

### Example 3 — contract PDF → `terms.md`

User: "Pull the key terms from this contract into terms.md"

```
1. PdfRead the contract.
2. Extract: parties, effective date, term length, payment terms,
   IP/confidentiality clauses, termination conditions, governing
   law. Use what's actually in the document — don't synthesize.
3. Confirm with the user (especially monetary figures + dates).
4. Write terms.md with one section per term, source page references
   in `_(p. NN)_` italic.
5. Report: "terms.md created — 7 sections, 412 lines."
```

### Example 4 — invoice → `line-items.json`

User: "Extract the line items from this invoice as JSON for our pipeline"

```
1. Read the invoice (image or PdfRead per format).
2. Extract: list of {sku, description, qty, unit_price, line_total},
   plus invoice-level {invoice_number, date, customer, currency,
   subtotal, tax, total}.
3. Confirm the line-item count + grand total match what's printed.
4. Write line-items.json with the structured payload.
5. Report: "line-items.json created — 23 items, $4,217.50 total
   (matches invoice subtotal)."
```

## Tips

- **Numbers are the highest-risk field.** Phone digits, monetary amounts, dates, IDs. Confirm visually with the user before writing — silent off-by-one in a phone number means an unreachable contact; silent off-by-one in an amount means a wrong expense report.
- **Multilingual content**: capture both scripts when present (e.g. Thai + Latin names on a Thai namecard). Use a separate column / field for the non-default script — it's searchable later.
- **Low-resolution or partial inputs**: name what you couldn't read so the user can fill it in manually. Better to fail visibly than to invent plausible-looking nonsense.
- **Tables in PDFs**: when `PdfRead` returns garbled column alignment, re-run `PdfRead` with `ocr: "vision"` on that page (the model reads the rendered page) and reconstruct the table cell-by-cell. Slower but more accurate.
- **Existing-file detection**: before `XlsxCreate` / `DocxCreate`, check whether the target file already exists. If it does, confirm with the user — overwriting an existing expense report by accident is the bug we want to avoid.
- **Empty fields**: leave them blank. Don't fill with `"N/A"` / `"—"` / `null`-as-text — those clutter the output and confuse downstream filters.
