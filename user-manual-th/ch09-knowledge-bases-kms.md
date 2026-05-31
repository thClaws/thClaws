# บทที่ 9 — Knowledge bases (KMS)

**knowledge base** (KMS — Knowledge Management System) คือโฟลเดอร์ของ markdown page ที่คุณดูแลเอง พร้อมกับ `index.md` ที่ทำหน้าที่เป็นสารบัญซึ่ง agent อ่านทุก turn แนวคิดนี้ได้แรงบันดาลใจมาจาก [LLM wiki pattern](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f) ของ Andrej Karpathy โดย thClaws ใส่ KMS มาให้ในตัวอยู่แล้ว ไม่มี embeddings ไม่มี vector store มีแค่ grep กับ read

Use case:

- **บันทึกส่วนตัว** — ทุกสิ่งที่คุณเรียนรู้เกี่ยวกับ API, library หรือ codebase ของลูกค้า
- **เอกสารอ้างอิงของโปรเจกต์** — architectural decision, design principle และ pattern ที่เฉพาะเจาะจงกับ repo นั้น ๆ
- **Playbook ของทีม** — standard operating procedure หรือ checklist สำหรับ onboarding
- **เนื้อหาเฉพาะภาษา** — รองรับภาษาไทยได้ทันทีตั้งแต่เริ่ม เพราะการค้นหาทำงานผ่าน Grep เป็นหลัก

## แตกต่างจาก memory หรือ AGENTS.md อย่างไร

| | ขอบเขต | ขนาด | การค้นข้อมูล |
|---|---|---|---|
| **AGENTS.md** | inject ข้อความเต็มทุก turn | เล็ก (ไม่กี่ KB) | ไม่ต้องค้น เพราะอยู่ใน prompt อยู่แล้ว |
| **Memory** | ข้อเท็จจริงแยกตามชนิด | เล็ก (index + body refs) | frontmatter ทำ index ไว้ให้ แล้วค่อยดึง body เมื่อจำเป็น |
| **KMS** | wiki ทั้งชุด โหลดแบบ lazy | ไม่จำกัด (เป็นพัน page ก็ไหว) | ใช้ Grep ค้น แล้วอ่านเฉพาะ page ที่ต้องการ |

หลักคร่าว ๆ คือ memory ไว้เก็บเรื่องเกี่ยวกับ *ตัวคุณ* และ *วิธีทำงานของคุณ* ส่วน AGENTS.md ไว้เก็บ convention ของโปรเจกต์ ขณะที่ KMS ไว้เก็บ *เนื้อหา* ที่ agent จะเข้าไปเปิดดู

## Scope

มีสอง scope ที่มีโครงสร้างภายในเหมือนกัน

- **User** — `~/.config/thclaws/kms/<name>/` — ใช้ได้ในทุกโปรเจกต์
- **Project** — `.thclaws/kms/<name>/` — อยู่กับ repo และตามไปกับ git ถ้าถูก track ไว้

หากมีชื่อซ้ำกันทั้งสอง scope ฝั่ง **project** จะถูกเลือกใช้ก่อน

## Layout ของ KMS directory

```
<kms_root>/
├── index.md       ← table of contents, one line per page. The agent reads this every turn.
├── log.md         ← append-only change log (humans + agent write here)
├── SCHEMA.md      ← optional: prose shape rules for pages
├── manifest.json  ← schema version + optional frontmatter requirements (see "Schema versioning")
├── pages/         ← individual wiki pages, one per topic
│   ├── auth-flow.md
│   ├── api-conventions.md
│   └── troubleshooting.md
└── sources/       ← raw source material (URLs, PDFs, notes) — optional
```

`/kms new` จะสร้างทุกอย่างข้างบนให้พร้อมเนื้อหา starter เล็ก ๆ เพื่อให้คุณเริ่มเขียนต่อได้ทันที

## Canonical page shape

ทุกหน้าจะถูก write ผ่าน `KmsWrite` ซึ่งคาดหวัง YAML frontmatter ในรูปแบบนี้ แล้วจะ inject header ที่เป็นมาตรฐานทับด้านบนของ body:

```yaml
---
title: ชื่อหน้าที่อ่านออก       # ใส่ไม่ครบ → fallback ใช้ชื่อไฟล์
topic: บรรยายในบรรทัดเดียว     # render เป็น Description: …; ขาดบรรทัดนี้ก็ถูก omit
sources: ["https://…", "memory"]  # **บังคับ** — provenance (URLs, session-XYZ, memory, หรือ [] สำหรับ opinion)
category: หมวดหมู่ (optional)
tags: [optional, free-form]
---

(เนื้อหา body — KmsWrite จะ inject # title / Description / --- block เองถ้า body ไม่ขึ้นต้นด้วย # heading)
```

หลังเขียนเสร็จ shape จริงบน disk จะเป็น:

```
---
title: …
topic: …
sources: […]
created: 2026-05-11
updated: 2026-05-11
verified: 2026-05-11                  # stamp เฉพาะตอน /research เขียน; manual KmsWrite ไม่ใส่
---

# {title}
Description: {topic}
---

(body)
```

**Provenance discipline** — `sources:` เป็นคำตอบของ critique "LLM-Wiki ฝัง hallucination ถาวร". `KmsWrite` warn เมื่อ frontmatter มีอยู่แต่ขาด `sources:` และ `KmsRead` ครั้งถัดไปจะ prepend banner `[note: this page has no verification record]`. หน้าที่เป็น opinion / convention ไม่มี external source → ใส่ `sources: []` ชัดเจน (เป็นการ ack ไม่ใช่ขาด)

**Freshness** — หน้าที่ `verified:` เก่ากว่า 90 วันจะได้ banner `[note: this page was last verified N days ago — sources may have drifted; re-verify before citing as current fact]` ทุกครั้งที่ `KmsRead`. `/research` stamp `verified: today` ให้ทุกหน้าที่เขียน; manual `KmsWrite` ก็ใส่ได้เองเมื่อได้ verify จริง

**ไฟล์เก่าไม่ migrate อัตโนมัติ** — หน้าที่มีอยู่ก่อนหน้านี้ที่ไม่มี canonical header จะคงรูปเดิมจนกว่าจะถูก rewrite ผ่าน `KmsWrite` (เช่น `/dream` consolidation, `/kms reconcile`, หรือเขียนใหม่ด้วยมือ)

## การเพิ่มเนื้อหา: capture และ ingest

มีสามวิธีที่เพิ่มเนื้อหาเข้า KMS เรียงจาก "ให้ agent คิดเอง" ไปจน "บอก agent ตรง ๆ ว่าต้องทำอะไร" เลือกตามจังหวะที่คุณอยู่

### ภาษาธรรมชาติ

แค่บอก agent มันก็เขียน markdown แบบเดียวกับที่เขียนไฟล์อื่น ๆ:

```
❯ I just read https://example.com/oauth-guide. Ingest the key points into 'notes'.

[assistant] Reading the page…
[tool: WebFetch(url: "https://example.com/oauth-guide")]
[tool: Write(path: "~/.config/thclaws/kms/notes/pages/oauth-client-credentials.md", ...)]
[tool: Edit(path: "~/.config/thclaws/kms/notes/index.md", ...)]
[tool: Edit(path: "~/.config/thclaws/kms/notes/log.md", ...)]
Wrote pages/oauth-client-credentials.md, added entry to index.md, appended to log.md.
```

ใช้ได้กับทุกอย่าง — บทความ, screenshot, transcript, task agent หาที่วางและเขียน page, index entry, log entry ให้เอง

### Slash command สำหรับเคสที่เจอบ่อย

เมื่อ source มีรูปแบบตายตัว slash command ช่วยให้ไม่ต้อง prompt-engineering แต่ละตัวมีหัวข้อของตัวเองด้านล่าง — แผนคร่าว ๆ ที่นี่:

- **`/kms ingest NAME <file-or-url-or-$>`** — ดึงไฟล์, URL, PDF หรือ chat session ปัจจุบันเข้า KMS เป็น stub page
- **`/kms dump NAME <text>`** — paste เนื้อหา freeform; agent แบ่งเป็น chunk แล้ว route แต่ละชิ้นไปที่ที่ใช่
- **`/kms file-answer NAME <title>`** — file ข้อความล่าสุดของ assistant เป็น page ใหม่

### Workflow สามขั้นตอนของ Karpathy

แนวคิดเบื้องหลังทั้งหมด:

1. **Ingest** — อ่านแหล่งข้อมูล สกัดข้อเท็จจริง เขียน page, อัปเดต index, append log
2. **Query** — ตอบคำถามจาก wiki (agent ทำให้เองเมื่อมี KMS ผูกอยู่)
3. **Lint** — เป็นระยะ ๆ อ่านทุก page แล้ว flag ว่าต้อง merge / split / orphan ไหน

ทำได้ผ่านภาษาธรรมชาติทั้งหมด slash command คือ shortcut

## Self-improving AI Agent (auto-learn)

ถ้าอยากให้ agent **เรียนรู้จากตัวเอง** อัตโนมัติทุก session โดยไม่ต้องสั่ง
`/kms ingest` หรือ `/kms reconcile` ด้วยตัวเอง เปิด flag เดียวใน
`.thclaws/settings.json`:

```json
{
  "autoLearn": true
}
```

เมื่อ flag นี้ on:

