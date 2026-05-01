# บทที่ 11 — Built-in tools

thClaws มาพร้อม built-in tools ประมาณสามสิบตัว ซึ่ง agent จะเลือกใช้
เองโดยอัตโนมัติ คุณจะเห็นการเรียกแต่ละครั้งในรูป `[tool: Name: …]`
ตามด้วย ✓ (สำเร็จ) หรือ ✗ (error) บทนี้คือเอกสารอ้างอิง

## File tools

| Tool | การอนุมัติ | สรุป |
|---|---|---|
| `Ls` | auto | แสดงรายการไดเรกทอรีแบบไม่ recursive |
| `Read` | auto | อ่านไฟล์ (ทั้งไฟล์ หรือเฉพาะช่วงบรรทัด) |
| `Glob` | auto | จับคู่ pattern แบบ shell-glob โดยเคารพ `.gitignore` |
| `Grep` | auto | ค้นด้วย regex ข้ามไฟล์ โดยเคารพ `.gitignore` |
| `Write` | prompt | สร้างไฟล์ใหม่หรือเขียนทับไฟล์เดิม |
| `Edit` | prompt | แทนที่สตริงแบบตรงเป๊ะ (หากไม่ unique จะล้มเหลว) |

ทั้งหมดนี้ถูกจำกัดขอบเขตอยู่ภายใน sandbox ([บทที่ 5](ch05-permissions.md))
สำหรับไฟล์ขนาดใหญ่ agent ถูกฝึกให้ใช้ `Glob` กับ `Grep` เพื่อจำกัด
ขอบเขตก่อน แล้วค่อยใช้ `Read` พร้อมระบุช่วงบรรทัด แทนที่จะดูดไฟล์
ทั้งก้อน อย่างไรก็ตาม tool ไม่ได้บังคับขีดจำกัดขนาดไว้ การ `Read`
ไฟล์ขนาดหลายกิกะไบต์จึงจะพยายามโหลดทั้งหมด หากต้องการขีดจำกัดที่
แน่นอน ให้รันในโหมด `ask` แล้วปฏิเสธการเรียก

## Shell

| Tool | การอนุมัติ | สรุป |
|---|---|---|
| `Bash` | prompt | รันคำสั่ง shell ผ่าน `/bin/sh -c` |

ค่าดีฟอลต์:

- timeout 2 นาที (เขียนทับด้วย `timeout_ms` ได้สูงสุด 10 นาที)
- output ที่เกิน 50 KB จะถูกตัด โดยข้อความเต็มจะถูกบันทึกไว้ที่ `/tmp/thclaws-tool-output/<id>.txt`
- pattern ที่อันตราย (`rm -rf`, `sudo`, `curl | sh`, `dd`, `mkfs`,
  `> /dev/sda`) จะถูกทำเครื่องหมาย `⚠` ก่อนขออนุมัติ
- สำหรับ server ที่รันยาว agent ถูกฝึกให้รันใน background (`... &`)
  หรือห่อด้วย `timeout 10` เพื่อไม่ให้ turn ค้าง
- Python `venv` จะ activate อัตโนมัติหากพบ `./.venv/bin/activate`
  (tool จะ source script `activate` ก่อนรันให้เอง)

## Web

| Tool | การอนุมัติ | สรุป |
|---|---|---|
| `WebFetch` | prompt | HTTP GET (จำกัด body 100 KB) แล้วแปลงเป็น Markdown |
| `WebSearch` | prompt | ค้นเว็บผ่าน Tavily / Brave / DuckDuckGo |

search provider จะถูกเลือกตาม `TAVILY_API_KEY` หรือ `BRAVE_SEARCH_API_KEY`
ที่ตั้งค่าไว้ หากไม่มีจะใช้ DuckDuckGo แทน (ไม่ต้องใช้ key แต่คุณภาพด้อยกว่า)
สามารถบังคับด้วย `searchEngine: "tavily"` ใน settings ได้

## เอกสาร — PDF กับ Office

Tool ภาษา Rust สำหรับสร้างและอ่านไฟล์ PDF, Word, Excel และ PowerPoint
**clean-room port จาก skill ของ Anthropic ที่เป็น source-available** เพื่อให้
thClaws redistribute ได้ภายใต้ MIT/Apache ฟอนต์ Noto Sans + Noto Sans Thai
ฝังไว้ใน binary (~650 KB รวม) ทำให้ภาษาไทย render ได้ถูกต้องโดยไม่ต้อง
อาศัยฟอนต์ที่ติดตั้งในระบบ

