# บทที่ 16 — Plugins

Plugin คือ **ชุดรวม** ของ skill + legacy command + agent definition +
MCP server ที่ถูกจัดการเป็นก้อนเดียว ติดตั้งทีเดียวได้ของครบทุกอย่าง

Plugin คือคำตอบแบบเบ็ดเสร็จของโจทย์ที่ว่า "อยากส่งโฟลเดอร์ที่รวม
ส่วนขยายของ agent ทั้งทีมให้เพื่อนร่วมทีม (หรือเครื่องอื่น) แล้วให้
ใช้งานได้ทันที"

## Manifest

ทุก plugin จะมี manifest อยู่ที่ราก โดยระบบจะค้นหาตามลำดับนี้

1. `.thclaws-plugin/plugin.json` (thClaws-native, ทางเลือกที่แนะนำ)
2. `.claude-plugin/plugin.json` (fallback สำหรับความเข้ากันได้กับ Claude Code)

รูปแบบ

```json
{
  "name": "agentic-press-deploy",
  "version": "1.0.0",
  "description": "Deploy apps to Agentic Press Hosting",
  "author": "Agentic Press",
  "skills": ["skills"],
  "commands": ["commands"],
  "agents": ["agents"],
  "mcpServers": {
    "deploy-hub": {
      "transport": "http",
      "url": "https://api.agentic.cloud/mcp"
    }
  }
}
```

path ทั้งหมดอ้างอิงแบบ relative จากรากของ manifest โดยแต่ละฟิลด์มีหน้าที่ดังนี้