1. **ปลายทุก session** (กดปุ่ม "new session" หรือปิด GUI) — thClaws
   จะสรุปบทสนทนานั้นเป็น KMS page ใหม่ใน KMS ชื่อ `self_learn`
   (สร้างให้อัตโนมัติครั้งแรก scope = project)
2. **ตามรอบเวลา (default ทุก 6 ชั่วโมง)** — หลัง ingest เสร็จ จะรัน
   `/kms reconcile self_learn --apply` แก้ contradictions ระหว่าง
   page ใน KMS นั้น

นั่นแค่นั้น — ใช้ปริมิทีฟที่เรียนรู้ไปแล้วในบทนี้ (`/kms ingest $`,
`/kms reconcile`) อัตโนมัติ ไม่มี agent ใหม่ ไม่มี prompt ใหม่

### ทำไม `self_learn` KMS เป็น KMS แยก

auto-learn ไม่แตะ KMS ที่คุณ curate เอง (`notes`, `client-api`,
อะไรก็ตามที่อยู่ใน `kms.active`) — เขียนเฉพาะ `self_learn` เท่านั้น
เหตุผล:

- **คุมเสียงรบกวน** — session บางอันไม่ได้มี insight ทุกครั้ง ไม่อยากให้
  KMS หลักโดน pollute
- **reset ง่าย** — เลิกชอบสิ่งที่ agent เรียนรู้? `rm -rf
  .thclaws/kms/self_learn/` แล้วเริ่มใหม่
- **review แยกได้** — `git diff .thclaws/kms/self_learn/` ดูเฉพาะที่
  agent เรียนรู้จากตัวเอง, `git diff .thclaws/kms/notes/` ดูเฉพาะที่
  คุณ curate เอง

### Setting เพิ่มเติม

| key | default | meaning |
|---|---|---|
| `autoLearn` | `false` | สวิตช์หลัก (opt-in) |
| `autoLearnKms` | `"self_learn"` | เปลี่ยนชื่อ KMS ปลายทางได้ ถ้ามี KMS ชื่อนั้นอยู่แล้วใช้ตัวเดิม |
| `autoLearnReconcileHours` | `6` | ระยะห่างขั้นต่ำระหว่าง reconcile (เซ็ต `0` = reconcile ทุก session) |

### Quality gate

session ที่สั้นกว่า **5 messages** จะถูกข้าม (ไม่ใช่ทุกการเปิด-ปิด app
มี insight) คุณเห็น log ของแต่ละครั้งที่
`~/.config/thclaws/auto-learn.log`:

```
2026-05-20T08:15:00Z ingest ok: session=sess-abc123 kms=self_learn page=auth-jwt-design
2026-05-20T08:15:42Z reconcile ok: kms=self_learn (next due in 6h)
2026-05-20T09:02:11Z skip ingest: session sess-def456 only had 3 messages (threshold 5)
```

### ใช้ได้ที่ไหน

ตอนนี้ (v0.13.0) — **Desktop GUI** และ **Webapp** (`--serve` + browser)
ทั้งสองใช้ worker เดียวกันที่ดูแล lifecycle ของ session ส่วน CLI REPL
และ print mode (`-p`) ยังไม่ trigger อัตโนมัติ — ใช้ `session_end`
hook ใน settings ผูกเองได้

ดูบทถัด ๆ ไปสำหรับ `session_end` hook ใน [บทที่ 13](ch13-hooks.md)

## Multi-KMS: ผูก KMS ชุดใดก็ได้เข้ากับการสนทนา

รายการ KMS ที่ active ของโปรเจกต์อยู่ใน `.thclaws/settings.json`:

```json
{
  "kms": {
    "active": ["notes", "client-api", "team-playbook"]
  }
}
```

`index.md` ของ KMS ที่ active ทุกตัวจะถูกนำมาต่อกันใน system prompt ภายใต้หัวข้อ `## KMS: <name>` พร้อม pointer ชี้ไปยัง tool `KmsRead` และ `KmsSearch` สิ่งที่ agent เห็นจะมีหน้าตาแบบนี้

```
# Active knowledge bases

The following KMS are attached to this conversation. Their indices are below —
consult them before answering when the user's question overlaps.

## KMS: notes (user)

# notes
- auth-flow → pages/auth-flow.md — JWT refresh pattern we use
- api-conventions → pages/api-conventions.md — REST style guide

To read a specific page, call `KmsRead(kms: "notes", page: "<page>")`.
To grep all pages, call `KmsSearch(kms: "notes", pattern: "...")`.
```

พร้อมทั้งลงทะเบียน `KmsRead` / `KmsSearch` (และ `KmsWrite` / `KmsAppend` / `KmsDelete` ที่ mutate ได้) ไว้ในรายการ tool ให้ด้วย **slash command หลายตัวด้านล่างต้องการ KMS ที่ active อย่างน้อยหนึ่งตัว** — ถ้าไม่มี KMS active เลย tools ของ KMS จะไม่ถูก register เข้า registry และ agent จะแอ็กเซส KMS ใด ๆ ทาง name ไม่ได้

## Slash commands

surface เต็ม จัดกลุ่มตาม purpose

- **ค้นและตรวจสอบ**: `/kms`, `/kms show`
- **Lifecycle**: `/kms new`, `/kms use`, `/kms off`
- **Capture**: `/kms ingest`, `/kms dump`, `/kms file-answer`
- **Maintenance**: `/kms lint`, `/kms wrap-up`, `/kms reconcile`, `/kms migrate`
- **Cross-link**: `/kms link`
- **รวม KMS**: `/kms merge`
- **Decision support**: `/kms challenge`
- **ลบ**: `/kms drop`

subcommand ส่วนใหญ่รับ alias สั้น ๆ (เช่น `add` แทน `ingest`,
`rm` แทน `drop`) — alias จะระบุไว้ใต้ header ของแต่ละ section ด้านล่าง

### `/kms` (หรือ `/kms list`)

แสดงรายการ KMS ทั้งหมดที่ค้นพบ โดยมี `*` กำกับไว้หน้าตัวที่ผูกกับโปรเจกต์ปัจจุบัน

```
❯ /kms
* notes              (user)
  client-api         (project)
* team-playbook      (user)
  archived-docs      (user)
(* = attached to this project; toggle with /kms use | /kms off)
```

### `/kms show NAME`

พิมพ์ `index.md` ของ KMS ออกมาให้ดูว่ามีอะไรบ้าง Aliases: `cat`

```
❯ /kms show notes
# notes
- auth-flow → pages/auth-flow.md — JWT refresh pattern we use
- api-conventions → pages/api-conventions.md — REST style guide
...
```

### `/kms new [--project] NAME`

สร้าง KMS ใหม่พร้อมไฟล์ starter ให้ในตัว (รวมถึง `manifest.json`) Aliases: `create`

```
❯ /kms new meeting-notes
created KMS 'meeting-notes' (user) → /Users/you/.config/thclaws/kms/meeting-notes

❯ /kms new --project design-decisions
created KMS 'design-decisions' (project) → ./.thclaws/kms/design-decisions
```

- scope ดีฟอลต์คือ **user** (ใช้ได้ในทุกโปรเจกต์)
- ใส่ `--project` เพื่อให้ไปอยู่ใน `.thclaws/kms/` (ติดไปกับ repo)

### `/kms use NAME`

ผูก KMS เข้ากับโปรเจกต์ปัจจุบัน ระบบจะลงทะเบียน tool `KmsRead` / `KmsSearch` / `KmsWrite` / `KmsAppend` / `KmsDelete` เข้า session ทันที พร้อมแทรก `index.md` เข้า system prompt — ไม่ต้อง restart ใช้ได้ทั้ง CLI REPL และ GUI ทั้งสองแท็บ Aliases: `on`

```
❯ /kms use notes
KMS 'notes' attached (tools registered; available this turn)
```

### `/kms off NAME`

ถอด KMS ออก มีผลทันทีเช่นกัน — เมื่อถอด KMS ตัวสุดท้ายออก tool ของ KMS จะถูกลบจาก registry เพื่อไม่ให้ model เห็นเป็นทางเลือก Aliases: `unuse`

```
❯ /kms off archived-docs
KMS 'archived-docs' detached (system prompt updated)
```

### `/kms ingest NAME <file-or-url-or-$>`

เพิ่ม source เข้า KMS ระบบจับชนิดของ source อัตโนมัติแล้ว route ไปยัง ingest path ที่ถูกต้อง split สองขั้น: bytes ดิบไปอยู่ที่ `sources/<alias>.<ext>` (immutable), stub page ไปอยู่ที่ `pages/<alias>.md` พร้อม frontmatter ที่ชี้ย้อนกลับไปที่ source จากนั้นคุณ enrich stub ผ่าน prompting ตามปกติหรือ `/kms ingest --force` อีกที Aliases: `add`

| รูปแบบ source | สิ่งที่ระบบทำ |
|---|---|
| `<file.md>` / `.txt` / `.json` / `.rst` / `.log` / `.markdown` | text ธรรมดา — copy bytes, write stub |
| `<file.pdf>` | run `pdftotext` ก่อน (ต้องติดตั้ง `poppler-utils`) แล้วค่อย ingest |
| `https://...` URL | HTTP fetch (timeout 30s); response body ได้ banner `<!-- fetched from <url> on <date> -->` ก่อน ingest |
| `$` | พิเศษ — "chat session ปัจจุบัน" trigger agent turn ที่สรุป conversation เป็น wiki page (200–1500 คำ, สังเคราะห์) แล้วเรียก `KmsWrite` ชื่อ page จาก `session.title` (sanitize) ถ้ามี ไม่งั้น `session.id` (`sess-<hex>`) — ดูด้านล่าง |

