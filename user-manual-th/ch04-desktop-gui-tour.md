# บทที่ 4 — ทัวร์ Desktop GUI

การรัน `thclaws` โดยไม่ใส่อาร์กิวเมนต์ใด ๆ จะเปิดแอป desktop แบบ native ซึ่งเป็น webview ที่หุ้ม React frontend ไว้ และเชื่อมต่อกับ Rust agent core ตัวเดียวกับที่ CLI ใช้ บทนี้คือทัวร์นำชมหน้าต่างหลัก — อ่านสักครั้งเพื่อให้จำทุกส่วนของ UI ได้ตอนต้องใช้งานจริง

ถ้าใช้แต่ REPL ในเทอร์มินัล จะอ่านบทนี้ผ่าน ๆ แล้วข้ามไปก็ได้ ทุกอย่างที่ GUI ทำได้ ก็ใช้ผ่าน terminal ได้เช่นกัน

> **การตั้งค่าเมื่อเปิดใช้งานครั้งแรก** — ตอนเปิด thClaws ครั้งแรก จะมี modal สองตัวขึ้นเรียงกัน (เลือก working directory ก่อน แล้วค่อยเลือกที่เก็บ API key) ทั้งคู่อธิบายไว้ใน[บทที่ 3](ch03-working-directory-and-modes.md#first-launch-setup) แล้ว บทนี้จึงถือว่าคุณผ่านขั้นตอนนั้นมาแล้ว

## หน้าต่างหลัก — เลย์เอาต์

![หน้าต่างหลักของ thClaws — แท็บ Terminal กำลังใช้งาน, sidebar แสดงหมวด Provider / Sessions / Knowledge / MCP](../user-manual-img/ch-01/main-window-layout.png)

- **แถบแท็บ** (ด้านบน) — Terminal, Chat, Files และ Team (ถ้าเปิดใช้) ชื่อหน้าต่างจะแสดงโปรเจกต์ปัจจุบัน
- **Sidebar** (คอลัมน์ซ้าย) — สี่หมวดแบบพับได้ ครอบคลุม provider + model ที่ใช้งานอยู่, session ที่บันทึกไว้, knowledge base ที่แนบไว้ และ MCP server ที่ตั้งค่าไว้
- **เนื้อหาของแท็บที่ใช้งาน** (ขวา) — เปลี่ยนไปตามแท็บที่คุณอยู่: เทอร์มินัลแบบ live, chat แบบ streaming, ตัวเปิดไฟล์ หรือหน้า team
- **แถบสถานะ** (ด้านล่าง) — working directory ปัจจุบันอยู่ทางซ้าย ส่วนไอคอนเฟืองสำหรับ Settings อยู่ทางขวา

### Sidebar (คอลัมน์ซ้าย)

Sidebar แสดงอยู่ตลอดเวลา และประกอบด้วยสี่หมวด:

| หมวด | แสดง | การกระทำ |
|---|---|---|
| **Provider** | provider + model ที่ใช้อยู่ จุดบอกสถานะ และเครื่องหมาย ▾ | คลิกเพื่อเปิด inline model picker (v0.7.2+) |
| **Sessions** | session ที่บันทึกล่าสุด 10 รายการ (ชื่อหรือ ID) | `+` เพื่อเริ่ม session ใหม่ · วางเมาส์เหนือรายการ → ไอคอนดินสอเพื่อเปลี่ยนชื่อ · คลิกเพื่อโหลด |
| **Knowledge** | KMS ทั้งหมดที่ค้นหาได้ พร้อม checkbox เพื่อแนบ | `+` เพื่อสร้าง KMS ใหม่ — ดู[บทที่ 9](ch09-knowledge-bases-kms.md) |
| **MCP Servers** | MCP server ที่ใช้อยู่ พร้อมจำนวน tool | อ่านอย่างเดียวตรงนี้ — ตั้งค่าผ่าน `/mcp add` |

หมวด **Provider** มีตัวแสดงสถานะแบบมองเห็นได้:

- 🟢 จุดเขียว + ข้อความปกติ: provider พร้อมใช้งาน
- 🔴 จุดแดง + ~~ขีดฆ่า~~ + "no API key — set one in Settings": provider ยังไม่มี credential

เมื่อบันทึก key ผ่าน Settings จุดจะเปลี่ยนเป็นสีเขียว และ model ที่ใช้งานอยู่อาจสลับไปยัง provider ตัวแรกที่มี credential ให้โดยอัตโนมัติ — ดู[บทที่ 6](ch06-providers-models-api-keys.md#auto-switch-on-key-save)

**Inline model picker** (v0.7.2+): คลิกที่แถว Provider เพื่อเปิด dropdown แบบ search-as-you-type แสดง model ทุกตัวที่ catalogue รู้จัก จัดกลุ่มตาม provider พร้อม model จาก local Ollama ที่ค้นพบ live ผ่าน `/api/tags` ด้วย คลิกแถวเพื่อสลับ — การเปลี่ยนแปลงจะ persist ลง `.thclaws/settings.json` และ provider ที่ใช้งานอยู่จะถูก rebuild ใน place (ใช้ path เดียวกับ `/model`) กด Esc หรือคลิกนอก dropdown เพื่อยกเลิกโดยไม่เปลี่ยนค่า

### แถบแท็บ

มีแท็บหลักทั้งหมดสี่แท็บ พร้อมไอคอนเฟืองสำหรับ settings อยู่ทางขวา

#### 1. แท็บ Chat

แผง chat แบบ streaming ที่ใช้ประวัติร่วมกับแท็บ Terminal (agent เดียวกัน session เดียวกัน) ข้อความจะ render เป็น Markdown ส่วนการเรียก tool จะแสดงเป็นบล็อก `[tool: Name]` ที่ยุบ/ขยายได้ และการใช้ token จะแสดงต่อท้ายข้อความตอบของ assistant ในแต่ละรอบ

ใช้แท็บ Chat เมื่อคุณชอบ UI แบบสนทนา ส่วนแท็บ Terminal ใช้เมื่อต้องการเห็น output ดิบ ๆ และรัน slash command

#### 2. แท็บ Terminal

เทอร์มินัล xterm.js ที่ฝังอยู่ภายใน รัน `thclaws --cli` (REPL ตัวเดียวกับที่ได้จาก CLI) การกดคีย์จะส่งผ่าน PTY bridge ไปยัง child process ส่วน output ก็ไหลกลับมาผ่าน frame ที่เข้ารหัสด้วย base64

![แท็บ Terminal — agent เพิ่งสแกฟโฟลด์เว็บ static เสร็จ บรรทัด `[tool: Write …]` แสดงการสร้างแต่ละไฟล์ พร้อม token/เวลาที่ใช้ด้านล่าง](../user-manual-img/ch-04/thClaws-gui-terminal.png)

พฤติกรรมสำคัญที่ควรรู้:

- **คัดลอก / วาง** — Cmd+C / Cmd+V (macOS) หรือ Ctrl+Shift+C / Ctrl+Shift+V (Linux/Windows) ทั้งหมดนี้ทำงานผ่าน native `arboard` IPC bridge เพราะ wry บล็อก `navigator.clipboard`
- **Ctrl+C** ทำงานตามบริบท: ถ้าบรรทัดที่พิมพ์อยู่ยังไม่ว่าง จะล้างบรรทัดนั้น (เหมือน `Ctrl+U` ใน bash) แต่ถ้าบรรทัดว่างอยู่แล้ว จะส่งต่อเป็น SIGINT
- **Resize** — ขนาดเทอร์มินัลจะเปลี่ยนตามหน้าต่าง ส่งต่อผ่าน `portable-pty` resize
- **Ctrl+L** ล้างหน้าจอ

#### 3. แท็บ Files

ตัวเปิดไฟล์ที่มี root เป็น working directory คลิกไฟล์ในทรีเพื่อเปิดดูในแพเนลด้านขวา และคลิกไอคอนดินสอข้าง path เพื่อสลับเข้าสู่โหมดแก้ไข

**โหมด Preview** (ค่าเริ่มต้น):

- ไฟล์ `.md` — เรนเดอร์เป็น HTML ที่ฝั่งเซิร์ฟเวอร์ (รองรับ GFM ทั้งตาราง task list ขีดฆ่า autolink และ footnote) แสดงใน iframe ที่มี sandbox โดย HTML ดิบที่ฝังอยู่ใน markdown จะถูกตัดออกก่อนเรนเดอร์
- ไฟล์ `.html` — เรนเดอร์ใน iframe sandbox ตัวเดียวกัน
- ไฟล์โค้ด (`.js`, `.ts`, `.tsx`, `.py`, `.rs`, `.go`, `.java`, `.cpp`, `.php`, `.json`, `.yaml`, `.sql`, `.xml`, `.css` และอื่น ๆ) — ไฮไลต์ syntax ด้วย CodeMirror 6 ในโหมดอ่านอย่างเดียว พร้อมเลขบรรทัด bracket matching และ search panel
- รูปภาพและ PDF — preview แบบ inline
- ไฟล์ข้อความ/คอนฟิก (`.txt`, `.log`, `.env`, `.conf`, `.ini`, `.toml`, `.sh`, `Dockerfile`, …) — แสดงใน `<pre>` ธรรมดา

![โหมด Preview ของแท็บ Files — `script.js` เรนเดอร์ผ่าน CodeMirror พร้อมเลขบรรทัดและการไฮไลต์ syntax ส่วนปุ่ม Edit อยู่มุมขวาบนเพื่อสลับเข้าสู่โหมดแก้ไข](../user-manual-img/ch-04/thClaws-gui-file-viewer.png)

ไฟล์ `.html` จะถูกเรนเดอร์สดใน sandboxed iframe จึงเห็นหน้าเว็บได้เหมือนที่ browser แสดง — style, รูป และ JS แบบ interactive ทำงานได้ครบ

![HTML preview ของแท็บ Files — `index.html` เรนเดอร์อยู่ใน sandboxed iframe พร้อม stylesheet, รูปภาพ และปุ่มที่คลิกได้](../user-manual-img/ch-04/thClaws-gui-file-html-viewer.png)

**โหมด Edit** (ไอคอนดินสอ):

- Markdown เปิดในเอดิเตอร์ **TipTap WYSIWYG** — ตัวเดียวกับที่ใช้กับ `AGENTS.md` ในเมนู Settings และรองรับ round-trip markdown
- ไฟล์โค้ดเปิดใน **CodeMirror 6** พร้อมไฮไลต์ syntax ตามภาษา bracket matching undo history และ search panel โดยภาษาจะเลือกให้จากนามสกุลไฟล์
- จุดทึบ (●) ข้างชื่อไฟล์หมายถึงมีการแก้ไขที่ยังไม่บันทึก ปุ่ม Save จะ disable ไว้จนกว่า buffer จะ dirty
- **Cmd/Ctrl+S** เพื่อบันทึก — มี toast สีเขียว "saved" หรือสีแดง "save failed: …" ขึ้นมายืนยัน
- ปุ่ม **Discard** (เมื่อ dirty) / **Preview** (เมื่อ clean) ใช้ออกจากโหมดแก้ไข การคลิก Discard จะเปิด native OS confirm dialog ("Discard / Keep editing") ขึ้นมาก่อนทิ้งการแก้ไข
- ถ้าคลิกไฟล์อื่นใน sidebar ขณะยัง dirty อยู่ ก็จะเจอ native confirm ตัวเดียวกัน — ต้อง save หรือ discard ก่อนจึงจะย้ายไฟล์ได้
- การ auto-refresh จะหยุด polling ระหว่างที่คุณแก้ไข เพื่อกันไม่ให้ tool call `Write`/`Edit` ของ agent มาทับ buffer ที่กำลังแก้อยู่

![โหมด Edit ของแท็บ Files — `index.html` เปิดอยู่ใน CodeMirror จุด ● หลังชื่อไฟล์หมายถึงยังไม่ได้บันทึก ส่วนปุ่ม Save / Discard จะปรากฏมุมขวาบน](../user-manual-img/ch-04/thClaws-gui-file-editor.png)

การเขียนไฟล์ทำผ่าน sandbox ของ working directory เดียวกับที่ agent ใช้ การแก้จึงอยู่ภายใน project tree เสมอ การ save ที่ผู้ใช้สั่งเองจะ **ไม่** ผ่าน approval prompt ของ agent — เพราะปุ่ม Save ถือเป็นการอนุมัติของคุณอยู่แล้ว

**ปุ่ม Refresh** — อ่านไฟล์ใหม่จากดิสก์และ remount iframe ของ preview ใช้หลังจาก agent แก้ไฟล์เบื้องหลัง (เช่น `dashboard.html` ของ productivity plugin ที่ regenerate task snapshot ของมันเอง) บังคับให้ iframe re-render แทนที่จะใช้ cache ของ browser มี prompt ก่อนทิ้งการแก้ที่ยังไม่ได้ save

**Dashboard host bridge** — HTML dashboard ที่เปิดในแท็บนี้สามารถอ่าน/เขียนไฟล์พี่น้องผ่าน `postMessage` ไปยัง React shell ได้เลย ไม่ต้องใช้ File System Access API picker dashboard ของ productivity plugin ใช้กลไกนี้: เมื่อ Refresh มันอ่าน `TASKS.md` สดผ่าน bridge (ไม่มี snapshot-staleness อีกต่อไป) ส่วน Save เขียนกลับลงดิสก์ผ่าน `file_write` IPC ของ thClaws HTML page ใด ๆ ที่ post `{type: "thclaws-dashboard-load" | "thclaws-dashboard-save", filename, content?}` ไปยัง parent ก็ใช้กลไกเดียวกันได้

#### 4. แท็บ Team

![หน้าต่างหลักของ thClaws — แท็บ Team กำลังใช้งาน](../user-manual-img/ch-04/thClaws-gui-teams.png)

แท็บ Team **ถูกซ่อนไว้โดยค่าเริ่มต้น** จะโผล่ขึ้นมาต่อเมื่อเปิด Agent Teams ผ่านเมนู Settings → Workspace → Agent Teams หรือแก้ `"teamEnabled": true` ใน `.thclaws/settings.json` ด้วยตัวเอง (ปิดเป็นค่า default เพราะทีมสปอว์น process ของ agent หลายตัวขนานกัน กินโทเคนเร็ว) เมื่อเปิดใช้งานแล้ว แท็บนี้จะเป็นที่แสดง pane ของเพื่อนร่วมทีมแต่ละตัว คลิก pane เพื่อ focus และส่ง input ส่วนตัวไปยังสมาชิกคนนั้นได้ รายละเอียดการสร้างทีม การสื่อสารระหว่างสมาชิก รวมถึง tool `TeamCreate` / `SpawnTeammate` / `SendMessage` / `TeamMerge` ดูได้ใน[บทที่ 17](ch17-agent-teams.md)

### เมนู Settings (ไอคอนเฟือง)

คลิกเฟือง ⚙ มุมขวาบนเพื่อเปิดเมนู popup:

| รายการ | เปิด |
|---|---|
| **Global instructions** | Tiptap markdown editor บน `~/.config/thclaws/AGENTS.md` |
| **Folder instructions** | Tiptap editor บน `./AGENTS.md` (ใน working directory) |
| **Provider API keys** | Settings modal สำหรับ key — ดู[บทที่ 6](ch06-providers-models-api-keys.md) |
| **Appearance** | สลับธีม Light / Dark / System — อธิบายด้านล่าง |
| **GUI scale** | ปรับขนาด zoom สำหรับจอ HiDPI / 4K — 75 / 90 / 100 / 110 / 125 / 150 / 175 / 200% (v0.7.3+) |

Tiptap editor แปลง markdown ไป-กลับผ่าน `tiptap-markdown`: คุณแก้ใน UI แบบ rich-text (หัวเรื่อง ตัวหนา list code fence) แล้วบันทึกลงดิสก์เป็น markdown จากนั้น agent ก็อ่านไฟล์นั้นในรอบถัดไป การแปลงไม่มีข้อมูลสูญหายสำหรับ Markdown มาตรฐาน

path ที่แสดงด้านบนของ editor คือชื่อไฟล์ที่ resolve แล้ว ช่วยให้คุณรู้แน่ชัดว่ากำลังแก้อะไรอยู่

### Appearance (Light / Dark / System)

ด้านล่างของเมนูเฟืองจะมีตัวเลือกธีมสามแบบ — Light, Dark, System — โดยตัวที่ใช้งานอยู่จะมีเครื่องหมายถูกกำกับไว้ คลิกเลือกแล้วจะมีผลทันที และจะถูกบันทึกลง `~/.config/thclaws/theme.json` (เป็นของผู้ใช้เอง ไม่ถูก commit ไปกับโปรเจกต์) เมนูจะเปิดค้างไว้หลังคลิก เพื่อให้ลองสลับไปมาได้โดยไม่ต้องเปิดเฟืองใหม่

![หน้าต่างหลักของ thClaws ใน dark theme](../user-manual-img/ch-01/main-window-layout-dark.png)

**Light** และ **Dark** เป็น override แบบชัดเจน จะถูกใช้แม้ OS จะตั้งค่าตรงข้ามอยู่ก็ตาม ส่วน **System** จะตามค่า `prefers-color-scheme` และเปลี่ยนตามเมื่อ OS สลับธีม (macOS Appearance, Linux DE theme, Windows personalization) โดยไม่ต้อง restart แอป

### GUI scale (v0.7.3+)

ใต้แถวธีม จะมี **GUI scale** เป็น dropdown สำหรับปรับ zoom WebView สำหรับจอ HiDPI / 4K โดยไม่ต้องเปลี่ยน display scaling ระดับ OS เลือก preset (75–200%) แล้วทั้งแอปจะ scale แบบ live — Chat, Terminal, Files, Settings, sidebar — ใช้ primitive เดียวกับที่ VS Code และ Slack ใช้ ค่าจะ persist per-project ลง `.thclaws/settings.json` เป็น `guiScale: <number>` และโหลดใหม่ทุกครั้งที่เปิดแอป

Use case: laptop จอ 4K ที่ Windows scaling 100% ทำให้ตัวอักษร thClaws เล็กกว่า dev tool อื่น ๆ — ดันเป็น 125% หรือ 150% ก็จะตรงกันโดยไม่กระทบแอปอื่นเลย

ธีมครอบคลุมทุกพื้นผิวของ UI:

- เปลือกของแอป (แท็บ, sidebar, แถบสถานะ, เมนู) — ผ่าน CSS custom properties
- แท็บ Terminal — palette ของ xterm.js สลับสดโดย scrollback ไม่หาย
- เอดิเตอร์/preview ของ CodeMirror — ธีมมืดใช้ `oneDark` ส่วนธีมสว่างใช้ highlighter ค่าเริ่มต้น
- preview markdown ในแท็บ Files — comrak เรนเดอร์ใหม่ให้ palette ตรงกับธีมที่เลือก ฝังเข้าไปใน iframe เลย

## คีย์ลัด

ใช้ได้ทุกที่ในแอป (รวมถึงแท็บ Terminal):

| คีย์ลัด | การทำงาน |
|---|---|
| Cmd/Ctrl+C | คัดลอกส่วนที่เลือก |
| Cmd/Ctrl+X | ตัดส่วนที่เลือก |
| Cmd/Ctrl+V | วางจาก clipboard |
| Cmd/Ctrl+A | เลือกทั้งหมด (ในช่อง text input) |
| Cmd/Ctrl+Z | ย้อนกลับ (ในช่อง text input) |
| Cmd+Q (macOS) | ออกจากแอป |

เฉพาะในแท็บ Terminal:

| คีย์ลัด | การทำงาน |
|---|---|
| Ctrl+C | ล้างบรรทัดถ้าไม่ว่าง ไม่งั้นส่ง SIGINT |
| Ctrl+L | ล้างหน้าจอ |
| Ctrl+U | ลบบรรทัด (มาตรฐาน bash) |

## การ poll ของ sidebar + การอัปเดตข้าม process

Sidebar จะ poll Rust backend ทุก 5 วินาทีเพื่อเช็กการเปลี่ยนแปลง config ดังนั้นถ้าคุณพิมพ์ `/model gpt-4o` ในแท็บ Terminal หน้าแสดง active-model ของแท็บ Chat ก็จะอัปเดตตามภายใน 5 วินาทีโดยไม่ต้อง restart

เมื่อบันทึก API key ผ่าน Settings ทั้ง GUI และ PTY-REPL ลูกจะอ่าน keychain entry ได้ในคำขอถัดไปทันที — ไม่ต้อง restart process ใด ๆ

## การใช้ session ร่วมกัน

แท็บ Terminal และแท็บ Chat **ใช้ session เดียวกัน** ประวัติจะเลื่อนไปพร้อมกัน การ `/save` ในแท็บใดแท็บหนึ่งจะบันทึกให้ทั้งคู่ และเมื่อโหลด session ที่บันทึกไว้จาก sidebar ทั้งสองแท็บจะเปลี่ยนตามไปด้วย

## ค่าต่าง ๆ เก็บไว้ที่ไหน

| อะไร | ที่ไหน |
|---|---|
| ขนาดหน้าต่าง | `.thclaws/settings.json` → `windowWidth` / `windowHeight` |
| working directory ที่ใช้ล่าสุด | `~/.config/thclaws/recent_dirs.json` |
| ตัวเลือก backend สำหรับ secret | `~/.config/thclaws/secrets.json` |
| API key (โหมด keychain) | OS keychain, service `thclaws`, account `api-keys` (JSON blob) |
| API key (โหมด .env) | `~/.config/thclaws/.env` |
| Session | `.thclaws/sessions/` (ผูกกับโปรเจกต์) — ดู[บทที่ 7](ch07-sessions.md) |
| KMS (user) | `~/.config/thclaws/kms/` — ดู[บทที่ 9](ch09-knowledge-bases-kms.md) |
| KMS (project) | `.thclaws/kms/` ใน working directory |
| MCP server (user) | `~/.config/thclaws/mcp.json` |
| MCP server (project) | `.mcp.json` หรือ `.thclaws/mcp.json` |

## เปลี่ยน working directory กลาง session

เมนู Settings → "Change working directory" เปิด modal เลือก folder ใหม่
พอเลือกแล้ว GUI จะ:

1. `cd` process ไป folder ใหม่
2. re-init filesystem sandbox ให้ตรง root ใหม่ (ดู[บทที่ 5](ch05-permissions.md#sandbox-ของ-filesystem))
3. **โหลด `ProjectConfig` จาก `.thclaws/settings.json` ของโปรเจกต์ใหม่** — ถ้า `model` ในไฟล์นั้นต่างจากของเดิม จะ swap provider/agent โดย **เริ่ม session ใหม่** (history ของ provider เก่ามักไม่ตรง schema กับ provider ใหม่ — ปลอดภัยกว่าที่จะเริ่มสด)
4. rebuild system prompt (เพราะ cwd ที่ฝังอยู่เปลี่ยน)
5. broadcast บรรทัดใน Terminal/Chat: `[cwd] /new/path → model: X (was: Y)` ให้คุณรู้ว่า swap จริง

contract ที่บังคับคือ "project ชนะ" — settings ของโปรเจกต์ที่อยู่ใน
folder ใหม่ override ทุกชั้น (user config, env, ค่าที่ session เก่าใช้
ก่อนเปลี่ยน) ทันที ถ้าไม่ต้องการ swap model ให้แน่ใจว่า `.thclaws/settings.json`
ของโปรเจกต์ใหม่ตั้ง `model` ตรงกับของเดิม

## เมื่อไรควรใช้ CLI แทน GUI

ใช้ CLI (`thclaws --cli`) เมื่อคุณต้องการ:

- SSH session หรือ server แบบ headless (ที่ไม่มี webview)
- cold start ที่เร็วกว่า (เพราะไม่ต้อง init webview)
- scripting / piping ด้วยโหมด non-interactive ของ `thclaws -p "prompt"`

ทุกอย่างที่ GUI เปิดให้ใช้ ก็ทำได้ผ่าน slash command ใน CLI — ทั้งสองเป็น UI คู่ขนานบน engine เดียวกัน ไม่ใช่ความสัมพันธ์แบบพ่อ-ลูก

ดูรายละเอียดเรื่อง working directory, โหมดการรัน และ flag ต่าง ๆ ของ command line ได้ใน[บทที่ 3](ch03-working-directory-and-modes.md)
