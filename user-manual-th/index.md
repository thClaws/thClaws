# คู่มือผู้ใช้ thClaws

Workspace สำหรับ AI agent ที่เขียนด้วย Rust แบบ native พร้อมทั้ง CLI และ
desktop GUI คู่มือเล่มนี้ครอบคลุมตั้งแต่การติดตั้ง ไปจนถึงการสร้างและ
deploy โปรเจกต์จริง ไม่ว่าจะเป็นงานเขียนโค้ด งานอัตโนมัติ knowledge base
หรือทีม agent หลายตัว

## ส่วนที่ 1 — การใช้งาน thClaws

เลขบทคงเดิม เพราะมันคือ URL ที่เผยแพร่ไปแล้ว หัวข้อกลุ่มด้านล่างจึงเป็น
*ลำดับการอ่าน* ไม่ใช่การเรียงเลขใหม่ ข้ามไปกลุ่มที่ตรงกับสิ่งที่คุณกำลังจะทำได้เลย

### เริ่มต้นใช้งาน

| # | บท |
|---|---|
| 1 | [thClaws คืออะไร?](ch01-what-is-thclaws.md) |
| 2 | [การติดตั้ง](ch02-installation.md) |
| 3 | [Working directory และโหมดการรัน](ch03-working-directory-and-modes.md) |
| 4 | [ทัวร์ Desktop GUI](ch04-desktop-gui-tour.md) |

### ใช้งานประจำวัน

| # | บท |
|---|---|
| 6 | [Provider, model และ API key](ch06-providers-models-api-keys.md) |
| 7 | [เซสชัน (Sessions)](ch07-sessions.md) |
| 10 | [Slash commands](ch10-slash-commands.md) |
| 11 | [Built-in tools](ch11-built-in-tools.md) |
| 28 | [Browser automation](ch28-browser-automation.md) |

### คุมขอบเขตของ agent

คุณตัดสินใจล่วงหน้าว่า agent ทำอะไรได้บ้าง และอะไรบ้างที่ออกจากเครื่องคุณไป

| # | บท |
|---|---|
| 5 | [สิทธิการใช้งานเครื่องมือ](ch05-permissions.md) |
| 18 | [Plan mode](ch18-plan-mode.md) |
| 32 | [การกลบข้อมูลส่วนบุคคลภาษาไทย](ch32-thai-pii-masking.md) |

### สิ่งที่ agent รู้

| # | บท |
|---|---|
| 8 | [Memory และคำสั่งประจำโปรเจกต์ (`CLAUDE.md` / `AGENTS.md`)](ch08-memory-and-agents-md.md) |
| 9 | [Knowledge bases (KMS)](ch09-knowledge-bases-kms.md) |
| 20 | [Research เบื้องหลัง (`/research`)](ch20-research.md) |

### ต่อยอด agent

| # | บท |
|---|---|
| 12 | [Skills](ch12-skills.md) |
| 13 | [Hooks](ch13-hooks.md) |
| 14 | [MCP servers](ch14-mcp.md) |
| 16 | [Plugins](ch16-plugins.md) |
| 26 | [GUI Shells](ch26-gui-shells.md) |

### ใช้ agent หลายตัว

| # | บท |
|---|---|
| 35 | [agent หลายตัวใน workspace เดียว](ch35-agents-in-a-workspace.md) |
| 15 | [Subagents](ch15-subagents.md) |
| 17 | [Agent Teams](ch17-agent-teams.md) |
| 25 | [Workflows (`/workflow run`)](ch25-workflows.md) |
| 19 | [การตั้งเวลา (Scheduling)](ch19-scheduling.md) |
| 31 | [Loops และ goals (`/loop`, `/goal`)](ch31-loops-and-goals.md) |

### เข้าถึง thClaws จากที่อื่น

| # | บท |
|---|---|
| 21 | [LINE chat และ web browser bridge](ch21-line-and-browser-chat.md) |
| 23 | [Telegram bot](ch23-telegram.md) |
| 24 | [Facebook Page Messenger bot](ch24-messenger.md) |
| 33 | [thClaws Remote](ch33-thclaws-remote.md) |
| 30 | [Job Artifacts (ไฟล์เข้า-ออกสำหรับ orchestrator)](ch30-job-artifacts.md) |

### thClaws.cloud และเครื่องที่องค์กรตั้งค่าให้

| # | บท |
|---|---|
| 27 | [thClaws.cloud (Agent Templates + hosted + gateway)](ch27-thclaws-cloud.md) |
| 34 | [Managed builds และนโยบายองค์กร](ch34-managed-builds.md) |

## ภาคผนวก

| # | ภาคผนวก |
|---|---|
| A | [Provider, โมเดล และราคา (thClaws.cloud gateway)](appendix-a-providers-models-prices.md) |

## ข้อกำหนดการเขียนที่ใช้ในคู่มือเล่มนี้

- `❯` คือ prompt ของ REPL ข้อความที่ตามหลังในบรรทัดเดียวกันคือสิ่งที่ **คุณ** พิมพ์
- `$` คือ shell prompt นอก thClaws
- บรรทัดรูปแบบ `[tool: Bash: …]` / `[tokens: Xin/Yout · Ts]` คือสิ่งที่ thClaws พิมพ์ตอบกลับ
- Code fence ที่ไม่ระบุภาษาคือ output ของ terminal ส่วน fence ที่ระบุภาษา (`rust`, `json`, `bash`) คือไฟล์ที่คุณเขียนเองหรือคำสั่งที่คุณรัน
- **ตัวหนา** ในป้ายกำกับคำสั่งหมายถึง input ที่ต้องระบุ (เช่น **name**)
- ทุกบทอ่านแยกกันได้ จะข้ามไปข้ามมาตามสบายก็ได้