flag เสริม:

- `as <alias>` — override page stem ที่ระบบ derive ให้ ใช้เมื่อชื่อไฟล์หรือ URL ผลิต stem หน้าตาน่าเกลียด
- `--force` — แทนที่ page ที่มีอยู่แล้วของ alias เดียวกัน และ mark ทุก page ที่ frontmatter `sources:` reference alias นี้ด้วย marker `> ⚠ STALE` (**re-ingest cascade**) page ที่ flag STALE ต้อง refresh ตามเนื้อหา source ใหม่; `/kms wrap-up` จะ surface ขึ้นมาให้

```
❯ /kms ingest notes ~/Downloads/oauth-spec.pdf
ingested oauth-spec → pages/oauth-spec.md (12 KB extracted)

❯ /kms ingest notes https://example.com/articles/best-practices.html as best-practices
ingested best-practices → pages/best-practices.md (4.2 KB)

❯ /kms ingest notes ~/Downloads/updated-spec.pdf as oauth-spec --force
re-ingested oauth-spec; marked 3 dependent page(s) stale
```

สำหรับ paste หลายย่อหน้าโดยไม่มี source file เฉพาะ `/kms dump` เหมาะกว่า

#### `/kms ingest NAME $` — file chat session ปัจจุบัน

source target พิเศษ `$` trigger **agent turn** ที่สรุป conversation ที่กำลังทำอยู่ slash จะ rewrite ตัวเองเป็น structured prompt ที่บอก agent ให้

1. สรุป conversation เป็น wiki page ที่อ่านเข้าใจในตัว (200–1500 คำ, สังเคราะห์ ไม่ใช่ transcribe)
2. เรียก `KmsWrite(kms: "<name>", page: "<page>", content: "...")` พร้อม frontmatter `category: session, sources: chat`
3. confirm ผลลัพธ์กับ user พร้อม path ที่ resolve ได้

ชื่อ page resolve ตาม precedence

1. **user-supplied** ผ่าน `as <alias>` (sanitize เป็น kebab-case stem)
2. **session title** ถ้า session ของคุณมี title
3. **session id** (`sess-<hex>`) เป็น fallback สุดท้าย

ใช้ `--force` ถ้าต้องการแทนที่ page ที่มีอยู่แล้วของ slug ที่ resolve ได้

### `/kms dump NAME <text>`

บันทึกเนื้อหา freeform แล้วให้ระบบจัดเส้นทางให้เอง agent จะแบ่ง dump เป็น chunk ย่อย ๆ (หนึ่ง decision หนึ่ง observation หนึ่ง source ใหม่ ต่อหนึ่ง chunk) ประกาศแผนการ route ออกมาเป็นข้อความก่อน แล้วค่อยรัน `KmsWrite` / `KmsAppend` จริง Aliases: `capture`

> ต้องมี KMS tools — รัน `/kms use <name>` ก่อนถ้ายังไม่มี KMS attached ถ้าไม่มี KMS active คำสั่งนี้จะ refuse พร้อม error ที่ชัดเจน

```
❯ /kms dump notes Big standup. Decision: defer Redis migration — Tom raised cost
  concerns, Sarah agreed. Win: auth refactor praised by manager. Risk:
  backend cap shrinks next sprint, may push deadline.

(/kms dump notes → routing 198 char(s))

[agent] I'll route this:
- Append to redis-migration.md — decision to defer with Tom's cost rationale
- Append to brag-doc.md — manager praise on auth refactor
- Append to team-capacity.md — backend cap risk for next sprint
- Skip "big standup" header — too generic to file

[KmsAppend ×3 fire]

**Created**: none
**Appended**: redis-migration.md, brag-doc.md, team-capacity.md
**Skipped**: "big standup" — too generic
```

paste แบบหลายบรรทัดใช้ได้ทั้ง CLI และ GUI pattern **ประกาศก่อนค่อยทำ** ถูก bake เข้า prompt ไว้แล้ว — agent จะพิมพ์แผนออกมาก่อนยิง tool คุณจึง ⌃C เพื่อยกเลิกได้ทัน กฎเข้มที่ใส่ให้ agent: ห้ามแต่ง source เอง, ห้ามใช้ `KmsDelete`, ทุก page ใหม่ต้อง reference page อื่นที่มีอยู่แล้วอย่างน้อยหนึ่ง link (ถ้า link ไม่ได้ chunk นั้นจะถูก defer)

`capture` เป็น alias ของ `dump` ใช้คำไหนก็ได้ตามถนัด

### `/kms file-answer NAME <title>`

file ข้อความล่าสุดของ assistant เป็น page ใหม่ใน KMS ใช้ตอนที่ agent เพิ่งผลิตอะไรที่น่าเก็บ (สังเคราะห์, comparison table, debugging recap) แล้วคุณอยากให้มันอยู่ใน wiki แทนที่จะต้องไปไล่หาใน chat history alias: `file`

```
❯ /kms file-answer notes oauth-debugging-recap
filed answer → /Users/you/.config/thclaws/kms/notes/pages/oauth-debugging-recap.md (1428 bytes)
```

ชื่อ page คือ `<title>` ที่ sanitize เป็น stem frontmatter pre-set เป็น `category: answer, filed_from: chat` body คือข้อความ assistant ล่าสุดทั้งดุ้น ใต้ H1 ที่ใส่ title ไว้

### `/kms lint NAME`

ตรวจสุขภาพแบบ pure-read เดินไล่ `pages/` แล้วรายงานปัญหา 6 หมวด: link markdown ที่ชี้ไปยัง page ที่ไม่มีจริง (broken link), page ที่ไม่มีใคร link เข้ามา (orphan), entry ใน index ที่ชี้ไปไฟล์ที่หายไป, page บนดิสก์ที่ไม่มีใน index, page ที่ไม่มี YAML frontmatter และ (เมื่อ `manifest.json` ประกาศ `frontmatter_required` ไว้) field บังคับที่ขาดหายของแต่ละ category

```
❯ /kms lint notes
KMS 'notes': 3 issue(s)

broken links (1):
  - oauth-flow → pages/sso-config.md (missing)

pages missing from index (1):
  - tracing-conventions

missing required frontmatter fields (1):
  - paper-x: 'sources' (required by research)
```

alias ของ `/kms lint`: `/kms check`, `/kms doctor`

### `/kms wrap-up NAME [--fix]`

review ตอนจบ session รวม lint กับการสแกนหา stale-marker page — page ที่ถูกแปะเครื่องหมาย `> ⚠ STALE: source <alias> was re-ingested on YYYY-MM-DD` จากการ re-ingest cascade รอให้ refresh เนื้อหาตาม source ใหม่ Aliases: `wrapup`, `wrap`

```
❯ /kms wrap-up notes
KMS 'notes': wrap-up — 3 lint issue(s), 1 stale marker(s)

broken links (1):
  - oauth-flow → pages/sso-config.md (missing)

stale pages awaiting refresh (1):
  - summary: source `topic` re-ingested on 2026-05-08 (page not yet refreshed)

next steps: ask the agent to refresh stale pages and fix lint issues, or run `/kms lint <name>` again after edits.
```

ใส่ `--fix` เพื่อสั่ง subagent **`kms-linker`** ที่ติดมาในตัว (ดูหัวข้อ "Maintenance subagents" ด้านล่าง) ให้ลงมือแก้ตาม report — ค้นหา target จริงของ broken link, append bullet ที่ขาดเข้า index, refresh stale page จาก source ของมัน กฎเข้ม: ห้ามแต่ง, ห้ามลบ, ปล่อย orphan ไว้ (มักตั้งใจไว้แบบนั้น) ใช้ได้เฉพาะใน GUI — CLI จะพิมพ์ report แล้วบอกให้ไปเรียกจาก GUI

> ต้องมี KMS tools — รัน `/kms use <name>` ก่อน branch `--fix` จะ refuse พร้อม error ที่ชัดเจนถ้าไม่มี KMS attached เพราะ subagent inherit tool registry จาก parent ถ้าไม่มี tool พร้อม subagent จะ spawn มาแบบใช้งานไม่ได้

### `/kms reconcile NAME [<focus>] [--apply]`

แก้ contradiction อัตโนมัติ ส่งงานให้ subagent **`kms-reconcile`** ที่ติดมาในตัว ทำงาน 4 pass (claims / entities / decisions / source-freshness) จัดประเภทแต่ละจุด (clear-winner / ambiguous / evolution) แล้วทั้ง rewrite page ที่ outdated พร้อม `## History` section หรือสร้าง `Conflict — <topic>.md` ให้ user ตัดสินสำหรับเคสที่ ambiguous จริง ๆ ค่าเริ่มต้น dry-run; `--apply` ลงมือเขียนจริง arg ตำแหน่งที่สองเป็น focus ที่ narrow pass ลงเฉพาะ topic ใช้ได้เฉพาะ GUI Aliases: `resolve`

> ต้องมี KMS tools — รัน `/kms use <name>` ก่อนถ้ายังไม่มี KMS attached