| Tool | การอนุมัติ | สรุป |
|---|---|---|
| `PdfCreate` | prompt | Markdown → PDF (printpdf + ฟอนต์ไทยฝังใน, A4/Letter/Legal) |
| `PdfRead` | auto | สกัดข้อความผ่าน `pdftotext` (poppler-utils — `brew install poppler` / `apt install poppler-utils`) |
| `DocxCreate` | prompt | Markdown → Word (.docx) ผ่าน `docx-rs` — heading, list, code block |
| `DocxRead` | auto | สกัดข้อความจากไฟล์ Word (XML walk แบบ pure Rust) |
| `DocxEdit` | prompt | `find_replace` / `append_paragraph` ในไฟล์เดิม |
| `XlsxCreate` | prompt | CSV หรือ JSON 2D-array → Excel (.xlsx) ผ่าน `rust_xlsxwriter` |
| `XlsxRead` | auto | อ่าน XLSX/XLSM/XLSB/XLS/ODS ผ่าน `calamine`; output เป็น CSV หรือ JSON พร้อม type |
| `XlsxEdit` | prompt | `set_cell` / `set_cells` / `add_sheet` / `delete_sheet` — รักษา format ผ่าน `umya-spreadsheet` |
| `PptxCreate` | prompt | markdown outline → PowerPoint (.pptx); `# Heading` = สไลด์ใหม่ |
| `PptxRead` | auto | สกัดข้อความรายสไลด์ (เรียงตามตัวเลข — slide10 ไม่มาก่อน slide2) |
| `PptxEdit` | prompt | `find_replace` ทั่วทุกสไลด์ — ออกแบบมาสำหรับเทมเพลต `{{placeholder}}` |

**การ render ภาษาไทยในแต่ละ format:**

- `PdfCreate` ฝังฟอนต์ Noto Sans Thai TTF ลงในไฟล์ PDF โดยตรง — ภาษาไทย
  render เหมือนกันทุกผู้ดู ไม่ขึ้นกับฟอนต์ที่ติดตั้ง
- `DocxCreate` / `PptxCreate` ตั้ง `<w:rFonts w:cs="Noto Sans Thai"/>`
  / `<a:cs typeface="Noto Sans Thai"/>` ต่อ run ทำให้ Word และ PowerPoint
  เลือกฟอนต์ไทยจากระบบของผู้ใช้ Win/Mac/Linux รุ่นใหม่ติดตั้ง Noto Sans
  Thai มาให้แล้ว Office จะ fallback ไป Tahoma / Cordia New หากไม่พบ
- `XlsxCreate` ใช้ Calibri (default ของ Excel) — text engine ของ Excel
  จัดการสคริปต์ไทยผ่าน OS Thai font stack โดยไม่ต้องตั้งค่าต่อเซลล์

**Semantics ของ tool edit:**

- `DocxEdit` / `PptxEdit` `find_replace` จับคู่แบบ **per text-run** Word
  และ PowerPoint แบ่ง text เป็นหลาย run เมื่อ style เปลี่ยนกลางย่อหน้า
  (เช่น คำเดียวที่เป็นตัวหนาในประโยค) ดังนั้น substring ที่คาบเกี่ยว
  ขอบเขต style จะไม่ตรง สำหรับเอกสารที่คุณสร้างด้วย `*Create` ของชุดนี้
  จะไม่มีปัญหา (แต่ละ block เป็น run เดียว) สำหรับเอกสารที่มนุษย์สร้าง
  พร้อมจัดสไตล์เยอะ ๆ ให้ flatten style ก่อน
- `XlsxEdit` **รักษา format** — `umya-spreadsheet` ออกแบบมาเพื่อ round-trip
  style, formula, chart และ conditional formatting ในส่วนที่ไม่เกี่ยวข้อง
  จะอยู่ครบหลังจากโหลด+แก้+เซฟ เซลล์ใช้ที่อยู่แบบ A1 (`B7`, `AA12`)

## ปฏิสัมพันธ์กับผู้ใช้

| Tool | การอนุมัติ | สรุป |
|---|---|---|
| `AskUserQuestion` | auto | หยุด turn เพื่อถามคำถามให้ผู้ใช้พิมพ์ตอบ |
| `EnterPlanMode` | auto | สลับเข้าสู่โหมดวางแผน (ไม่เปลี่ยนแปลงอะไรจนกว่าจะ ExitPlanMode) |
| `ExitPlanMode` | auto | กลับมาทำงานตามปกติ |

## การติดตาม task

| Tool | การอนุมัติ | สรุป |
|---|---|---|
| `TaskCreate` | auto | เพิ่ม task หรือ todo |
| `TaskUpdate` | auto | เปลี่ยนสถานะ (pending / in_progress / completed / deleted) |
| `TaskGet` | auto | ค้นหา task ด้วย id |
| `TaskList` | auto | แสดง task ปัจจุบัน |
| `TodoWrite` | auto | แทนที่รายการ todo ทั้งหมดในครั้งเดียว (แบบ Claude Code) |

