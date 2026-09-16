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
| `WebFetch` | prompt | HTTP GET (จำกัด body 100 KB ต่อ section) ถ้ามี `HAL_API_KEY` → ยิงทั้ง HAL headless-browser scrape **และ** plain HTTP GET พร้อมกัน return เป็น response เดียวที่มี 2 section แยกป้าย (ดูด้านล่าง) |
| `WebSearch` | prompt | ค้นเว็บผ่าน Tavily / Brave / SerpAPI (Google) / You.com / Serply (Google) / DuckDuckGo |
| `WebScrape` | prompt | HAL scrape ตรงๆ พร้อม parameter ขั้นสูง (`wait_for` CSS selector, `scroll_to_bottom`, `remove_selectors`, `output_format`) — จะปรากฏเฉพาะเมื่อมี `HAL_API_KEY` |
| `YouTubeTranscript` | prompt | ดึง transcript ของวิดีโอ YouTube ผ่าน HAL (รองรับหลายภาษา + timestamps) — จะปรากฏเฉพาะเมื่อมี `HAL_API_KEY` |

search provider จะถูกเลือกตาม `TAVILY_API_KEY`, `BRAVE_SEARCH_API_KEY`,
`SERPAPI_API_KEY`, `YDC_API_KEY` หรือ `SERPLY_API_KEY` ที่ตั้งค่าไว้ หากไม่มีจะใช้ DuckDuckGo แทน (ไม่ต้องใช้ key แต่คุณภาพด้อยกว่า)
สามารถบังคับด้วย `searchEngine: "tavily"` ใน settings ได้

### พฤติกรรม combine ของ `WebFetch` (เมื่อมี `HAL_API_KEY`)

ก่อนหน้านี้ `WebFetch` ทำ plain HTTP GET เพียงอย่างเดียว ตอนนี้ถ้าตั้ง `HAL_API_KEY` ไว้ จะยิง 2 path พร้อมกัน แล้วส่ง response รวมแบบ 2 section กลับมาให้ model:

```
[via HAL scrape — JS-rendered + extracted to Markdown]

# {page title}

(เนื้อหา Markdown ที่ render แล้ว)

---

[via plain HTTP GET — raw response body]

(raw HTTP body — เก็บ JSON, headers-style content, อะไรที่ HAL อาจทำเสีย)
```

Agent เลือก slice ที่ตอบคำถาม:

- **HAL section** สำหรับ SPA / JS-rendered / docs / blog content
- **Plain GET section** สำหรับ JSON API / sitemap / robots.txt / อะไรที่ raw bytes สำคัญ

ถ้า path ใด path หนึ่ง fail อีกตัวยังกลับมา + `[note: …]` อธิบาย `prefer_raw: true` ข้าม HAL ทั้งหมด (เร็วกว่า half token) — ใช้เมื่อรู้ว่า URL เป็น JSON endpoint. `max_bytes` (default 100 KB) cap แต่ละ section อิสระ ถ้าไม่มี `HAL_API_KEY` → `WebFetch` เป็น plain GET ปกติ

### Tool ที่ใช้ service key (HAL)

`WebScrape` และ `YouTubeTranscript` เรียก public API ของ HAL
(`hal.thaigpt.com/api`) — ทั้งสองตัวใช้ `HAL_API_KEY` เดียวกัน วาง key
ที่ **Settings → Providers → Service keys → HAL Public API** หรือ set
`HAL_API_KEY` ใน shell tool เหล่านี้จะปรากฏใน tool list ของ model
อัตโนมัติเมื่อมี key และจะหายไปเมื่อไม่มี ไม่เปลือง token และไม่
ชวน model เรียกแล้ว fail key เปลี่ยนระหว่าง session ก็ flip
in/out ใน turn ถัดไป ไม่ต้อง restart

