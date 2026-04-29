# บทที่ 10 — Slash commands

Slash commands คือ control plane ของ thClaws พิมพ์ `/` ตามด้วยชื่อ
คำสั่งเพื่อรันคำสั่งนั้น แทนที่จะส่งบรรทัดดังกล่าวให้โมเดล พิมพ์
`/help` ได้ตลอดเวลาเพื่อดูรายการทั้งหมด

> **CLI กับ GUI ใช้ได้เหมือนกัน** ทุกคำสั่งในบทนี้ทำงานเหมือนกันทั้ง
> จาก CLI REPL, แท็บ Terminal ของ GUI และแท็บ Chat ของ GUI —
> input `/<word>` วิ่งผ่าน dispatcher ตัวเดียวกันในทั้งสามที่ คำสั่ง
> ที่ mutate tool state บางตัว (`/mcp add`, `/skill install`,
> `/plugin install`, `/kms use`) ยังเปิดใช้ผลลัพธ์ใน session ปัจจุบัน
> ได้เลยโดยไม่ต้อง restart ด้วย

## ลำดับการตีความคำสั่ง

เมื่อคุณพิมพ์ `/<word>` thClaws จะตีความตามลำดับนี้:

1. **Built-in command** — จากตารางด้านล่าง
2. **Installed skill** — เขียนบรรทัดใหม่เป็นการเรียก `Skill(name: "word")`
   ([บทที่ 12](ch12-skills.md))
3. **Legacy prompt command** — เทมเพลต `.md` จากไดเรกทอรี `commands/`
   โดยแทน `$ARGUMENTS` ด้วยข้อความที่ผู้ใช้ป้อน (อธิบายในบทนี้)
4. **Unknown** — แสดง error สีเหลือง

รายการที่ match เป็นอันดับแรกจะถูกเลือกใช้ ดังนั้น skill จึงไม่สามารถ
บดบัง built-in ได้ เพราะ built-in ถูกตรวจก่อนเสมอ

## เอกสารอ้างอิง Built-in command

### Session และ model

| Command | ทำอะไร |
|---|---|
| `/help` | แสดง built-in commands ทั้งหมด |
| `/model [NAME]` | แสดงโมเดลปัจจุบัน หรือสลับไปใช้ NAME (ตรวจสอบความถูกต้องให้ ถ้าพิมพ์ผิดจะย้อนกลับอัตโนมัติ) |
| `/models` | แสดงรายการโมเดลที่ใช้ได้จาก provider ปัจจุบัน |
| `/models refresh` | ดาวน์โหลด model catalogue (context window ของแต่ละโมเดล) จาก thclaws.ai และอัปเดต cache (ดู[บทที่ 6](ch06-providers-models-api-keys.md)) |
| `/provider [NAME]` | แสดง provider ปัจจุบัน หรือสลับไปใช้ตัวอื่น |
| `/providers` | แสดง provider ทั้งหมดพร้อมโมเดลดีฟอลต์ |
| `/save` | บังคับบันทึก session ปัจจุบันลงดิสก์ |
| `/load ID\|NAME` | โหลด session ด้วย id, id-prefix หรือชื่อเรื่อง |
| `/sessions` | แสดงรายการ session ที่บันทึกไว้ (เรียงจากใหม่สุด) |
| `/rename [NAME]` | เปลี่ยนชื่อ session ปัจจุบัน (หากไม่ใส่ argument จะล้างชื่อเรื่องออก) |
| `/resume ID\|NAME` | (CLI flag `--resume`) เริ่มใหม่พร้อมโหลด session |
| `/clear` | ล้างประวัติในหน่วยความจำ (ไม่แตะไฟล์ที่บันทึกไว้) |
| `/history` | พิมพ์สรุปจำนวนข้อความ |
| `/compact` | ตัดข้อความเก่าออก เขียน checkpoint ลง JSONL เพื่อประหยัด token (auto-run ที่ 80% ของ context window ด้วย) |
| `/fork` | บันทึก session ปัจจุบัน, สรุปประวัติด้วย LLM, เริ่ม session ใหม่ที่ seed ด้วย summary — ใช้ตอนไฟล์ JSONL ใหญ่เกิน 5 MB (ดู[บทที่ 7](ch07-sessions.md)) |
| `/cwd` | แสดง working directory (sandbox root) |

### Memory และ context

| Command | ทำอะไร |
|---|---|
| `/memory` | แสดงรายการ memory entry |
| `/memory read NAME` | พิมพ์เนื้อหา memory entry ออกมา |
| `/context` | แสดงสถิติ context ของ session ปัจจุบัน — จำนวนข้อความ, content block, ขนาด system prompt, token ที่ประเมินว่าใช้ไป, context window ของโมเดล และ progress bar สี % ที่ใช้อยู่ |

### Tools, skills, plugins, MCP