`TaskCreate`/`Update`/`Get`/`List` เป็นอินเทอร์เฟซแบบละเอียดรายตัว
ขณะที่ `TodoWrite` จะเขียนทับทั้งรายการในครั้งเดียว ซึ่งเป็นตัวที่
agent มักเลือกใช้ระหว่าง turn ที่ต้องวางแผนยาว ๆ ตรวจสอบระหว่าง turn
ได้ด้วย `/tasks`

## การสร้าง agent ย่อย

| Tool | การอนุมัติ | สรุป |
|---|---|---|
| `Task` | prompt | สร้าง sub-agent สำหรับปัญหาย่อยที่แยกเป็นเอกเทศ |

sub-agent มี tool registry ของตัวเอง และ recurse ได้ลึกสุด 3 ระดับ
รายละเอียดอยู่ใน [บทที่ 15](ch15-subagents.md)

## ฐานความรู้ (KMS)

| Tool | การอนุมัติ | สรุป |
|---|---|---|
| `KmsRead` | auto | อ่านหน้าเดียวจากฐานความรู้ที่ผูกไว้ |
| `KmsSearch` | auto | Grep ทุกหน้าใน knowledge base ตัวเดียว |

เครื่องมือเหล่านี้ **จะถูกลงทะเบียนก็ต่อเมื่อมี KMS อย่างน้อยหนึ่งตัว
ผูกอยู่** กับโปรเจกต์ปัจจุบัน (ผ่าน `/kms use NAME` หรือ checkbox
ที่แถบข้าง) โดย agent จะเห็น `index.md` ของ KMS ที่ active แต่ละตัว
ใน system prompt และเรียกเครื่องมือเหล่านี้เพื่อดึงหน้าที่ต้องการ

```
[tool: KmsSearch(kms: "notes", pattern: "bearer")]
```

ผลลัพธ์คือบรรทัดในรูปแบบ `page:line:text` แนวคิดและเวิร์กโฟลว์ฉบับ
เต็มอยู่ใน [บทที่ 9](ch09-knowledge-bases-kms.md)

## MCP tools

tool ของ MCP server ทุกตัวจะถูกค้นพบตอนเริ่มต้น และลงทะเบียนด้วยชื่อ
ที่มี server นำหน้า เช่น `weather__get_forecast`,
`github__list_issues` เป็นต้น ทุกตัวจะ prompt ขออนุมัติก่อนรัน
รายละเอียดอยู่ใน [บทที่ 14](ch14-mcp.md)

## อ่าน tool stream

turn ปกติจะมีหน้าตาแบบนี้:

```
❯ check if there's a README and show me its first section

[tool: Glob: README*] ✓
[tool: Read: README.md] ✓
The README's first section is "Install" — it walks through…
[tokens: 2100in/145out · 1.8s]
```

- `[tool: Name: detail]` — tool ที่ถูกเรียก พร้อมพรีวิว argument
  แบบย่อ (path แรก, คำสั่ง, URL ฯลฯ)
- `✓` ต่อท้าย — tool ทำงานสำเร็จ
- `✗ <error>` ต่อท้าย — tool ล้มเหลว โดยโมเดลจะได้รับ error คืนและอาจ
  ลองใหม่ด้วยวิธีอื่น

## การตัด tool output

คำสั่ง shell และการอ่านไฟล์ที่ผลิต output เกิน 50 KB จะมี body
ถูกตัดในมุมมองของโมเดล โดยเก็บพรีวิวเล็ก ๆ ไว้ให้แทน ส่วนเนื้อหา
เต็มจะถูกบันทึกไว้ที่ `/tmp/thclaws-tool-output/<tool-id>.txt` เพื่อให้
คุณเข้าไปดูเองได้ โมเดลจะได้รับแจ้งเรื่องการตัด และพรีวิวมักเพียงพอ
ให้ทำงานต่อได้

## จำกัดว่า tool ไหนรันได้

มีกลไกสามแบบ:

1. **`allowedTools` / `disallowedTools`** ใน settings — ลบ tool ออก
   จาก registry ไปเลย เพื่อให้โมเดลมองไม่เห็น เหมาะกับเวิร์กโฟลว์
   "read-only review"
2. **Agent defs** ([บทที่ 15](ch15-subagents.md)) — กำหนด scope tool ให้
   เฉพาะแต่ละ agent โดยเขียนทับ registry ส่วนกลาง
3. **Permissions** ([บทที่ 5](ch05-permissions.md)) — tool ยังอยู่ใน registry
   แต่จะ prompt ถามก่อนรัน หากตอบ `n` จะปฏิเสธการเรียก

## Hook ที่เชื่อมกับ tool events

คำสั่ง shell สามารถยิง hook ได้ที่ `pre_tool_use` / `post_tool_use` /
`post_tool_use_failure` / `permission_denied` ดูรายละเอียดใน [บทที่ 13](ch13-hooks.md)