เรียก `WebScrape` ตรงๆ เฉพาะเมื่อต้องการ parameter HAL ขั้นสูง (`wait_for` CSS selector, `scroll_to_bottom`, `remove_selectors`, `output_format`) สำหรับการอ่านหน้าทั่วไป ใช้ `WebFetch` ดีกว่าเพราะจะได้ทั้ง HAL rendered และ raw plain-GET ในตัวเดียว

pattern `requires_env` นี้ใช้ได้ทั่วไป tool ตัวใดก็สามารถประกาศได้
registry จะกรองออกเมื่อ env var ที่ระบุยังไม่ตั้ง HAL ทั้งสองตัว
คือ user แรกที่ใช้ pattern นี้

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

## สื่อ — สร้างภาพและวิดีโอ

เครื่องมือสร้างและแก้ไขภาพ/วิดีโอแบบ provider-abstracted หนึ่ง tool ต่อ
หนึ่งงาน เลือก backend ด้วยอาร์กิวเมนต์ `provider` + `model` **ปิดอยู่โดย
ค่าเริ่มต้น** — ดู "เปิดใช้ media tools" ด้านล่าง ภาพถูกเขียนไปที่
`output/img-<ts>-<hash>.<ext>` ส่วนวิดีโอรันเป็น async job แล้วไปอยู่ที่
`output/vid-<ts>-<hash>.mp4` เมื่อเสร็จ

| Tool | การอนุมัติ | สรุป |
|---|---|---|
| `TextToImage` | prompt | prompt → ภาพ |
| `ImageToImage` | prompt | ภาพต้นทาง + prompt → ภาพที่แก้แล้ว |
| `TextToVideo` | prompt | prompt → วิดีโอ (async job) |
| `ImageToVideo` | prompt | ภาพต้นทางเป็นเฟรมแรก + prompt → วิดีโอ (async job) |
| `MediaJobStatus` | auto | poll งาน async ด้วย `job_id` → `running` / `done` (path) / `failed` |
| `TextToSpeech` | ถาม | ข้อความ → ไฟล์เสียงพูด (Gemini TTS) เขียนลง `output/` เหมือนสื่ออื่น เสียงบรรยายยาวๆ จึงไม่ต้องวิ่งผ่านบทสนทนา |
| `RenderSlides` | ถาม | สไลด์ Marp แบบ markdown → PDF + PNG หน้าละไฟล์ ผ่านบริการ slide-render ของ thClaws |
| `QuizRender` | ถาม | เรนเดอร์แบบทดสอบที่สร้างขึ้นให้กลายเป็น artifact ที่เล่นได้ — เป็นตัวที่ `/quiz` เรียกใช้ |

**โมเดลและ key** (เลือกด้วยอาร์กิวเมนต์ `model`):

| Provider | โมเดลภาพ | โมเดลวิดีโอ | Key |
|---|---|---|---|
| Google Gemini | `gemini-3.1-flash-image`, `gemini-3.1-pro-image` | `veo-3.1-fast-generate-preview`, `veo-3.1-generate-preview`, `veo-3.1-lite-generate-preview` | `GEMINI_API_KEY` / `GOOGLE_API_KEY` |
| OpenAI | `gpt-image-2` | — | `OPENAI_API_KEY` |
| Alibaba DashScope | `qwen-image-2.0`, `qwen-image-2.0-pro` | `happyhorse-1.0-t2v` (text→video), `happyhorse-1.0-i2v` (image→video) | `DASHSCOPE_API_KEY` |
| LTX (Lightricks) | — | `ltx-2-3-fast` (alias `ltx`), `ltx-2-3-pro` (`ltx-pro`), `ltx-2-5-fast` (`ltx-2-5`), `ltx-2-5-pro` | `LTX_API_KEY` |

- **วิดีโอเป็นแบบ asynchronous** `TextToVideo` / `ImageToVideo` จะ submit
  งานแล้วคืน `job_id` ทันที — ไฟล์ยังไม่พร้อม เรียก
  `MediaJobStatus { job_id }` เพื่อ poll: `running`, `done` (พร้อม path
  `output/…mp4`) หรือ `failed` (พร้อม error ของ provider) สถานะงานถูก
  บันทึกที่ `.thclaws/state/media-jobs.jsonl` การ poll จึงรอดแม้รีสตาร์ท