| Command | ทำอะไร |
|---|---|
| `/skills` | แสดงรายการ skill ที่โหลดไว้ |
| `/skill show NAME` | แสดงคำอธิบายเต็มพร้อม path ของ skill |
| `/skill marketplace [--refresh]` | เปิดดู catalog จาก thclaws.ai/api/marketplace.json |
| `/skill search QUERY` | ค้น marketplace catalog แบบ substring |
| `/skill info NAME` | รายละเอียด marketplace ของ skill (license, source, install URL) |
| `/skill install [--user] <name-or-url> [name]` | ติดตั้ง skill — slug ตรง ๆ จะ lookup จาก marketplace, หรือใช้ URL git/`.zip` |
| `/<skill-name> [args]` | เรียกใช้ skill ที่ติดตั้งไว้โดยตรง |
| `/<command-name> [args]` | เรียกใช้ legacy prompt command (template) |
| `/plugins` | แสดงรายการ plugin ที่ติดตั้งไว้ (ทั้งเปิดและปิด) |
| `/plugin install [--user] <url>` | ติดตั้งชุด plugin |
| `/plugin remove [--user] <name>` | ถอนการติดตั้ง plugin |
| `/plugin enable [--user] <name>` | เปิด plugin ที่ปิดอยู่ |
| `/plugin disable [--user] <name>` | ปิดใช้งานโดยไม่ต้องถอนการติดตั้ง |
| `/plugin show <name>` | แสดงรายละเอียด manifest |
| `/mcp` | แสดง MCP server ที่ใช้งานอยู่พร้อม tool ที่มี |
| `/mcp add [--user] <name> <url>` | ลงทะเบียน MCP server ระยะไกล (HTTP) |
| `/mcp remove [--user] <name>` | ลบ MCP server ออกจาก config |

### ฐานความรู้ (KMS)

| Command | ทำอะไร |
|---|---|
| `/kms` (หรือ `/kms list`) | แสดง KMS ทั้งหมดที่ค้นพบ โดยมี `*` กำกับหน้า KMS ที่ผูกกับโปรเจกต์นี้ |
| `/kms new [--project] NAME` | สร้าง KMS ใหม่ (scope ดีฟอลต์คือ user) |
| `/kms use NAME` | ผูก KMS เข้ากับการสนทนาของโปรเจกต์นี้ |
| `/kms off NAME` | ถอด KMS ออก |
| `/kms show NAME` | พิมพ์ `index.md` ของ KMS ออกมา |

แนวคิดและเวิร์กโฟลว์ KMS ฉบับเต็มอยู่ใน [บทที่ 9](ch09-knowledge-bases-kms.md)

### พฤติกรรม Agent

| Command | ทำอะไร |
|---|---|
| `/permissions MODE` | สลับระหว่าง `auto` และ `ask` ระหว่าง session |
| `/thinking BUDGET` | กำหนด budget token สำหรับ extended-thinking (0 = ปิด ใช้ได้เฉพาะ Anthropic) |
| `/tasks` | แสดง task / todo ที่ agent สร้างไว้ |
| `/config key=val` | เขียนทับค่า config เฉพาะ session นี้ |
| `/team` | เข้าร่วม tmux session ของทีม (หรือแสดงสถานะทีม) |
| `/doctor` | รันการตรวจสอบวินิจฉัย |
| `/usage` | แสดงการใช้ token แยกตาม provider และ model |
| `/version` | แสดงเวอร์ชัน thClaws และ commit SHA |
| `/quit` | ออกจากโปรแกรม |

### Shell escape

| Command | ทำอะไร |
|---|---|
| `! <command>` | รัน `<command>` ใน terminal โดยตรง ข้าม agent |

เหมาะสำหรับตรวจสอบเร็ว ๆ (`! ls`, `! git status`) โดยไม่เปลือง
model token

## ทางลัดของ skill และ command

skill ที่ติดตั้งไว้ทุกตัวเรียกใช้ได้ผ่าน `/<skill-name>`:

```
❯ /skills
  docx — Create, read, edit Word documents
  pdf  — Read, split, merge, OCR PDFs
  …

❯ /pdf extract text from report.pdf
(/pdf → Skill(name: "pdf"))
Using the pdf skill to extract text from report.pdf…
```

Legacy prompt command เก็บในรูปไฟล์ markdown:

```markdown
# .thclaws/commands/review.md
---
description: Code review a branch
---
Review the diff from `main` to HEAD. Flag security issues, bad naming,
and missing tests. Focus on $ARGUMENTS.
```

```
❯ /review authentication
(/review → prompt from .thclaws/commands/review.md)
Reviewing the diff, focused on authentication…
```

`$ARGUMENTS` จะถูกแทนด้วยข้อความที่ตามหลังชื่อคำสั่ง หาก template
ไม่มี placeholder แต่ผู้ใช้พิมพ์ args มา ข้อความนั้นจะถูกต่อท้ายใน
บรรทัดว่าง

## เขียน slash command ของคุณเอง

สำหรับคำสั่งบรรทัดเดียว ให้ใส่ไฟล์ `.md` ลงใน `.thclaws/commands/`
หากต้องมี script หรือ scaffolding ให้ทำเป็น **skill** ([บทที่ 12](ch12-skills.md))
แต่ถ้าเป็นชุดรวม (skill + command + MCP) ให้ส่งเป็น
**plugin** ([บทที่ 16](ch16-plugins.md))
