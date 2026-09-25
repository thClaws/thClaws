# บทที่ 8 — Memory และคำสั่งประจำโปรเจกต์ (Project Instructions)

มีระบบแยกกันสองระบบที่คอยป้อน context ระยะยาวเข้าสู่ system prompt
ของโมเดลตั้งแต่เริ่มต้น

1. **Project instructions** — กฎเกณฑ์คงที่เกี่ยวกับ codebase ที่เขียนครั้งเดียว
   (แล้ว check in ลง git): `CLAUDE.md`, `AGENTS.md`, `.claude/rules/*.md`
2. **Memory** — บันทึกแบบ dynamic ที่ agent เขียนและอ่านระหว่างทำงาน:
   `MEMORY.md` กับไฟล์แยกตามหัวข้อภายใต้ `.thclaws/memory/`

ทั้งสองอย่างจะไปรวมอยู่ใน system prompt agent จึงมองเห็นได้ทุก turn
ถ้าคุมขนาดให้เล็ก ระบบก็จะช่วยให้ความต่อเนื่องข้ามเซสชันดีขึ้น

## Project instructions (`CLAUDE.md` / `AGENTS.md`)

วางไฟล์ชื่อ `CLAUDE.md` หรือ `AGENTS.md` ไว้ที่ root ของโปรเจกต์
เพื่ออธิบายข้อตกลงที่อยากให้ agent ทำตาม