- **คลิปยาว 4–8 วินาที** `resolution` มีผลกับ LTX และ HappyHorse
  (`720P` / `1080P` และ LTX รับ `4K` ด้วย) ส่วน Veo ไม่สนใจค่านี้ —
  render ตามขนาดมาตรฐานของ aspect ที่เลือก
- **LTX สร้างเสียงเองได้ รวมถึงเสียงพูดภาษาไทย** และเป็นตัวเดียวที่คุม
  ให้ตัวละครหน้าเหมือนเดิมข้ามช็อตได้ ถ้าป้อนภาพอ้างอิงผ่าน
  `ImageToVideo` สวิตช์เสียงมีตัวเดียวคือ `generate_audio` (ดีฟอลต์ true,
  ใส่ false ได้คลิปเงียบ) — **ไม่มีพารามิเตอร์ voice/ภาษา/สำเนียง**
  ทั้งหมดคุมจาก **prompt**: ใส่บทพูดในเครื่องหมายคำพูดและระบุภาษา+สำเนียง
  (`พูดภาษาไทย สำเนียงกรุงเทพธรรมชาติ: "…"`) อีกสองปุ่มที่มีผลกับคุณภาพเสียง
  คือ `fps` (24/25/48/50 — 48 หรือ 50 ทำให้เห็นการปิดปาก P/B/M ชัด ซึ่ง
  24/25 จะเบลอ) และ `duration` ที่ต้องยาวพอจะพูดจบโดยไม่ต้องรีบ LTX รับ
  4, 6, 8, 10 … 20 วินาที ค่าอื่นจะถูกปัดเข้าค่าที่ใกล้ที่สุด ส่วน
  `ltx-2-5-*` คือรุ่นปัจจุบัน (เสียงพูดดีขึ้น ทำหลายช็อตในคลิปเดียวโดยคง
  เสียงเดิม) ราคาราว 2 เท่าของ 2.3 LTX รับขนาดเป็นพิกเซลไม่ใช่ aspect tier ดังนั้น
  `aspect_ratio: 9:16` จะสลับด้านของ resolution ที่เลือก (1080P →
  1080x1920) ตั้ง `LTX_BASE_URL` เพื่อชี้ไป deployment ที่ self-host ได้
- **`ImageToVideo`** ใช้ภาพในเครื่องเป็นเฟรมแรก ส่งแบบ inline (base64
  data URI) — ไม่มีขั้นตอน upload แยก

### เปิดใช้ media tools

media tools มีค่าใช้จ่ายต่อภาพ / ต่อวินาทีวิดีโอ จึง **ปิดอยู่โดยค่า
เริ่มต้น** เปิดใน `settings.json`:

```jsonc
// ./.thclaws/settings.json
{ "mediaToolsEnabled": true }   // alias เดิม: "imageToolsEnabled"
```

GUI shell **Media Studio** ที่มีมาให้ (บทที่ 26) จะเปิด media tools ให้
อัตโนมัติสำหรับ session ของมันเองโดยไม่สนใจ flag นี้ — เป็นทางเข้าแบบ
คลิก ๆ ไม่ต้องตั้งค่า สำหรับคนที่ไม่ได้สั่ง agent ผ่านแชต

## รีวิววิดีโอ & สร้างหนัง

