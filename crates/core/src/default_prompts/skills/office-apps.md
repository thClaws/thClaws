---
name: office-apps
short_description: Create and edit real Excel, Word and PowerPoint files with the office apps
description: Create or edit an Excel workbook (.xlsx), a Word document (.docx) or a PowerPoint deck (.pptx) — budgets, tables with formulas and charts, memos, letters, reports, slide decks — and turn them into PDF. Use whenever the user wants an Office file made or changed and the `gridcraft__*`, `wordcraft__*` or `deckcraft__*` tools are available. Covers Thai documents (Sarabun font, line spacing) and checking the result.
---

# Office apps: Excel, Word, PowerPoint

Three office apps run as tool servers:

| Tools | App | Saves |
|---|---|---|
| `gridcraft__*` | GridCraft (Excel) | `.xlsx` |
| `wordcraft__*` | WordCraft (Word) | `.docx`, `.pdf` |
| `deckcraft__*` | DeckCraft (PowerPoint) | `.pptx`, `.pdf`, `.png` |

Every button in each app is a command: `list_commands` finds one by name, and `execute_command` (Grid), `execute`/`batch` (Word) or `run_command` (Deck) runs it.

## Rules for every file

1. **Paths are relative to the workspace**, e.g. `reports/budget-q4.xlsx`. Use the name the user gave; never overwrite a file the user did not ask you to change.
2. **Thai text uses the font Sarabun.** Set it as you write. When a file is saved, thClaws also fixes Thai for Office by itself: Sarabun as the Thai font, 125% line spacing, no red spelling marks in Word, and Excel text stored the way Excel shows correctly. Do not script those fixes yourself.
3. **Copy the user's text exactly.** Never shorten, rephrase or translate it.
   - **One kind of digit in the whole file.** If the user wrote Thai digits (๒๕๗๐), use Thai digits everywhere, also in text and tables you write yourself; otherwise use 0–9. Formulas and numbers in Excel cells stay 0–9.
4. **Never make up real details.** Names of people, organisations, document numbers, dates, rooms or amounts the user did not give go in as dotted blanks `………………` for the user to fill in. Do not invent them.
5. **Save, then read it back** (`read_range`, `get_text`, `inspect_slide`) and compare with what the user asked for: numbers, totals, order, every line present.
6. **Never tell the user the Thai spelling is correct.** Models misread Thai. Say which file you made, quote the Thai text, and ask the user to check every consonant, vowel and tone mark.

## Excel (GridCraft)

1. `new_workbook`, then `execute_command` `sheet.rename` `{"name": "..."}` if the sheet needs a name.
2. `write_range` from `A1` with the whole table at once:
   - Numbers are JSON numbers, not strings.
   - Formulas are strings starting with `=`, e.g. `"=SUM(B2:D2)"`.
3. `format_range`:
   - `{"fontName": "Sarabun"}` on the whole table;
   - `"bold": true` on the header and total rows;
   - `"numberFormat": "#,##0"` or `"#,##0.00"` on money;
   - `"horizontalAlign": "right"` on the headers of number columns, so they line up with the numbers.
4. **Set every column width yourself** with `execute_command` `home.columnWidth` `{"cols": "A:A", "chars": N}`. Do not use autofit: it makes Thai columns far too wide and leaves number columns too narrow, so headers run into each other.
   - **Text column:** N = the longest label's letter count + 4, at least 12. Thai vowel and tone marks written above or below a letter do not count.
   - **Number columns:** N = 13. Use more only for numbers over 999,999,999.
5. **Fit the printout to one page wide:** `execute_command` `pageLayout.pageSetup` `{"paper": "A4", "fitToPage": true, "fitWidth": 1, "fitHeight": 0}`. Use `"orientation": "landscape"` when there are more than 6 columns.
6. Charts: `insert_chart` `{"range": "A1:D6", "type": "column", "title": "...", "at": "G2"}`.
7. `save_workbook` `{"path": "....xlsx"}`, then `read_range` with `"formatted": true` and check every total.

## Word (WordCraft)

Write the whole document in **one `batch`** call; typing step by step costs many times more, and looking up commands one by one wastes even more. `new_document` `{"template": "blank"}` first, then this memo recipe (change the text, keep the steps):