```
❯ /kms reconcile notes
✓ kms-reconcile dispatched (id: side-7e2a, dry-run)

[subagent รายงานกลับ]

**Auto-resolved (3):**
- `oauth-flow.md`: "tokens expire 15min" → "tokens expire 30min" (source ใหม่ปี 2026-04 supersede 2025-09)
- `team-sarah-chen.md`: role อัปเดตจาก "Eng Lead" เป็น "Director" ตาม Q2 standup
- `redis-config.md`: cite `redis-2026-spec.md` แทน `redis-2025-spec.md`

**Flagged for user (1) — Conflict pages would be created:**
- `Conflict — auth-token-rotation.md`: paper-x ว่า rotate ทุก 24h, paper-y ว่าทุก 7d
  ทั้งคู่ peer-reviewed ต้องการการตัดสินจาก human

**Stale pages updated (2):**
- `architecture-overview.md`: ตอนนี้ cite `2026-arch-rfc.md` (เดิม `2025-arch-rfc.md`)
- `db-migrations.md`: เหมือนกัน

this was a dry-run preview. re-run with `--apply` to execute.
```

tool whitelist ของ `kms-reconcile` **แคบกว่า `dream`** — `KmsRead, KmsSearch, KmsWrite, KmsAppend, TodoWrite` เท่านั้น ไม่มี `KmsDelete` (reconcile รักษาทุก claim เดิมไว้ ทั้งใน `## History` หรือใน Conflict page) กฎเข้ม: ห้ามแต่ง date หรือ source; "เปลี่ยนใจ" จัดเป็น Evolution ไม่ใช่ contradiction

### `/kms migrate NAME [--apply]`

migration ของ schema ค่าเริ่มต้นเป็นแบบ dry-run (พิมพ์แผนออกมาเฉย ๆ ไม่เขียน) ใส่ `--apply` เพื่อลงมือจริง idempotent — รันบน KMS ที่อยู่เวอร์ชันล่าสุดแล้วจะรายงาน `already at schema version X — nothing to migrate` Aliases: `upgrade`

```
❯ /kms migrate legacy-notes
KMS 'legacy-notes': migration plan (0.x → 1.0, 1 step(s))

0.x → 1.0:
  - write /Users/you/.config/thclaws/kms/legacy-notes/manifest.json (schema_version: 1.0, frontmatter_required: empty)

this was a dry-run preview. re-run with `--apply` to execute.

❯ /kms migrate legacy-notes --apply
KMS 'legacy-notes': migration applied (0.x → 1.0, 1 step(s))

0.x → 1.0:
  - write /Users/you/.config/thclaws/kms/legacy-notes/manifest.json (schema_version: 1.0, frontmatter_required: empty)

logged to log.md. /kms lint to verify.
```

เมื่อมี schema ใหม่ใน release ถัดไป `/kms migrate` จะเดิน chain ทีละขั้นจากเวอร์ชันปัจจุบันของคุณไปจนถึงล่าสุด step 0.x → 1.0 ปัจจุบันแค่เขียน `manifest.json` ไม่ยุ่งกับ page

### `/kms challenge NAME <idea>`

red-team ก่อนตัดสินใจ — ส่งไอเดียหรือแผนเข้าไป agent จะค้น KMS หา past failure / decision ที่กลับลำ / contradiction ที่ user เคยเตือนตัวเองไว้ แล้วผลิตการวิเคราะห์ Red Team พร้อม citation ไปที่ page เฉพาะ read-only — ไม่มี write alias: `redteam`

> ต้องมี KMS tools — รัน `/kms use <name>` ก่อนถ้ายังไม่มี KMS attached

```
❯ /kms challenge notes ผมจะ ship auth refactor สัปดาห์นี้โดยที่ test harness ยังไม่พร้อม

[agent ค้นใน KMS]

**Your position:** Ship auth refactor this week without the new test harness.

**Counter-evidence from your vault:**
- `incident-2026-01-12` (date: 2026-01-12): "Auth incident traced to insufficient
  integration test coverage. Decision: never ship auth changes without the test harness."
- `1-1-Sarah-2026-04-08` (date: 2026-04-08): Sarah เคย flag ไว้ว่า ship-without-tests
  เป็น pattern ซ้ำที่กัดทุก quarter

**Blind spots:** อาจกำลังลดน้ำหนักของ integration test gap เพราะ unit test ผ่านอยู่
incident เก่าใน vault ชี้ว่า failure mode อยู่ที่ integration boundary

**Verdict:** vault แนะนำให้ระวัง past incidents กับ 1:1 ล่าสุดชี้ไปทางเดียวกัน
อย่างน้อยทำ manual smoke pass ก่อน merge
```

prompt บอก agent ตรง ๆ ว่า "อย่ายอม" — push back ถ้า vault ให้กระสุนมาถาม ผลลัพธ์เป็นการวิเคราะห์ ไม่มีการเขียนกลับเข้า vault

### `/kms link [<name>] [--apply] [--llm] [--min-len N]`

แทรก link `[[wiki-style]]` ข้าม page ใน KMS อัตโนมัติ ถ้าไม่ใส่
ชื่อ จะ iterate ทุก KMS ที่อยู่ใน `kms_active` ของ session
ปัจจุบัน Aliases: `autolink`, `cross-link`

**deterministic เป็น default** — scan หัวข้อ `## Goal` / `## Links`
และ body ของแต่ละ page หา occurrence ของ stem ของ page อื่น
(case-insensitive, ระวัง word boundary) แล้ว rewrite เป็น
`[[page-stem]]` `--min-len N` (default `4`) ตัด link ที่สั้นกว่า
N ตัวอักษรออก กัน noise แบบ `[[api]]` ที่จะ carpet ทั่ว page

**`--llm` switch ไปใช้ LLM-driven pass per page** — ส่ง page ไป
ผ่าน model ปัจจุบันพร้อม KMS index เป็น context ให้ surface โอกาส
cross-link ที่ deterministic หาไม่เจอ ("session" ↔ "conversation",
"token" ↔ "API key" ฯลฯ) ช้ากว่า (provider call ต่อ page) แต่
จับ semantic match ได้

**default เป็น dry-run** ใส่ `--apply` ถึงจะ write จริง

```
❯ /kms link notes
/kms link notes (deterministic, dry-run): scanned 23 page(s), 8 would gain link(s), 19 link insertion(s) total.
    oauth-flow: "session" → [[session-management]]
    oauth-flow: "refresh token" → [[token-refresh]]
    incident-2026-01-12: "auth flow" → [[oauth-flow]]
    …
  re-run with --apply to write the changes.

❯ /kms link notes --apply
/kms link notes (deterministic, applied): scanned 23 page(s), 8 modified, 19 link insertion(s) total.
```

ใช้หลัง `/kms ingest` run ใหม่ ๆ เพื่อทอ page ใหม่เข้าใน graph
เดิม หรือหลัง `/kms merge` เมื่อชุดที่รวมแล้วนิ่งแล้ว

### `/kms merge <src> <dst>`

รวม KMS สองตัวเข้าด้วยกัน — copy ทุก page, source, และ index
entry จาก `src` ไป `dst` พร้อม collision handling Aliases:
`combine`

- **page ชนชื่อ** → page ที่เข้ามาใหม่ถูก rename เป็น
  `<stem>-1.md` (หรือ `-2`, `-3`, …) ตัว original ของ destination
  ชนะ
- **aggregator page** (page ที่มี `aggregator: true` ใน
  frontmatter เช่น `architecture.md`) จะถูก **รวม** (combine)
  แทนการ rename — body ของ src จะถูก append ใต้ body ของ dst
  เพื่อไม่ให้ overview page consolidated แตกออกจากกัน
- **source file** ใน `sources/` ทำตามกฎ rename-on-collision
  เดียวกัน
- **index entry** ใน `dst/index.md` ถูก append ทุก page ใหม่

`src` **คงไว้ตามเดิม** — merge ไม่ destructive ฝั่ง source ให้
verify ก่อนค่อย cleanup

```
❯ /kms merge old-notes new-notes
merged 'old-notes' → 'new-notes': 47 page(s) copied (3 renamed, 2 combined), 14 source(s) copied (1 renamed), 47 index entr(ies) added.
  aggregator pages combined (src body appended under dst body):
    architecture.md
    decisions.md
  collision renames (kept original on dst, incoming was renamed):
    page: oauth-flow.md → oauth-flow-1.md
    page: session-id.md → session-id-1.md
    page: README.md → README-1.md
    source: spec.pdf → spec-1.pdf
  'old-notes' is left intact; run `/kms drop old-notes` once you've verified.

suggested workflow now:
  /kms wrap-up new-notes --fix       # fix broken links + STALE markers
  /kms link new-notes                # dry-run preview of auto-links
  /kms link new-notes --apply        # write the wikilinks
  /kms reconcile new-notes --apply   # resolve contradictions across pages
  /kms drop old-notes --force        # remove the source KMS once happy
```

output แนะนำ sequence cleanup ตามธรรมชาติ — `wrap-up --fix` patch
link ที่หักจากการ rename, `link --apply` ทอ page ใหม่เข้า graph,
`reconcile --apply` แก้ contradiction กรณี KMS สองตัวคุย topic
เดียวกันแบบไม่ตรงกัน แล้วค่อย `drop --force` retire KMS ต้นทาง

### `/kms drop NAME [--force]`

destructive — ลบ directory tree ทั้ง KMS (`<scope>/.thclaws/kms/<name>/`
หรือ `~/.config/thclaws/kms/<name>/`) Aliases: `delete`, `rm`