```markdown
# Project conventions

- Language: Rust 2021, `cargo fmt` before every commit.
- Tests live alongside code in `#[cfg(test)]` modules.
- Never touch files under `vendor/`.
- Prefer `anyhow::Result` over `Box<dyn Error>` in application code.
- Commit messages: imperative mood, ≤72 chars in the first line.
```

**รองรับทั้งสองชื่อ**: `AGENTS.md` เป็นมาตรฐาน vendor-neutral
จาก Google / OpenAI / Factory / Sourcegraph / Cursor (ดูแลโดย
Agentic AI Foundation) ส่วน `CLAUDE.md` เป็น convention ดั้งเดิมของ
Claude Code หากมีทั้งคู่ในตำแหน่งเดียวกัน ระบบจะโหลดทั้งสอง
โดยโหลด `CLAUDE.md` ก่อน เพื่อให้การปรับแต่งเฉพาะ vendor ทับลงบน
baseline กลางได้

### แก้ไขผ่าน Settings menu ใน desktop GUI

คลิกไอคอนเฟือง ⚙ ที่มุมขวาล่างเพื่อเปิดเมนู settings — สองรายการแรก
คือทางลัดสำหรับแก้ไข project instructions โดยไม่ต้องเปิด text editor
ภายนอก

![เมนู Settings — Instructions, Settings & API keys, Connect a channel, Appearance, GUI scale และหมวด Workspace](../user-manual-img/ch-04/settings-menu.png)

- **Global instructions** — แก้ไข `~/.config/thclaws/AGENTS.md` ที่เป็น
  baseline ของทุก session บนเครื่องนี้
- **Folder instructions** — แก้ไข `AGENTS.md` ใน working directory ของ
  project ปัจจุบัน

ทั้งสองเปิดใน **WYSIWYG editor** (TipTap) แบบเดียวกัน — พิมพ์เป็น rich
text ปกติ พร้อม heading, bold, italic, code, lists และ link ระบบจะ
แปลง markdown บนดิสก์ ↔ HTML ในเอดิเตอร์ให้อัตโนมัติ (marked → HTML
ตอนโหลด, turndown → markdown ตอน save) ไฟล์บนดิสก์ยังคงเป็น
markdown ปกติที่ Claude Code และ agent ตัวอื่นอ่านได้

![Global instructions editor — รายละเอียด About Me, Communication Style, Coding Preferences ฯลฯ แสดงเป็น WYSIWYG บน modal ขนาดกลาง](../user-manual-img/ch-08/thclaws-global-instructions.png)

Folder instructions ใช้ layout เดียวกันแต่ชี้ไปที่ `AGENTS.md` ใน working
directory ของ project ปัจจุบัน (path จริงแสดงใต้หัว modal ให้เห็นชัด)

![Folder instructions editor — แก้ไข AGENTS.md ของ /Users/jimmy/__2026/thclaws-teams/ โดยเนื้อหาคือ Project conventions](../user-manual-img/ch-08/thclaws-folder-instructions.png)

รายละเอียดการใช้งาน modal:

- **Save** — บันทึกลงดิสก์แล้วปิด modal ให้อัตโนมัติ; agent จะเห็นเนื้อหา
  ใหม่ใน turn ถัดไป (system prompt ถูกสร้างใหม่ทุกครั้งที่ส่ง prompt)
- **Cancel** / Esc / คลิกนอก modal — ปิดโดยไม่บันทึก
- **Cmd/Ctrl + C / V / X** — copy / paste / cut ภายในเอดิเตอร์ทำงานผ่าน
  clipboard bridge ของ wry (เนื่องจาก `navigator.clipboard` ถูกบล็อกใน
  webview)
- **คลิกนอก modal แบบลากเลือกข้อความ** — ถ้าเริ่ม mousedown ใน modal
  แล้วปล่อย mouseup นอก modal จะ **ไม่** ปิด modal (กัน drag-to-select
  ทำให้หายวงการโดยไม่ตั้งใจ)

ถ้าอยากแก้แบบเป็น text ธรรมดาก็ยังทำได้ผ่าน `vim` / VS Code / อะไรก็ได้ —
ไฟล์บนดิสก์เป็น markdown ล้วน thClaws อ่านเนื้อหาใหม่ทุกครั้งที่เริ่ม turn

### thClaws มองหาไฟล์พวกนี้ที่ไหน

โหลดตามลำดับนี้ (รายการที่มาทีหลังจะ refine หรือ override รายการก่อนหน้า)

1. `~/.claude/CLAUDE.md`, `~/.claude/AGENTS.md`,
   `~/.config/thclaws/AGENTS.md`, `~/.config/thclaws/CLAUDE.md` —
   baseline ระดับ user-global
2. เดินขึ้นจาก cwd: `CLAUDE.md` และ `AGENTS.md` ใน ancestor directory
   ทุกระดับ (เริ่มจาก root บนสุดก่อน)
3. project config dir ตามลำดับ
   `.claude/CLAUDE.md`, `.thclaws/CLAUDE.md`, `.thclaws/AGENTS.md`
4. Rules dir — ไฟล์ `.md` ทุกไฟล์เรียงตามตัวอักษร เริ่มจาก
   `.claude/rules/` แล้วตามด้วย `.thclaws/rules/`
5. `CLAUDE.local.md`, `AGENTS.local.md` — เป็นการ override เฉพาะเครื่อง
   โดยทั่วไปใส่ไว้ใน gitignore และมีลำดับความสำคัญสูงสุด

รัน `/context` ใน REPL เพื่อดู system prompt ที่รวมแล้วได้

### อะไรควรอยู่ที่นี่ vs ใน memory

`CLAUDE.md` / `AGENTS.md` เหมาะกับ **เรื่องที่คุณต้องบอกพนักงานใหม่ทุกคน**
เช่น "ใช้ Prisma ไม่ใช่ Drizzle", "API endpoint ให้ไปไว้ใน `api/v2/`",
"log เป็น JSON ห้าม plain text" สิ่งเหล่านี้เป็นของคงที่และอยู่ยาว

Memory เหมาะกับ **สิ่งที่ agent เพิ่งเรียนรู้** เช่น "ผู้ใช้ชอบคำตอบสั้นกระชับ"
หรือ "เรื่อง Stripe webhook ที่ล้มเหลวเมื่อเดือนที่แล้วเป็นบั๊ก clock-skew
ไม่ใช่ signing ไม่ถูกต้อง"

## Memory

Memory อยู่ที่ `.thclaws/memory/`:

```
.thclaws/memory/
├── MEMORY.md              one-line index (what files exist, what they cover)
├── user_preferences.md    what the user likes, disliked approaches, past corrections
├── project_context.md     in-flight work, deadlines, why decisions were made
└── reference_links.md     "bugs are tracked in Linear ENG project", "staging URL is …"
```

### การเขียน memory

agent เขียน memory ได้ผ่าน 3 tool โดยทุกตัวผ่าน permission system
ตามปกติ:

- **`MemoryWrite`** — สร้างหรือแทนที่ entry ประทับ frontmatter
  (`name`, `created`, `updated`) ให้อัตโนมัติ และอัปเดต index ใน
  `MEMORY.md` ให้เอง ขออนุมัติก่อนเขียนเสมอ
- **`MemoryAppend`** — เพิ่มเนื้อหาต่อท้าย entry เดิม พร้อมเลื่อน
  `updated:`
- **`MemoryRead`** — ดึงเนื้อหาเต็มของ entry ที่ system prompt ทำเครื่อง
  หมายว่า `body deferred` (ถูกตัดออกเพื่อให้ prompt อยู่ในงบประมาณ)

ดังนั้นคุณแค่บอกว่า "จำไว้ว่าฉันชอบ TypeScript มากกว่า plain JS" แล้ว
agent จะบันทึกให้ผ่าน permission gate — ไม่ต้องแก้ด้วยมือ คุณยังเปิดไฟล์
`*.md` ใด ๆ ใน `~/.local/share/thclaws/memory/` (หรือ
`./.thclaws/memory/` สำหรับบันทึกแบบ project-scoped) แก้เองได้ agent จะ
อ่านไฟล์เหล่านี้ทุก turn tool ที่เขียนได้เหล่านี้จงใจ bypass filesystem
sandbox เพื่อลงใน memory root ที่ resolve แล้ว (เป็น carve-out แบบ
เดียวกับ `TodoWrite` และ `KmsWrite`) โดยบังคับความปลอดภัยของ path แยก
ต่างหาก

ไฟล์ memory แต่ละไฟล์มี YAML frontmatter

```markdown
---
name: project_context
description: Ongoing context about the Q2 refactor
type: project
---