```json
{"commands": [
  {"command": "layout.size", "params": {"name": "A4"}},
  {"command": "format.font", "params": {"name": "Sarabun"}},
  {"command": "format.size", "params": {"size": 16}},
  {"command": "para.alignCenter"}, {"command": "format.bold"},
  {"command": "text.insert", "params": {"text": "บันทึกข้อความ"}},
  {"command": "text.newParagraph"},
  {"command": "format.bold"}, {"command": "para.alignLeft"},
  {"command": "text.insert", "params": {"text": "ส่วนราชการ  ………………"}},
  {"command": "text.newParagraph"},
  {"command": "text.insert", "params": {"text": "เรื่อง  ..."}},
  {"command": "text.newParagraph"},
  {"command": "text.insert", "params": {"text": "ย่อหน้าที่ ๑ ทั้งย่อหน้าในคำสั่งเดียว"}},
  {"command": "text.newParagraph"},
  {"command": "para.bullets"},
  {"command": "text.insert", "params": {"text": "วาระที่ ๑ ..."}},
  {"command": "text.newParagraph"},
  {"command": "text.insert", "params": {"text": "วาระที่ ๒ ..."}},
  {"command": "text.newParagraph"},
  {"command": "para.bullets", "params": {"off": true}},
  {"command": "insert.table", "params": {"rows": 3, "cols": 3}},
  {"command": "table.columnWidth", "params": {"width": 60}},
  {"command": "text.insert", "params": {"text": "ลำดับ"}}, {"command": "text.tab"},
  {"command": "table.columnWidth", "params": {"width": 240}},
  {"command": "text.insert", "params": {"text": "วาระ"}}, {"command": "text.tab"},
  {"command": "table.columnWidth", "params": {"width": 160}},
  {"command": "text.insert", "params": {"text": "ผู้นำเสนอ"}}, {"command": "text.tab"},
  {"command": "text.insert", "params": {"text": "๑"}}, {"command": "text.tab"},
  {"command": "text.insert", "params": {"text": "..."}}, {"command": "text.tab"},
  {"command": "text.insert", "params": {"text": "..."}},
  {"command": "caret.docEnd"},
  {"command": "text.newParagraph"},
  {"command": "text.insert", "params": {"text": "จึงเรียนมาเพื่อโปรดทราบ"}},
  {"command": "text.newParagraph"},
  {"command": "para.alignRight"},
  {"command": "text.insert", "params": {"text": "(ลงชื่อ) ………………………………"}},
  {"command": "text.newParagraph"},
  {"command": "text.insert", "params": {"text": "(………………………………)"}}
]}
```

- **One paragraph = one `text.insert`.** Never put `\n` inside the text and never break a paragraph by hand: Word wraps lines itself.
- **No empty paragraphs** for spacing.
- **Tables:** `text.tab` moves to the next cell. Set each column's width (`table.columnWidth`, in points; the page has about 450) while the caret is in that column of the first row. A column must be wide enough for its longest single word, or Word breaks the word in the middle. `caret.docEnd` leaves the table.
- **Finish:** `save_document` `{"path": "....docx"}`, then `get_text` to check.

## PowerPoint (DeckCraft)

1. `new_presentation` (themes: DeckCraft, Harbor, Ember, Meadow, Nocturne, Paper, Coral Reef, Slate).
2. The title slide: `inspect_slide` `{"index": 0}` gives the shape ids. Then `set_text` the title and subtitle.
3. More slides: `add_slide` `{"layout": "titleAndContent", "title": "...", "body": "line 1\nline 2"}`. One bullet per line; a leading tab indents.
4. Tables: `insert_table` `{"rows": R, "cols": C, "data": [["หัว", "..."], ...]}`.
5. Fonts: `run_command` `font.used`, then `font.replace` `{"from": "<each font>", "to": "Sarabun"}`.
6. `save_presentation` `{"path": "....pptx"}`, then `inspect_slide` on each slide to check.

## PDF

| From | How |
|---|---|
| Word | `DocConvert` `{"path": "....docx", "to": "pdf"}` after saving the .docx. Not WordCraft's own PDF: it breaks Thai words in the middle. |
| PowerPoint | `deckcraft__export` `{"path": "....pdf"}` |
| Excel | `DocConvert` `{"path": "....xlsx", "to": "pdf"}`. **Never** GridCraft's own PDF: it prints Thai as empty boxes. |

## Tell the user

- **Which files** you made, and with which app.
- **What to check:** the Thai spelling, and that the numbers are right.
- **Excel for Mac only:** Excel for Mac can draw Sarabun's upper vowels wrong even though the file is right. If they see that, open the file in Numbers, or in Excel on Windows.