**default เป็น dry-run** ถ้าไม่ใส่ `--force` จะ print ว่าจะลบ
page กี่ตัว source กี่ตัว แต่ไม่ touch ดิสก์:

```
❯ /kms drop archived-notes
/kms drop archived-notes: dry-run (would remove 12 page(s), 3 source(s) from /Users/you/.config/thclaws/kms/archived-notes).
  re-run with --force to delete.

❯ /kms drop archived-notes --force
deleted KMS 'archived-notes' (12 page(s), 3 source(s)) from /Users/you/.config/thclaws/kms/archived-notes.
```

`--force` ยัง detach KMS ออกจาก `kms_active` ของ session ด้วย
(ไม่งั้น system prompt rebuild รอบหน้าจะ fail ตอน resolve ชื่อที่
ค้างอยู่) sidebar GUI refresh ทันที KMS ที่ drop หายจาก Knowledge
section

ไม่มี undo — directory หายไปหลัง `--force` ถ้า KMS อยู่ใน git
(project-scope ที่ commit ไว้) recover ด้วย `git checkout` ได้
ไม่งั้นหายเลย แนะนำให้ pair กับ `/kms merge` ก่อน ถ้าทำ
consolidation เพื่อให้มี copy ใน destination KMS ก่อน drop ตัว
ต้นทาง

## Schema versioning และกฎ frontmatter

`manifest.json` คือ schema ของ KMS ในรูปที่เครื่องอ่านได้ KMS ใหม่จะได้ไฟล์นี้ติดมาให้อัตโนมัติ:

```json
{
  "schema_version": "1.0",
  "frontmatter_required": {}
}
```

มีสองสิ่งอยู่ในนี้

- **`schema_version`** — เป็น anchor ของ `/kms migrate` เมื่อ thClaws ออก schema ใหม่ migrator จะตรวจเวอร์ชันปัจจุบันจาก field นี้แล้วเดินตาม chain ขึ้นไปจนถึงเวอร์ชันล่าสุด
- **`frontmatter_required`** — การบังคับใช้แบบ optional ค่าเริ่มต้นคือว่าง แก้ไฟล์นี้เพื่อประกาศว่า page แต่ละ category ต้องมี YAML frontmatter field อะไรบ้าง คีย์ `global` มีผลกับทุก page; คีย์อื่น ๆ มีผลเฉพาะ page ที่ field `category:` ตรงกัน

```json
{
  "schema_version": "1.0",
  "frontmatter_required": {
    "global": ["category", "tags"],
    "research": ["sources"]
  }
}
```

`/kms lint` จะรายงานเมื่อพบการละเมิด:

```
missing required frontmatter fields (1):
  - paper-x: 'sources' (required by research)
```

page ที่ไม่มี frontmatter เลย จะถูกแยกรายงานในหมวด `pages without YAML frontmatter` และถูกข้ามจากการตรวจ field รายตัว — แก้ทีละอย่าง

KMS เก่า (ที่สร้างก่อนมี manifest) จะไม่มี `manifest.json` ระบบจะข้ามการตรวจ field ให้เงียบ ๆ ใช้ `/kms migrate <name> --apply` เพื่อย้ายขึ้นมา v1.0 ได้ migration นี้เป็นแบบเพิ่มเท่านั้น (เขียนไฟล์ manifest ไม่ยุ่งกับ page เลย)

## Sidebar (GUI)

ส่วน **Knowledge** ของ sidebar จะแสดง KMS ทุกตัวที่ค้นพบ พร้อม checkbox ให้ทุกรายการ ติ๊กเพื่อผูก เอาติ๊กออกเพื่อถอด ซึ่งก็คือ toggle เดียวกับ `/kms use` และ `/kms off` นั่นเอง

ปุ่ม `+` จะถามชื่อก่อนแล้วจึงถาม scope (OK = user, Cancel = project) จากนั้นจะสร้าง KMS ใหม่พร้อมไฟล์ starter ที่เปิดแก้ไขต่อได้ทันที

## Tool ที่ agent เรียกใช้

### `KmsRead(kms: "name", page: "slug")`

อ่าน `<kms_root>/pages/<slug>.md` โดยเติมนามสกุล `.md` ให้เองหากไม่ใส่มา หากมีการพยายาม path traversal จะถูกปฏิเสธ (`..`, absolute path หรืออะไรก็ตามที่อยู่นอก `pages/`)

agent จะเรียก tool นี้เมื่อเห็นรายการที่เกี่ยวข้องใน `index.md`

```
[assistant] I'll check the auth-flow page first…
[tool: KmsRead(kms: "notes", page: "auth-flow")]
[result] (page content)
```

### `KmsSearch(kms: "name", pattern: "regex")` — line grep (default)

สแกนแบบ grep ครอบคลุม `<kms_root>/pages/*.md` ทั้งหมด แล้วคืนบรรทัดที่ตรงในรูปแบบ `page:line:text` หนึ่งรายการต่อบรรทัด ใช้เมื่อรู้ pattern ที่แน่นอน (TODO marker, function name, error code)

```
[assistant] Let me search for "bearer" across my notes…
[tool: KmsSearch(kms: "notes", pattern: "bearer")]
[result]
auth-flow:12:Bearer tokens expire after 15 minutes
api-conventions:34:Always include "Authorization: Bearer <token>"
```

### `KmsSearch(kms: "name", query: "...")` — BM25-ranked search (ค้นแบบ natural language)

ค้นด้วยภาษาธรรมชาติ ครอบคลุม title (boost ×4), topic (×2), body คืน hit ที่เรียงตาม relevance พร้อม snippet preview ใช้เมื่อไม่รู้คำเป๊ะ ๆ และอยากได้หน้าที่เกี่ยวข้องที่สุด ไม่ใช่ทุกบรรทัดที่ match

```
[assistant] Let me find pages about refresh tokens…
[tool: KmsSearch(kms: "notes", query: "token refresh flow")]
[result]
[score 6.12] page: auth-flow
  title: Refresh-token rotation
  topic: auth
  preview: The token refresh rotates on every login. Refresh tokens are stored…

[score 4.88] page: bug-2023-03
  preview: Rotation logic in __refresh_token__ misfired when the session…
```

Optional filter (ไม่มีผลต่อ score ranking — แค่ narrow ผู้สมัคร):

- `tags: ["auth", "security"]` — match หน้าที่มี tag ANY ของรายการ (OR semantics; ใช้ frontmatter `tags:`)
- `category: "runbook"` — exact match กับ frontmatter `category:`
- `limit: 20` — จำนวน hit สูงสุด (default 10, สูงสุด 50)

**ข้อแม้ build prerequisite** `query:` mode ต้องการ Cargo feature `kms_search_index` ซึ่งเพิ่ม ~4-5 MB ใน binary (tantivy + Thai dict) Release binary ทางการบน github.com/thClaws/thClaws/releases เปิด feature นี้แล้ว user ที่ใช้ `cargo install` ต้อง `cargo install thclaws-core --features kms_search_index` ถ้าไม่มี feature `query:` จะคืน error ชัดเจนชี้ไป `pattern:` ทาง regex `pattern:` ใช้ได้เสมอ

**First-touch indexing** การเรียก `query:` ครั้งแรกบน KMS ที่ยังไม่มี index จะ build index จาก `pages/` บน disk แบบ sync และแสดงบรรทัด `[index rebuilt — N page(s) indexed]` ครั้งเดียว ครั้งต่อ ๆ ไปจะใช้ warm index (sub-50 ms บน KMS 1000 หน้า) Bulk operation ที่ไม่ trigger hook ต่อหน้า — `/kms merge`, `/kms link --apply` — จะ trigger rebuild ในการค้นครั้งถัดไป หรือ force rebuild ทันทีด้วย `/kms reindex <name>`

**Thai-aware tokenization** เส้น BM25 ใช้ Rust port ของ PyThaiNLP `newmm` segmenter ดังนั้นเนื้อหาภาษาไทยถูก index แบบคำต่อคำ ไม่ใช่ทั้งย่อหน้าเป็น token เดียว ค้นได้เท่ากันทั้ง `query: "token refresh"` และ `query: "การรีเฟรช token"` Per-project supplement ทาง `<kms_root>/extra_words_th.txt` ให้เพิ่มคำเฉพาะทาง domain ที่ base dict พลาด

### `/kms search <name|*> <query>` — one-shot operator search

Surface เดียวกับ tool `KmsSearch` แต่เป็น slash command ค้นได้โดยไม่ผ่าน model round-trip (ประหยัด token + latency สำหรับ lookup สำรวจ และยืนยันว่า index ทำงานหลัง `/kms reindex`)

```
> /kms search notes token refresh
[score 6.12] page: auth-flow
  title: Refresh-token rotation
  preview: The token refresh rotates on every login…
```

ใช้ `*` แทน `<name>` เพื่อ fan out ค้นใน KMS ทั้งหมดที่มองเห็น — ผลลัพธ์ถูกจัดกลุ่มใต้ header ของแต่ละ KMS ดังนี้

```
> /kms search * bearer
── KMS: notes ──
[score 5.41] page: auth-flow
  preview: Bearer tokens expire after 15 minutes…

── KMS: project ──
(no hits)
```

Default คือ BM25 `query:` สลับไป regex line-grep ด้วย `--pattern`

```
> /kms search notes --pattern ^TODO
todos:3:TODO: rotate the staging cert
api:18:TODO: deprecate /v1
```

### `/kms reindex <name>` — manual rebuild