The billing module rewrite is blocked on legal review of the new
pricing tiers. Target unblock date: 2026-09-15. Contact: Priya.
```

ชนิดที่ thClaws รู้จักได้แก่ `user`, `feedback`, `project`, `reference`
รายการจะอยู่ใน `MEMORY.md` เป็น pointer บรรทัดเดียว ส่วน body เต็ม
ของไฟล์จะถูกโหลดต่อเมื่อ agent ขอมาโดยชัดเจนเท่านั้น (ผ่าน `/memory read NAME`)

### คำสั่ง memory

```
❯ /memory
  user_preferences [user] — what the user likes and dislikes
  project_context [project] — ongoing Q2 refactor notes
  …

❯ /memory read project_context
(prints the full file body)
```

### Memory vs session history

Memory จะคงอยู่ **ข้ามเซสชันและข้ามเครื่อง** (ถ้า check in ลง git)
ส่วนประวัติเซสชันเป็นแบบ per-conversation ซึ่งมีประโยชน์เวลาต้อง resume
thread ที่เจาะจง แต่ไม่ใช่ knowledge base

หลักคร่าว ๆ: ถ้าอีกเดือนหนึ่งข้างหน้ายังเป็นเรื่องจริงอยู่ ของชิ้นนั้นควรอยู่ใน memory
แต่ถ้าจริงเฉพาะตอนนี้สำหรับงานนี้ ของชิ้นนั้นควรอยู่ใน conversation

## งบประมาณขนาด (Size budget)

ทั้ง `CLAUDE.md` / `AGENTS.md` และ memory ต่างเข้าไปอยู่ใน system prompt
จึงกิน token ทุก turn ควรคุมให้อยู่ในช่วงนี้

- `CLAUDE.md` / `AGENTS.md`: ไม่เกิน 1 KB
- `MEMORY.md` (index): ไม่เกิน 500 bytes
- ไฟล์ memory แต่ละหัวข้อ: ไม่เกิน 1 KB

สำหรับ context ที่ใหญ่กว่านี้ ให้ใส่ไว้ในไฟล์ปกติแทน แล้วปล่อยให้ agent
ใช้ `Read` อ่านเอาเมื่อเกี่ยวข้องเท่านั้น

ตัวเลขข้างบนเป็นแนวทาง ไม่ใช่ขีดจำกัด — ไม่มีอะไรถูกตัดทิ้ง มีเกณฑ์จริง
อยู่ข้อเดียวคือ ไฟล์ `CLAUDE.md` หรือ `AGENTS.md` ไฟล์เดียวที่ขนาด
**ตั้งแต่ 40 KB ขึ้นไปจะถูกเตือน** ตอนเริ่มโปรแกรม ไฟล์ยังถูกโหลดครบ
คำเตือนมีไว้เพราะไฟล์ขนาดนั้นเลยจุดที่โมเดลจะอ่านครบทุกบรรทัดอย่างตั้งใจ
แล้ว ควรแยกไฟล์หรือย้ายเนื้อหาส่วนใหญ่ไปไว้ในไฟล์ที่ agent อ่านตอนต้องใช้
