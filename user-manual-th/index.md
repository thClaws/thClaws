# คู่มือผู้ใช้ thClaws

Workspace สำหรับ AI agent ที่เขียนด้วย Rust แบบ native พร้อมทั้ง CLI และ
desktop GUI คู่มือเล่มนี้ครอบคลุมตั้งแต่การติดตั้ง ไปจนถึงการสร้างและ
deploy โปรเจกต์จริง ไม่ว่าจะเป็นงานเขียนโค้ด งานอัตโนมัติ knowledge base
หรือทีม agent หลายตัว

## ส่วนที่ 1 — การใช้งาน thClaws

| # | บท |
|---|---|
| 1 | [thClaws คืออะไร?](ch01-what-is-thclaws.md) |
| 2 | [การติดตั้ง](ch02-installation.md) |
| 3 | [ทัวร์ Desktop GUI](ch04-desktop-gui-tour.md) |
| 4 | [Working directory และโหมดการรัน](ch03-working-directory-and-modes.md) |
| 5 | [สิทธิ์การใช้งาน (Permissions)](ch05-permissions.md) |
| 6 | [Provider, โมเดล และ API key](ch06-providers-models-api-keys.md) |
| 7 | [Session](ch07-sessions.md) |
| 8 | [Memory และคำสั่งประจำโปรเจกต์ (`CLAUDE.md` / `AGENTS.md`)](ch08-memory-and-agents-md.md) |
| 9 | [Knowledge base (KMS)](ch09-knowledge-bases-kms.md) |
| 10 | [Slash command](ch10-slash-commands.md) |
| 11 | [Tool ที่มีให้ในตัว](ch11-built-in-tools.md) |
| 12 | [Skill](ch12-skills.md) |
| 13 | [Hook](ch13-hooks.md) |
| 14 | [MCP server](ch14-mcp.md) |
| 15 | [Subagent](ch15-subagents.md) |
| 16 | [Plugin](ch16-plugins.md) |
| 17 | [ทีมของ Agent](ch17-agent-teams.md) |
| 19 | [การตั้งเวลา (Scheduling)](ch19-scheduling.md) |
| 20 | [Background research (`/research`)](ch20-research.md) |
| 21 | [LINE chat & web browser bridge](ch21-line-and-browser-chat.md) |
| 22 | [Paperclip adapter](ch22-paperclip-adapter.md) |
| 23 | [Telegram bot](ch23-telegram.md) |
| 24 | [Facebook Page Messenger bot](ch24-messenger.md) |

> **ส่วนที่ 2 — กรณีศึกษา (บทที่ 25–28)** — walkthrough สำหรับสร้าง
> โปรเจกต์จริงด้วย thClaws (เว็บ static, Node.js app, AI agent, การ
> deploy ขึ้น Agentic Press) ยังอยู่ระหว่างพัฒนา จะถูกเพิ่มเข้ามาในคู่มือ
> เมื่อรีวิวและพร้อมเผยแพร่ทีละบท

## ข้อกำหนดการเขียนที่ใช้ในคู่มือเล่มนี้

- `❯` คือ prompt ของ REPL ข้อความที่ตามหลังในบรรทัดเดียวกันคือสิ่งที่ **คุณ** พิมพ์
- `$` คือ shell prompt นอก thClaws
- บรรทัดรูปแบบ `[tool: Bash: …]` / `[tokens: Xin/Yout · Ts]` คือสิ่งที่ thClaws พิมพ์ตอบกลับ
- Code fence ที่ไม่ระบุภาษาคือ output ของ terminal ส่วน fence ที่ระบุภาษา (`rust`, `json`, `bash`) คือไฟล์ที่คุณเขียนเองหรือคำสั่งที่คุณรัน
- **ตัวหนา** ในป้ายกำกับคำสั่งหมายถึง input ที่ต้องระบุ (เช่น **name**)
- ทุกบทอ่านแยกกันได้ จะข้ามไปข้ามมาตามสบายก็ได้