Drop `<kms_root>/.index/` แล้ว rebuild จาก `pages/` บน disk Operator-only (ไม่มี tool `KmsReindex` — model ไม่ตัดสินเองให้ rebuild กลางคัน) เหมาะหลัง bulk operation ที่ index ไม่เห็น หรือถ้า index file เสีย

```
> /kms reindex notes
/kms reindex notes — rebuilding…
/kms reindex notes — indexed 247 page(s)
```

### `KmsWrite`, `KmsAppend`, `KmsDelete`, `KmsCreate`

Surface สำหรับ mutate KMS ที่ agent (และ `/dream` consolidator ด้านล่าง) ใช้ Always-on — register ใน registry ตลอด ไม่ว่ามี KMS active หรือไม่ ทำให้ `/dream` กับ side-channel agent ตัวอื่น bootstrap audit-log KMS จากศูนย์ได้ ทุกตัวยกเว้น `KmsCreate` ต้อง approval (KmsCreate idempotent + name-validated, risk เท่ากับ `SessionRename`)

- `KmsWrite(kms, page, content)` — สร้างหรือเขียนทับ page รักษา YAML frontmatter ไว้, bump `updated:`, อัปเดต bullet ใน `index.md`, append `wrote | <page>` เข้า `log.md`. Auto-inject `# {title}\nDescription: {topic}\n---` block ถ้า body ไม่ขึ้นด้วย `# heading` Warn เมื่อ frontmatter ขาด `sources:`
- `KmsAppend(kms, page, content)` — ต่อท้าย page ที่มีอยู่ เร็วกว่า `KmsWrite` สำหรับการอัปเดตทีละนิด (log, journal, accumulating notes) bump `updated:` ถ้า page มี frontmatter
- `KmsDelete(kms, page)` — ลบ page, ตัด bullet ออกจาก `index.md`, append `deleted | <page>` ใน `log.md` ใช้ตอน consolidate เพื่อปลด page ที่ซ้ำหรือล้าสมัย
- `KmsCreate(name, scope)` — ensure ว่า KMS มีอยู่ Idempotent: return ref เดิมถ้ามีแล้ว ถ้ายังไม่มีก็ seed directory tree (pages/, sources/, index.md, log.md, SCHEMA.md, manifest.json) `/dream` Pass 5 ใช้ตัวนี้ bootstrap `dreams` KMS ก่อนเขียน summary

ชื่อ page จะถูก validate เป็น path-segment — ไม่มี separator, ไม่มี traversal, และชื่อสงวน `index`, `log`, `SCHEMA` ใช้เป็นชื่อ page ไม่ได้ (KMS เป็นคนจัดการเอง)

## Maintenance subagents

มี subagent ที่ติดมาในตัว 3 ตัวสำหรับดูแล KMS ทั้งหมดรันเป็น side channel (บทที่ 15) — agent ที่รันในของตัวเอง ในของ context window ตัวเอง งานเดินยาว ๆ จึงไม่ปนเข้า conversation หลักของคุณ

| Agent | สั่งด้วย | ขอบเขต | ใช้เมื่อไหร่ |
|---|---|---|---|
| `dream` | `/dream` | active KMS ทุกตัว | consolidate ลึกแบบเป็นระยะ — ขุด session ล่าสุด, dedupe page, ปรับโครงสร้าง |
| `kms-linker` | `/kms wrap-up <name> --fix` | KMS เดียว, report เดียว | แก้แบบเจาะจง — ลงมือตาม lint + stale-marker report ที่เป็นรูปธรรม |
| `kms-reconcile` | `/kms reconcile <name> [--apply]` | KMS เดียว | แก้ contradiction ข้าม page — rewrite พร้อม `## History` หรือ flag เป็น Conflict page |

> ทั้งสามตัวต้องมี KMS อย่างน้อยหนึ่งตัวอยู่ใน `kms_active` เพื่อให้ tool ของ KMS register ก่อน subagent spawn รัน `/kms use <name>` ก่อน; ถ้าไม่มี KMS active dispatch จะ refuse พร้อม error ชัดเจน แทนที่จะ spawn subagent ที่ไม่มี tool ใช้งาน

นอกจากนี้ยังรันบน schedule ได้ผ่าน [preset สำเร็จรูปในบทที่ 19](ch19-scheduling.md) — `nightly-close`, `weekly-review`, `contradiction-sweep`, `vault-health` หมายเหตุ: schedule ที่ยิงผ่าน daemon จะใช้ natural-language tool directive (ไม่ใช่ slash command) เพราะ daemon ยิงผ่าน `thclaws --print` ที่ไม่มี slash dispatch

### Consolidate แบบกว้าง: `/dream`

หลังจากทำงานไปไม่กี่สัปดาห์ KMS จะมี duplicate สะสม: page สอง page ที่พูดเรื่องเดียวกันแต่เนื้อหาไหลออกจากกัน, ข้อมูลเก่าที่ขัดกับสิ่งที่คุณพูดเมื่อวาน, insight จาก session ที่ไม่เคยถูกบันทึกเป็น page **`/dream`** คือ slash command ที่แก้ปัญหานี้ — มัน dispatch built-in `dream` agent เป็น side channel ซึ่ง consolidate KMS ของ project ใน background ขณะที่คุณทำงานอื่นต่อได้

```
/dream                 # consolidate 10 session ล่าสุด
/dream --all           # consolidate ทุก session ใน .thclaws/sessions/
/dream auth            # ให้ bias ไปทาง topic "auth"
/dream --all auth      # รวมกัน
/agents                # ดู dream ที่ active + เริ่มเมื่อไหร่
/agent cancel <id>     # หยุด dream ที่ออกนอกเรื่อง
```

`/dream` ใช้ได้เฉพาะใน GUI (ต้องใช้ chat surface ในการ render side bubble) dream agent รันแบบ concurrent กับ main คุณจึงสั่ง main ต่อได้ระหว่าง dream ทำงาน