| Tool | การอนุมัติ | ทำอะไร |
|---|---|---|
| `WatchVideo` | prompt | ให้ model **ดู** วิดีโอในเครื่อง: ดึง key frame แบบรู้ฉาก (เพื่อให้มัน *เห็น* ว่าเกิดอะไรขึ้น) + transcript จาก Whisper เมื่อมี `GROQ_API_KEY` ใช้รีวิว/วิจารณ์คลิป |
| `FilmCompile` / `FilmGenerate` / `FilmJobStatus` / `FilmJobCancel` / `FilmAssetImport` | `FilmGenerate` + `FilmAssetImport` = prompt | ชุดเครื่องมือ **Movie Maker** — เปลี่ยนบท `.film` เป็นวิดีโอ AI ที่เสร็จสมบูรณ์ ซ่อนอยู่จนกว่าจะติดตั้ง agent Movie Maker (ซึ่งเปิด gate `filmscript`) `FilmGenerate` ต้องมี `budgetUsd` — เป็นทั้งเพดานเงินและการยินยอม Movie Maker เป็น catalog agent ไม่ใช่ส่วนหนึ่งของ engine ติดตั้งด้วย `/cloud get movie-maker-2` แล้วมันมีคู่มือของตัวเองมาให้ |

## Tool อื่น ๆ

- **`FetchImages`** — ดาวน์โหลดรูปจากเน็ตทุกรูปที่อ้างในไฟล์ Markdown ลง
  โฟลเดอร์ `images/` ข้าง ๆ แล้วแก้ลิงก์ให้ (ใช้โดย agent content-extractor)
  จำกัดอยู่ใน workspace ของคุณ
- **`FolderIndex`** — สร้าง `<folder>/index.md`: ตารางหนึ่งแถวต่อหนึ่งไฟล์
  พร้อม **title** สั้น ๆ (ว่าเป็นเรื่องอะไร) และคำอธิบาย ทั้งคู่เขียนจาก *เนื้อหา*
  จริงของไฟล์ ไม่ใช่จากชื่อไฟล์ ทำงานแบบ
  deterministic — เดินโฟลเดอร์ ทำ fingerprint ทุกไฟล์ (sha256) เขียน index
  แล้วส่งให้ agent `folder-indexer` อ่านเฉพาะไฟล์ที่เนื้อหาเปลี่ยนไปจาก
  สรุปที่ cache ไว้ ดังนั้นการ index ซ้ำโฟลเดอร์ที่ไม่มีอะไรเปลี่ยนจะไม่อ่าน
  ไฟล์ใหม่เลย cache ของ fingerprint อยู่ข้าง index ที่
  `<folder>/.thclaws-index.json` — ลบทิ้งถ้าอยากเริ่มใหม่หมด ซ่อนอยู่จนกว่า
  จะใช้ agent `folder-indexer` (คลิกขวาที่โฟลเดอร์ในแท็บ Files →
  **Index folder…** หรือ `/index`) ใส่ `report_depth: 1` ถ้าอยากให้ index
  เหลือระดับเดียว — ลิสต์เฉพาะไฟล์ในโฟลเดอร์นี้ บวกโฟลเดอร์ย่อยแถวละบรรทัด
  บอกว่าข้างในมีอะไร (ของที่ลึกกว่านั้นยัง scan และสรุปครบ เพราะเป็นวัตถุดิบ
  ของบรรทัดสรุปนั้น แค่ไม่ถูกลิสต์) ในโมดัลคือตัวเลือก **This folder + one
  row per subfolder**
- **`EpubCreate`** — Markdown → e-book EPUB (เข้ากับชุด document tool
  Word/Excel/PowerPoint/PDF ด้านบน)

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
| `WorkflowRun` | prompt | author + รัน workflow (JavaScript) ที่กระจายงานไปยัง subagent หลายตัว |

sub-agent มี tool registry ของตัวเอง และ recurse ได้ลึกสุด 3 ระดับ
([บทที่ 15](ch15-subagents.md)) ส่วน `WorkflowRun` คือทางเข้า workflow
แบบที่ model สั่งเอง ([บทที่ 25](ch25-workflows.md))

## Memory

| Tool | การอนุมัติ | สรุป |
|---|---|---|
| `MemoryRead` | auto | อ่าน entry จาก persistent file-based memory |
| `MemoryWrite` | prompt | สร้าง/แทนที่ memory entry (frontmatter + body) |
| `MemoryAppend` | prompt | ต่อท้าย memory entry ที่มีอยู่ |