| ฟิลด์ | ชี้ไปยัง | แต่ละ entry คือ |
|---|---|---|
| `skills` | ไดเรกทอรีของ subdir `<name>/SKILL.md` | skill-catalog directory ([บทที่ 12](ch12-skills.md)) |
| `commands` | ไดเรกทอรีของ prompt template `*.md` | commands directory ([บทที่ 10](ch10-slash-commands.md#resolution-order)) |
| `agents` | ไดเรกทอรีของ agent def `*.md` | agent-catalog directory ([บทที่ 15](ch15-subagents.md)) |
| `mcpServers` | JSON map ของ server config | รูปแบบเดียวกับ `mcp.json` ([บทที่ 14](ch14-mcp.md)) |

## โครงสร้างไดเรกทอรีของ plugin ที่ติดตั้งแล้ว

Plugin ที่ติดตั้งแล้วจะอยู่ภายใต้ path ต่อไปนี้

| Scope | รากการติดตั้ง | ไฟล์ registry |
|---|---|---|
| Project | `.thclaws/plugins/<name>/` | `.thclaws/plugins.json` |
| User | `~/.config/thclaws/plugins/<name>/` | `~/.config/thclaws/plugins.json` |

Registry คือ JSON array ที่เก็บชื่อ URL ต้นทาง path การติดตั้ง
เวอร์ชัน และ enabled flag ของ plugin แต่ละตัว

## Marketplace

`/plugin marketplace` เปิดดู catalog ที่ดูแลอยู่ที่
[`thClaws/marketplace`](https://github.com/thClaws/marketplace)
รูปแบบเดียวกับ skill marketplace — สามคำสั่งสำรวจ + install ด้วยชื่อ:

```
❯ /plugin marketplace
plugin marketplace (baseline 2026-04-29, 1 plugin(s))
── workflow ──
  productivity             — Task management, workplace memory, visual dashboard
install with: /plugin install <name>   |   detail: /plugin info <name>
```

```
❯ /plugin info productivity
license:      Apache-2.0 (open)
install with: /plugin install productivity (resolves to https://github.com/thClaws/marketplace.git#main:plugins/productivity)
```

```
❯ /plugin install productivity
plugin 'productivity' installed (project, 1 skill dir(s)) → .thclaws/plugins/productivity
skills callable in this session — no restart needed
```

ใช้ `/plugin show <name>` สำหรับดู plugin ที่ติดตั้งแล้ว (path,
contributions, scope) — `/plugin info` ของ marketplace ใช้ดู entry
ใน catalog ก่อนติดตั้ง

## การติดตั้ง (จาก URL ที่กำหนดเอง)

สำหรับ plugin ที่ไม่อยู่ใน marketplace:

```
❯ /plugin install https://github.com/agentic-press/deploy-plugin.git
plugin 'agentic-press-deploy' installed (project) → .thclaws/plugins/agentic-press-deploy
Skills refreshed and callable this session.
1 plugin-contributed MCP server(s) still need a restart to spawn — or use /mcp add to register them now.
```

extension `<git-url>#<branch>:<subpath>` ใช้กับ plugin ได้ด้วย
(เหมือน skill) — มีประโยชน์เมื่อ repo upstream รวมหลาย plugin ไว้
ใต้ directory `plugins/`:

```
❯ /plugin install https://github.com/anthropics/knowledge-work-plugins.git#main:productivity
```

skill ที่ plugin contribute จะถูกเปิดใช้ทันทีใน session ปัจจุบัน —
live store ของ SkillTool ถูก refresh และ skill ใหม่โผล่ใน system
prompt ส่วน `# Available skills` ให้เห็น ส่วน MCP server ที่ plugin
contribute ยังต้องใช้ `/mcp add` เพื่อลงทะเบียน live หรือ restart
(ยังไม่มีตัว auto-spawn เพราะต้อง diff กับ server ที่รันอยู่แล้ว)
command และ agent definition อยู่ในไดเรกทอรีที่ถูก scan ใหม่ตอนใช้งาน
จึงใช้งานได้ทันทีเหมือนกัน

จากไฟล์ `.zip`

```
❯ /plugin install --user https://agentic-press.com/plugins/deploy-v1.zip
```

การเลือก scope

- default (ไม่ระบุ flag) → ติดตั้งระดับ project (`.thclaws/plugins/`)
- `--user` → ติดตั้งระดับ user global (`~/.config/thclaws/plugins/`)

## List / show / enable / disable / remove

```
❯ /plugins
  agentic-press-deploy v1.0.0 (enabled) → .thclaws/plugins/agentic-press-deploy
    source: https://github.com/agentic-press/deploy-plugin.git
  big-noisy-plugin v0.2.3 (disabled) → .thclaws/plugins/big-noisy-plugin

❯ /plugin show agentic-press-deploy
  agentic-press-deploy v1.0.0 (enabled)
  path: .thclaws/plugins/agentic-press-deploy
  source: https://github.com/agentic-press/deploy-plugin.git
  description: Deploy apps to Agentic Press Hosting
  author: Agentic Press
  skill dirs: skills
  command dirs: commands
  agent dirs: agents
  mcp servers: deploy-hub

❯ /plugin disable big-noisy-plugin
plugin 'big-noisy-plugin' disabled (restart to drop its contributions)

❯ /plugin enable big-noisy-plugin
plugin 'big-noisy-plugin' enabled (restart to pick up its contributions)

❯ /plugin remove big-noisy-plugin
plugin 'big-noisy-plugin' removed (restart to drop active tools)
```

Disable ไม่เหมือน remove เพราะไฟล์ยังคงอยู่บนดิสก์ แค่ `enabled` flag ถูกสลับเท่านั้น

## Plugin มีส่วนร่วมอะไรบ้าง

เมื่อเริ่มต้นครั้งถัดไป กระบวนการ discovery ของ thClaws จะเดินสำรวจทั้ง
ไดเรกทอรีมาตรฐาน **และ** ไดเรกทอรีที่แต่ละ plugin ที่ enabled ประกาศไว้

- **Skills** จาก plugin จะปรากฏเคียงข้างกับ skill ที่อยู่ใน project
- **Commands** ก็เช่นเดียวกัน
- **Agents** (อาร์เรย์ `agents`) จะรวมเข้ากับ catalogue ของ agent-def
  แบบ additive โดย agent ของ plugin ไม่สามารถบดบัง agent ที่ชื่อซ้ำกัน
  ในระดับ user หรือ project ได้ เหมาะสำหรับการส่งทีม agent
  เฉพาะทาง (เช่น `reviewer`, `tester`, `architect`) รวมเป็นชุดติดตั้งเดียว
- **MCP servers** ใน manifest จะถูก merge เข้าไปที่
  `config.mcp_servers` โดย entry ใน `mcp.json` ระดับ project จะชนะเสมอ
  เมื่อชื่อซ้ำกัน

เมื่อเกิดการชนกัน กฎจะเหมือนกันทุกที่ คือของ **คุณ** ชนะของ **plugin**
เสมอ ไม่ว่าจะเป็น skill, command, agent หรือ MCP server ที่คุณกำหนด
ในระดับ project หรือ user จะ override ของ plugin ที่ชื่อเดียวกันเสมอ ดังนั้น
การติดตั้ง plugin จึงไม่ไปเปลี่ยนสัญญาที่คุณ commit ไว้แล้วโดยไม่รู้ตัว

## การเขียน plugin

ตัวอย่างขั้นต่ำ ประกอบด้วย skill หนึ่งชุด command หนึ่งตัว และ agent def หนึ่งตัว

```
my-plugin/
├── .thclaws-plugin/
│   └── plugin.json
├── skills/
│   └── hello/
│       └── SKILL.md
├── commands/
│   └── greet.md
└── agents/
    └── reviewer.md
```

```json
{
  "name": "my-plugin",
  "version": "0.1.0",
  "description": "Say hello in style + a reviewer agent",
  "skills": ["skills"],
  "commands": ["commands"],
  "agents": ["agents"]
}
```

ตัวอย่าง `agents/reviewer.md`

```markdown
---
name: reviewer
description: Read-only code review focused on naming + security
model: claude-haiku-4-5
tools: Read, Glob, Grep
permissionMode: auto
---

You are a code reviewer. Read the files you're pointed at. Flag
naming inconsistencies, missing tests, and security-sensitive
patterns. Don't propose fixes unless asked.
```

zip รวมเข้าด้วยกัน

```bash
cd my-plugin
zip -r ../my-plugin.zip .thclaws-plugin skills commands agents
```

โฮสต์ไฟล์ zip ไว้ที่ไหนก็ได้ที่เข้าถึงผ่าน HTTPS ได้ (เช่น CDN ของคุณ S3
หรือ GitHub Releases) แล้วแชร์ URL นั้น หรือจะ push เข้า git repo ก็ได้
เพราะ `/plugin install <git-url>` ทำงานได้เหมือนกันทุกประการ

## เทียบกับ skill + `/mcp add`

ถ้าติดตั้งแต่ละชิ้นแยกกันก็ได้ฟังก์ชันที่เทียบเท่ากันเหมือนกัน
แต่จุดเด่นของ plugin คือความ **เป็นหนึ่งเดียว** คือ ติดตั้งครั้งเดียว ถอนครั้งเดียว
pin เวอร์ชันเดียว ส่งให้ทีมใช้ชุดเดียว ทุกคนได้เหมือนกันหมด

## ฟีเจอร์ที่เลื่อนไว้

ฟีเจอร์ต่อไปนี้ยังไม่รองรับ แต่อยู่ใน roadmap

- **Hook merging** — บล็อก `hooks` ใน manifest ที่จะ apply เข้ากับ
  runtime hooks config ([บทที่ 13](ch13-hooks.md))
- **Marketplace** — `/plugin search`, `/plugin browse`

ช่วงนี้จึงต้องจัดการเองด้วยการแยกไฟล์ไปก่อน