**Background agents sidebar** (ดู[บทที่ 4](ch04-desktop-gui-tour.md#sidebar-ขวา-context-sensitive)) จะแสดง dream live: ชื่อ agent, เวลาที่ผ่าน, tool ที่เรียกล่าสุด, และ (ตอนจบ) hint ชี้ไปหน้า summary

#### มันทำอะไร

dream agent รัน **5 pass:**

1. **Survey + skip-already-dreamed** — อ่านรายการ active KMS + ของ `index.md` แต่ละตัว แล้วเปิด summary `dream-` ล่าสุดใน `dreams` KMS (dedicated audit-log KMS) เพื่อหาว่า session ไหน process ไปแล้ว Session ที่ `last_message_at` ≥ file mtime ปัจจุบัน → skip + list ใน "Skipped" section ของ summary
2. **Read sessions + auto-rename** — อ่าน session JSONL ที่รอด Pass 1 Session ที่ยังเป็น auto-title `sess-XXXXXXXX` → propose meaningful title (≤70 chars) แล้วเรียก `SessionRename` Skip ephemera (bug fix เล็กน้อยที่อยู่ใน git แล้ว, transient task) หา stable fact ที่ user สรุปไว้
3. **Consolidate** — สำหรับแต่ละ insight เลือก **active KMS** ที่ตรง topic (เช่น project convention → `project-knowledge`, preference ส่วนตัว → `personal-notes`); `KmsSearch` ใน active KMS นั้น; ถ้ามี page → `KmsAppend` แทน create ถ้า 2 page overlap หนัก → merge ผ่าน `KmsWrite` + `KmsDelete` ตัวซ้ำ. Page ใหม่/merged ได้ canonical shape (`title:` + `topic:` + `sources:`). **Pass 3 ทุก write ลง active KMS — ไม่ใช่ `dreams`** ถ้าไม่มี active KMS → skip Pass 3, ข้ามไป Pass 4 เลย
4. **Targeted reconcile (Pass 3b)** — walk back ทุก page ที่ modify ใน Pass 3 (อยู่ใน active KMSes ทั้งหมด) หา internal contradiction ใน page นั้นๆ → rewrite ด้วย `## History` section. Scope แค่ page ที่ run นี้แตะ — full-vault sweep เป็นงาน `/kms reconcile`. Rewrite ยังคงอยู่ใน active KMS เดิม
5. **Summarize** — เขียน **page เดียว** `dream-YYYY-MM-DD.md` ลงใน **`dreams` KMS** (ไม่ใช่ active KMS) นี่คือ page เดียวที่ลง `dreams` ทุก knowledge page จาก Pass 3 / Pass 3b อยู่ใน active KMSes แล้ว. Summary มี Sessions-processed table ที่ Pass 1 ของ dream ครั้งถัดไปจะอ่าน → รู้ว่า session ไหน process ไปแล้ว

**Two-way invariant** — Pass 3 + 3b เขียน active KMS เท่านั้น (ไม่ใช่ `dreams`); Pass 4 เขียน `dreams` เท่านั้น (ไม่ใช่ active KMS). Pass 1 อ่านทั้งคู่ได้ (หา prior summary ใน `dreams`, อ่าน index ของ active KMSes) Prompt มี "Common mistakes to avoid" section enumerate failure pattern ที่ model เคยพลาด (knowledge page mis-route ไป `dreams`, summary mis-route ไป active KMS, cross-vault merge)

`dreams` KMS ถูก auto-create (project-scope) ตอน `/dream` ครั้งแรกโดย dispatch path; `KmsCreate({name: "dreams", scope: "project"})` ถูกเรียกใน Pass 4 อีกครั้งโดย agent เป็น defense-in-depth (idempotent — no-op ถ้ามีอยู่แล้ว)

```
❯ /dream
✓ dreaming (id: side-9c4f1e)

[dream] surveying 2 active KMS (project-knowledge, scratch)…
[dream] reading 10 most recent sessions…
[dream] consolidating project-knowledge:
[dream]   appended 4 lines to auth-flow.md
[dream]   merged old-deployment.md into deployment.md, deleted old-deployment.md
[dream]   added 2 new pages: tracing-conventions.md, kafka-topics.md
[dream] writing dream-2026-05-07.md…
[dream] ✓ done in 3m12s. See dream-2026-05-07.md for the change log.
```

#### การ review ผลลัพธ์

dream agent รันด้วย `permission_mode: auto` — แก้และลบ page ได้โดยไม่ถาม **ขั้นตอน review คือ `git diff`** ถ้า project KMS ของคุณอยู่ใต้ git (ซึ่งควรจะอยู่ — `.thclaws/kms/` ก็แค่ markdown):

```bash
git diff .thclaws/kms/                        # ดูว่าเปลี่ยนอะไร
git checkout -- .thclaws/kms/                 # ทิ้งงานของ dream
git add .thclaws/kms/ && git commit -m "..."  # รับงาน
```

หน้า `dream-YYYY-MM-DD.md` คือคำอธิบายของ agent เองว่าทำอะไรไปบ้าง — อ่านอันนี้ก่อน แล้วค่อย spot-check diff ที่สำคัญ ถ้า summary บอกว่า "no new insights" และเขียน stub page นั่นคือ no-op outcome ที่ valid เช่นกัน

#### การ customize

built-in dream agent shipped อยู่ใน binary (system prompt + tool whitelist) คุณ override ได้ที่ระดับ project โดยสร้าง `.thclaws/agents/dream.md` พร้อม frontmatter และคำสั่งของคุณเอง — ตัว disk ชนะ built-in เสมอ ใช้ได้ถ้าทีมคุณมีนโยบาย KMS curation เฉพาะ (เช่น "ห้ามลบ page ที่ tag `archive: keep`")

dream agent default ใช้ tool: `KmsRead, KmsSearch, KmsWrite, KmsAppend, KmsDelete, Read, Glob, Grep, TodoWrite` — ไม่มี `Bash`, ไม่มี `Edit`/`Write` กับ project source, ไม่มี `Memory*` มัน modify ได้แค่ KMS เท่านั้น

### แก้แบบเจาะจง: `kms-linker`

ที่ `/dream` เป็นการกวาดกว้าง ๆ ทั่ว active KMS ทุกตัว **`kms-linker`** เป็นคู่หูแบบเจาะ — มันลงมือตาม lint report ที่เป็นรูปธรรมจาก `/kms wrap-up <name> --fix` จังหวะการใช้งานต่างกัน

- `/dream` เป็นแบบ *สำรวจ*: ขุด session หาเนื้อหาใหม่ ๆ ปรับโครงสร้าง dedupe page เหมาะรันเป็นระยะ (ทุกสัปดาห์ จบสปรินต์)
- `/kms wrap-up --fix` คือ *ปิดงาน*: ส่ง lint+stale ที่เจอให้มันปะ patch สิ่งที่แก้ตรงไปตรงมาได้ เหมาะรันตอนจบ session ก่อนเดินจาก

operating procedure ของ agent (encode อยู่ใน prompt)

| หมวดของ lint | สิ่งที่ทำ |
|---|---|
| Broken link `(page → target)` | `KmsSearch` หา target stem; ถ้ามีตัวเดียวที่เข้าได้ชัด แก้ link, ถ้าไม่ชัดให้ defer |
| Stale page `(stem, source, date)` | `KmsRead` stub page ของ source กับ stale page เอง; เขียน page ใหม่โดยรักษาโครงสร้างเดิม ตัดบรรทัด `> ⚠ STALE` ออก |
| Missing-in-index page | `KmsAppend` bullet 1 บรรทัดเข้า `index.md` ใต้ section ของ category ที่ตรงกัน |
| Missing required field | เติมเฉพาะที่ derive จาก body หรือ source ได้; ที่เหลือ defer |
| Orphan page | ไม่ทำอะไร — orphan มักมีเหตุผล รายงานในบรรทัดสุดท้ายให้คุณตัดสินใจ |

ข้อความปิดท้ายของ agent มี contract ตายตัว — block `**Fixed**` ลิสต์ทุกอย่างที่แก้แล้ว, block `**Skipped (need human judgment)**` ลิสต์ที่ปล่อยให้คุณ กฎเข้มเหมือน dream: ห้าม `KmsDelete`, ห้ามแต่ง source tool whitelist แคบกว่า dream — `KmsRead, KmsSearch, KmsWrite, KmsAppend, TodoWrite` เท่านั้น — เพราะ `kms-linker` ทำงานบนสิ่งที่ wrap-up หยิบยื่นให้เท่านั้น ไม่อ่าน session ไม่อ่านไฟล์ภายนอก

override ที่ `.thclaws/agents/kms-linker.md` ได้ถ้าทีมต้องการ policy อื่น

### Auto-reconcile: `kms-reconcile`

subagent ตัวที่สามที่ทำงานบน contradiction ไม่ใช่ lint finding ที่ `kms-linker` แก้ broken link กับ stale marker จาก `/kms wrap-up` **`kms-reconcile`** รัน 4 pass แบบขนานเพื่อหา contradiction จัดประเภท แล้วแก้พร้อมรักษา history เต็ม

4 pass (encode อยู่ใน prompt ของ agent)

| Pass | จับอะไร |
|---|---|
| Claims | concept และ project page ที่มี factual claim ทับซ้อนแต่ขัดกัน |
| Entities | entity page ที่ role / company / title / relationship drift ไป |
| Decisions | decision page ที่ถูก contradict โดย page หลังโดยไม่มี link `supersedes:` |
| Source-freshness | wiki page cite source เก่าทั้งที่ source ใหม่ในหัวข้อเดียวกันมีอยู่ใน KMS |

ต่อ finding agent จัดเป็น

- **Clear winner** — side ที่ใหม่กว่า + มี authority สูงกว่า rewrite page เก่า; `## History` section รักษาว่าอะไรเปลี่ยนกับเหตุผล
- **Genuinely ambiguous** — ทั้งสอง side มีหลักฐาน ไม่มีฝั่งไหน authoritative ชัด สร้าง `Conflict — <topic>.md` พร้อม `status: open` ทั้งสอง position ระบุ พร้อม evidence
- **Evolution** — ไม่ใช่ contradiction; user เปลี่ยนใจ จัดเป็น growth ผ่าน `## Timeline` section

tool whitelist เหมือน `kms-linker` — `KmsRead, KmsSearch, KmsWrite, KmsAppend, TodoWrite` **ไม่มี `KmsDelete`** (reconcile รักษาทุก claim เดิม ทั้งใน `## History` หรือใน Conflict page) override ที่ `.thclaws/agents/kms-reconcile.md` ได้ถ้าทีมต้องการ policy ต่าง (เช่น "สร้าง Conflict page เสมอ ไม่ auto-resolve")

`/kms reconcile` ค่าเริ่มต้น dry-run; `--apply` ลงมือ arg ตำแหน่งที่สอง narrow pass ลงเฉพาะ topic หรือ entity

## Artifacts ที่จะเห็นใน vault

subagent และ slash command เขียน pattern เฉพาะลง KMS ของคุณ เมื่อเจอใน page นี่คือใครเขียนและความหมาย

| Artifact | ใครเขียน | หมายความว่าอย่างไร |
|---|---|---|
| `## History` section ต่อท้าย page | `kms-reconcile` (clear-winner classification) | page ถูก rewrite ด้วย info ใหม่; block History เก็บ claim เดิมและเหตุผลของการอัปเดต |
| `## Timeline` section ต่อท้าย page | `kms-reconcile` (evolution classification) | ความคิดของ user เรื่องนี้เปลี่ยนตามเวลา; Timeline แสดงพัฒนาการตามลำดับ |
| page `Conflict — <topic>.md` พร้อม `status: open` | `kms-reconcile` (ambiguous classification) | สอง page ขัดกันแต่ไม่มีฝั่งไหน authoritative ชัด; Conflict page เก็บทั้งสอง position ให้คุณตัดสิน |
| บรรทัด `> ⚠ STALE: source ...` ใน body ของ page | `mark_dependent_pages_stale` หลัง re-ingest cascade | source ถูก re-ingest ด้วย `--force`; page นี้ reference ผ่าน frontmatter `sources:` และต้อง refresh |
| page `dream-YYYY-MM-DD.md` | `/dream` consolidation pass | audit trail ของหนึ่ง dream session — เพิ่ม / อัปเดต / ลบอะไรไป พร้อมเหตุผล |
| stub page ใน `pages/<alias>.md` ลงท้ายด้วย `_Replace this stub with a curated summary..._` | `/kms ingest` (file/URL/PDF) | source ดิบไปอยู่ที่ `sources/<alias>.<ext>`; stub นี้ชี้กลับไป enrich ผ่าน prompting ตามปกติหรือ `KmsWrite` |
| บรรทัด `## [date] verb \| <alias>` ใน `log.md` | ทุกการ write KMS | change log แบบ append-only grep ได้: `grep "^## \[" log.md \| tail -20` ดู activity ล่าสุด |

## Browse, graph และ HTML export (v0.8.5+)

KMS เพิ่ม 3 surface สำหรับ browse-time ใน v0.8.5 — ทั้งหมดอยู่ใน Desktop GUI ไม่กระทบไฟล์ format เดิม

### KMS browser sidebar

คลิกชื่อ KMS ใน sidebar ด้านซ้าย (ไม่ใช่ checkbox) จะมี panel ขนาด 260 px เลื่อนเข้ามาทางขวาแสดง page และ source archive ทั้งหมด คลิกไฟล์เปิด in-app viewer ทับ tab หลัก Tab ที่อยู่ใต้ยังถูก mount อยู่ จะ preserve state ของ xterm / chat ได้ ปิด browser, สลับ tab, หรือกด `ESC` จะกลับมาที่ tab เดิม

Viewer render Markdown ผ่าน `marked` พร้อม CSS แบบเอกสาร: heading ขีดเส้นใต้, blockquote tint accent, table มี border + zebra stripes, link 3 แบบ — external (underline solid), `[[wikilinks]]` ภายใน (dotted underline + accent pill), citation chip `[N]` (pill เล็ก rounded)

### Graph view สไตล์ Obsidian

Browser sidebar มีปุ่ม "Graph View" เหนือ list page คลิกแล้วจะแทน main pane ด้วย force-directed graph: page เป็นวงกลม, `[[wikilinks]]` เป็น edges, checkbox "Include sources" (default on) เพิ่ม source archive เป็น diamond node สีจางที่เชื่อมกับ page ที่อ้างถึง

- ลาก empty space เพื่อ pan; mouse wheel zoom รอบ cursor
- ลาก node ย้ายตำแหน่ง — pin กับ mouse, neighbors ตอบสนองด้วย spring forces
- คลิก node เปิดไฟล์ใน viewer
- Hover ไฮไลต์ node + neighbors ที่เชื่อม, อย่างอื่น dim
- Force simulation auto-stop เมื่อ layout settle (annealed damping; cap ~3 วินาที)

### `/kms html NAME [OUT]` — single-file interactive site

สร้าง HTML site แบบ self-contained จาก page ของ KMS เขียนลง workspace (default `./<NAME>-site/index.html`) ต่างจาก in-app viewer ตรงที่นี่เป็น **derived artifact** ที่แชร์, commit git, host บน S3, หรือส่งให้เพื่อนได้ — ไม่ต้อง depend thClaws Aliases: `site`, `export`

Agent ทำงาน 3 phase:

1. **Explore** — `KmsRead` index/manifest ถ้ามี แล้ว `KmsSearch` enumerate page slug. อ่าน 4–8 page ตัวแทนเพื่อเข้าใจ style เนื้อหา. ไฟล์ source ปิดเป็น default — citation อย่างเดียวไม่พอเป็นเหตุผลให้อ่าน
2. **Design components** — sketch component vocabulary ใน chat เป็น prose: site shell, page reader, navigation, citation chip, wikilink chip ฯลฯ. เลือก design tokens (typography, accent color, dark mode). Print sketch ออกมาเพื่อให้ ⌃C ได้ถ้าออกนอกทาง
3. **Assemble** — อ่าน page ที่เหลือ, เขียน `index.html` หนึ่งไฟล์ที่มี `<style>` + `<script>` inline พร้อม hash routing สำหรับ multi-view (`#/`, `#/page/<slug>`) และ JSON data island ที่มี body + frontmatter ทุก page. Citation render เป็น plain `[N]` marker; page คือ substance ของ site

Hard rules ใน prompt:

- ไฟล์เดียว. ไม่มี external CSS/JS, ไม่มี CDN, ไม่มี `fetch`. Double-click เปิดได้ offline
- Page เป็น primary content; source body ไม่ฝัง
- Multi-view ผ่าน hash routing เพื่อให้ deep link ใช้ได้
- Plain HTML + CSS + vanilla JS — ไม่มี framework

ปรับ output dir ผ่าน positional argument ที่ 2:

```
/kms html llm-wiki              # → ./llm-wiki-site/index.html
/kms html llm-wiki ../shareable # → ../shareable/index.html (relative กับ cwd)
/kms html llm-wiki /tmp/site    # → /tmp/site/index.html
```

Agent ใช้ session model ปัจจุบัน (Opus / GPT-4.1 / Sonnet 4.6 ใช้ดี — long-context model ทำงานดีกว่าเพราะ agent ฝัง body ทุก page ใน JSON island)

## ขีดจำกัดการ scale และทิศทางในอนาคต

KMS ตั้งใจให้ไม่มี embeddings โดย

- Grep เร็วพอใช้งานได้ถึงระดับไม่กี่ร้อย page
- การให้อ่าน `index.md` ก่อน ทำให้ agent มักเจอ page ที่เกี่ยวข้องได้โดยไม่ต้องค้นเลย
- page เป็น markdown ที่มนุษย์อ่านได้ จึงเปิดดูเองได้โดยไม่ต้องใช้เครื่องมือใด ๆ

เมื่อ KMS โตเกิน ~200 page หรือมีเนื้อหาภาษาอื่นที่ไม่ใช่อังกฤษซึ่ง grep จับคู่ข้ามไม่ได้สะอาดนัก hybrid RAG (BM25 + vector + LLM rerank ผ่าน [`qmd`](https://github.com/tobi/qmd)) เป็น fallback แบบ opt-in อยู่ใน roadmap โดย API ฝั่ง client จะยังคงเหมือนเดิม

## หมายเหตุสำหรับภาษาไทย

Grep ทำงานกับภาษาไทยได้ทันทีเพราะใช้การค้นแบบ substring ไม่ได้ผ่าน tokenize agent ของคุณจึงค้นคำว่า `"การยืนยันตัวตน"` ข้าม Thai note ทั้งหมดได้ผลลัพธ์ทันทีโดยไม่ต้องตั้งค่าอะไรเพิ่ม

สำหรับเนื้อหาเทคนิคที่ผสมไทยกับอังกฤษ ให้เขียนศัพท์เทคนิคภาษาอังกฤษไว้ในหน้าเดียวกับข้อความภาษาไทย แล้วทั้งคู่จะถูกจับได้เมื่อค้นเรื่องที่เกี่ยวข้อง

## Troubleshooting

- **"no KMS attached to this session"** — `/kms challenge`, `/kms dump`, `/kms reconcile`, และ `/kms wrap-up --fix` ต้องมี KMS อย่างน้อยหนึ่งตัวอยู่ใน `kms_active` เพื่อให้ tool ของ KMS register error message จะระบุชื่อ KMS เป้าหมาย — รัน `/kms use <name>` ก่อนเพื่อแก้
- **KMS ไม่ขึ้นใน sidebar** — ตรวจสอบว่าโฟลเดอร์มี `index.md` ที่ใช้ได้ (สร้างเองด้วยมือถ้าคุณปั้น KMS เอง) และอยู่ใน `~/.config/thclaws/kms/` หรือ `.thclaws/kms/`
- **การเปลี่ยนแปลงไม่สะท้อนในคำตอบของ agent** — `index.md` ถูกอ่านตอนเริ่ม turn ดังนั้น turn ที่กำลังรันอยู่จะยังใช้ snapshot ที่ถ่ายไว้ก่อนหน้า ให้เริ่ม turn ใหม่เพื่ออัปเดต
- error **"no KMS named 'X'"** จาก tool call — ชื่อเป็น case-sensitive และต้องตรงกับชื่อ directory ทุกตัวอักษร ให้ตรวจสอบด้วย `/kms list`
- **รายการ active เก่าค้างอยู่** — `.thclaws/settings.json` คือ source of truth หาก checkbox บน sidebar ไม่ตรงกับความจริง ให้แก้ไฟล์นี้ด้วยมือ
- **`/kms wrap-up --fix` บอก "nothing actionable"** — fix subagent ข้าม dispatch เมื่อ issue ที่เหลือมีแค่ orphan page กับ missing-frontmatter (สิ่งเหล่านี้ต้องการการตัดสินจาก human ไม่ใช่ mechanical fix) แก้เองด้วยมือ
- **Schedule preset ยิงแต่ไม่มีอะไรเกิดขึ้น** — prompt ของ preset เป็น natural-language directive ไม่ใช่ slash command `.thclaws/settings.json` ของ cwd ต้องมี KMS เป้าหมายอยู่ใน `kms_active` เพื่อให้ tool ของ KMS register ก่อน agent เริ่ม ดูบทที่ 19

## อ่านต่อที่ไหน

- [บทที่ 8](ch08-memory-and-agents-md.md) — memory และ project instructions (อีกสอง mechanism ที่ใช้จัดการ context)
- [บทที่ 10](ch10-slash-commands.md) — เอกสารอ้างอิง slash command รวมถึงตระกูล `/kms`
- [บทที่ 11](ch11-built-in-tools.md) — เอกสารอ้างอิง tool รวมถึง `KmsRead` และ `KmsSearch`
- [บทที่ 15](ch15-subagents.md) — subagent และ side channel (เจาะลึก `dream`, `kms-linker`, `kms-reconcile`)
- [บทที่ 19](ch19-scheduling.md) — scheduling รวมถึง preset สำเร็จรูปสำหรับการดูแล KMS (`nightly-close`, `weekly-review`, `contradiction-sweep`, `vault-health`)