หน่วยความจำข้ามเซสชัน เก็บว่าคุณเป็นใครและชอบทำงานแบบไหน —
ดู [บทที่ 8](ch08-memory-and-agents-md.md)

## Goals (โหมด `/goal` อัตโนมัติ)

| Tool | การอนุมัติ | สรุป |
|---|---|---|
| `RecordGoalProgress` | auto | บันทึกความคืบหน้าหนึ่งขั้นของ goal ที่ทำอยู่ |
| `MarkGoalComplete` | auto | ประกาศว่า goal เสร็จ — จบลูปอัตโนมัติ |
| `MarkGoalBlocked` | auto | ทำเครื่องหมายว่า goal ติด (ต้องให้ผู้ใช้ช่วย) |

register เฉพาะตอนมี `/goal` รันอยู่ — ลูปใช้ tool พวกนี้รายงานความคืบหน้า
และตัดสินใจว่าจะหยุดเมื่อไร

## ฐานความรู้ (KMS)

| Tool | การอนุมัติ | สรุป |
|---|---|---|
| `KmsRead` | auto | อ่านหน้าเดียวจากฐานความรู้ที่ผูกไว้ (prepend banner `[note: …]` เมื่อ `verified:` ขาดหรือเก่ากว่า 90 วัน) |
| `KmsSearch` | auto | Grep ทุกหน้าใน knowledge base ตัวเดียว |
| `KmsWrite` | prompt | สร้างหรือเขียนทับหน้า; auto-inject `# {title}\nDescription: {topic}\n---` header; warn เมื่อขาด `sources:` frontmatter |
| `KmsAppend` | prompt | ต่อท้ายหน้าที่มีอยู่ |
| `KmsDelete` | prompt | ลบหน้า (ทางสุดท้าย; prefer KmsWrite สำหรับ merge หรือ supersede) |
| `KmsCreate` | auto | Ensure ว่า KMS มีอยู่ (idempotent) `/dream` ใช้ bootstrap `dreams` audit KMS |
| `KmsWriteSource` | ถาม | บันทึกหน้าเว็บที่ดึงมาลงโฟลเดอร์ `sources/` ของ KMS เป็นสำเนา **offline** — คือ input ชั้นที่ 1 ที่หน้า synthesis อ้างอิงถึง เก็บไว้เพื่อให้หลักฐานของหน้ายังอยู่แม้ URL ต้นทางจะหายไป |

เครื่องมือเหล่านี้ **ลงทะเบียนเสมอ** ไม่ว่าจะมี KMS active หรือไม่ ก่อน fix นี้การลงทะเบียนถูก gate ด้วย `kms_active` ที่ไม่ว่าง ซึ่งทำให้ `/dream` และ side-channel agent ตัวอื่น bootstrap audit KMS จากศูนย์ไม่ได้ Agent จะเห็น `index.md` ของ KMS ที่ active แต่ละตัวใน system prompt และเรียกเครื่องมือเหล่านี้เพื่อดึงหน้าที่ต้องการ

```
[tool: KmsSearch(kms: "notes", pattern: "bearer")]
```

ผลลัพธ์คือบรรทัดในรูปแบบ `page:line:text` แนวคิด เวิร์กโฟลว์ และ canonical page shape (`title:` / `topic:` / `sources:` / `verified:`) ฉบับเต็มอยู่ใน [บทที่ 9](ch09-knowledge-bases-kms.md)

## MCP tools

tool ของ MCP server ทุกตัวจะถูกค้นพบตอนเริ่มต้น และลงทะเบียนด้วยชื่อ
ที่มี server นำหน้า เช่น `weather__get_forecast`,
`github__list_issues` เป็นต้น ทุกตัวจะ prompt ขออนุมัติก่อนรัน
รายละเอียดอยู่ใน [บทที่ 14](ch14-mcp.md)

## Tool ที่มี gate — GUI Shell

tool บางตัว **register ไว้แต่ซ่อน** จนกว่า skill จะเปิด *gate* ของมัน
ตอนปิดอยู่ **กิน token = 0** (model ไม่เห็นชื่อ tool เลย) แล้วโผล่มาเฉพาะ
ตอนต้องใช้จริง tool สำหรับสร้าง GUI Shell (custom HTML frontend,
[บทที่ 26](ch26-gui-shells.md)) ทำงานแบบนี้:

| Tool | การอนุมัติ | สรุป |
|---|---|---|
| `GuiShellCreate` | prompt | สร้าง GUI Shell ใหม่ (manifest + entry HTML) ที่ `.thclaws/gui-shell/<id>/` |
| `GuiShellWriteFile` | prompt | เขียน/แทนที่ไฟล์ใน GUI Shell — path-jail อยู่ใน dir ของตัวเอง |
| `GuiShellList` | auto | ลิสต์ GUI Shell ที่ติดตั้งไว้ |
| `GuiShellRemove` | prompt | ลบ GUI Shell |

### gate เปิดยังไง

1. **แต่ละ gated tool ประกาศ gate ของตัวเอง** — `Tool::requires_gate()`
   คืน `Some("gui-shell")` ทำให้ **per-turn tool filter ซ่อนมันไว้**
   (กลไกเดียวกับ `requires_env` ของ service-key tool) → `GuiShell*` ทั้ง 4
   ตัวมองไม่เห็นโดย model และไม่เพิ่มอะไรใน system prompt
2. **มี skill เป็นเจ้าของ gate** — skill **`gui-shell`** ที่ ship มาประกาศ
   `tool-gate: gui-shell` ใน frontmatter ([บทที่ 12](ch12-skills.md))
3. **เรียก skill = เปิด gate** — skill นี้ trigger เมื่อขอ "สร้าง UI /
   dashboard / custom frontend …" พอ model เรียก `SkillTool::call` จะรัน
   `activate_gate("gui-shell")`
4. **มีผลรอบถัดไป** — gate เป็น **process-global + session-sticky** เปิด
   แล้วอยู่ยาวทั้ง session per-turn filter เช็ค gate ใหม่ในรอบถัดไป →
   `GuiShell*` โผล่มาเรียกได้ **โดยไม่ต้อง rebuild agent/registry**

→ model จะใช้ GUI-Shell tool ลอยๆ ไม่ได้ ต้องผ่าน skill ก่อน ซึ่งเป็น
ตัวที่ทำให้ tool โผล่มา กลไก gated-tool-group นี้ reuse ได้ — tool group
ในอนาคตก็ surface แบบนี้ได้ (skill เปิด gate → group โผล่)

## อ่าน tool stream

turn ปกติจะมีหน้าตาแบบนี้:

```
❯ check if there's a README and show me its first section

[tool: Glob: README*] ✓
[tool: Read: README.md] ✓ 0.2s
The README's first section is "Install" — it walks through…
[tokens: 2100in/145out · 1.8s]
```

- `[tool: Name: detail]` — tool ที่ถูกเรียก พร้อมพรีวิว argument
  แบบย่อ (path แรก, คำสั่ง, URL, search query ฯลฯ) โดยค่าที่ดูเหมือน
  secret เช่น token, API key, password และ bearer auth header จะถูก
  redact ก่อนแสดงผล
- `✓ <duration>` ต่อท้าย — tool ทำงานสำเร็จ พร้อมเวลาที่ใช้
- `✗ <error>` ต่อท้าย — tool ล้มเหลว โดยโมเดลจะได้รับ error คืนและอาจ
  ลองใหม่ด้วยวิธีอื่น
- tool ที่รันนานจะแสดง heartbeat แบบไม่ถี่เกินไป หลังประมาณ 10 วินาที
  แล้วตามด้วยทุก ๆ ประมาณ 30 วินาทีขณะยังรันอยู่:

```
[tool: Bash (cargo test -p thclaws-core)] still running 40s
```

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
