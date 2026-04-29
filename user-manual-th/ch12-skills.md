# บทที่ 12 — Skills

Skill คือ **เวิร์กโฟลว์ที่นำกลับมาใช้ใหม่ได้** ซึ่งบรรจุเป็นไดเรกทอรีที่มี:

- `SKILL.md` — YAML frontmatter (name, description, whenToUse) พร้อม
  คำสั่ง Markdown ให้โมเดลทำตาม
- `scripts/` (ไม่บังคับ) — shell / Python / Node script ที่ SKILL.md
  อ้างถึง โดยโมเดลจะเรียกผ่าน `Bash` เท่านั้น ไม่เคยเขียนใหม่เอง

skill คือวิธีย่อ "ช่วย deploy ตามพิธีกรรม 6 ขั้นของเราหน่อย" ให้
เหลือเพียงการเรียก tool ครั้งเดียว โมเดลจะอ่าน SKILL.md ทำตามคำสั่ง
แล้วใช้ script ที่คุณเตรียมไว้

## การค้นพบ (Discovery)

ตอนเริ่มต้น thClaws จะไล่หาในไดเรกทอรีเหล่านี้ตามลำดับ:

1. `.thclaws/skills/` — scope ของโปรเจกต์
2. `~/.config/thclaws/skills/` — scope ระดับ user
3. `~/.claude/skills/` — เพื่อรองรับ Claude Code
4. ไดเรกทอรีที่ plugin เพิ่มเข้ามา

`/skills` แสดงรายการที่โหลดไว้ ส่วน `/skill show <name>` พิมพ์เนื้อหา
SKILL.md ฉบับเต็มพร้อม path ที่ resolve เรียบร้อยแล้ว

## Marketplace

thClaws marketplace เป็นแคตาล็อก skill ที่ผ่านการคัดสรรและตรวจสอบ
license ดูแลที่ [thClaws/marketplace](https://github.com/thClaws/marketplace)
client จะดึงแคตาล็อกจาก `thclaws.ai/api/marketplace.json` และมีคำสั่ง
สำรวจสามตัว:

```
❯ /skill marketplace
marketplace (baseline 2026-04-29, 1 skill(s))
── development ──
  skill-creator            — Create new skills, optimize triggering, run evals
install with: /skill install <name>   |   detail: /skill info <name>
```

```
❯ /skill search creator
1 match(es) for 'creator':
  skill-creator            — Create new skills, optimize triggering, run evals
```

```
❯ /skill info skill-creator
name:        skill-creator
description: Create new skills, modify and improve existing skills...
category:    development
license:     Apache-2.0 (open)
source:      thClaws/marketplace (skills/skill-creator)
homepage:    https://github.com/thClaws/marketplace/tree/main/skills/skill-creator
install:     /skill install skill-creator (resolves to https://github.com/thClaws/marketplace.git#main:skills/skill-creator)
```

`/skill marketplace --refresh` จะดึงแคตาล็อกล่าสุดจาก `thclaws.ai`
(ปกติ refresh ตามต้องการ — baseline ที่ฝังในไบนารีรองรับการใช้งาน
แบบ offline) ส่วน search ไม่สนตัวพิมพ์ใหญ่/เล็ก และจัดอันดับให้
ตัวที่ตรงกับชื่ออยู่บนสุด

### License tier

แต่ละ entry มี `license_tier`:

- **`open`** — Apache-2.0 / MIT / ฯลฯ ใช้ `/skill install <name>`
  ติดตั้งได้ตรง ๆ
- **`linked-only`** — source-available redistribute ไม่ได้ จะแสดงใน
  แคตาล็อก (ให้รู้ว่ามีอยู่) แต่ `/skill install <name>` จะปฏิเสธ
  พร้อมส่ง URL upstream — ถ้าต้องการต้องไปติดตั้งจาก repo ต้นทางเอง

## การติดตั้ง skill

### จาก marketplace (แนะนำ)

ถ้า skill อยู่ใน marketplace catalog ติดตั้งด้วยชื่อได้เลย ไม่ต้องใช้
URL:

```
❯ /skill install skill-creator
  cloned https://github.com/thClaws/marketplace.git (subpath: skills/skill-creator) → .thclaws/skills/skill-creator
  installed skill 'skill-creator' (single)
(skill available in this session — no restart needed)
```

ภายในเบื้องหลัง thClaws จะ resolve ชื่อ marketplace ไปเป็น
`install_url` ของ entry นั้น แล้ว clone เฉพาะ subpath ออกจาก
registry repo (ดู subpath syntax ในหัวข้อ "จาก git repo" ด้านล่าง)
และนำผลไปวางที่ `.thclaws/skills/<name>/` skill จะใช้ได้ใน session
เดียวกันทันที

### จาก git repo

สำหรับ skill ที่ไม่ได้อยู่ใน marketplace ใส่ git URL ใด ๆ ก็ได้:

```
❯ /skill install https://github.com/some-user/some-skill.git
  cloned https://github.com/some-user/some-skill.git → .thclaws/skills/some-skill
  installed skill 'some-skill' (single)
```

ถ้า repo เป็น **bundle** (มี skill หลายตัวใน subdirectory) thClaws
จะตรวจจับและเลื่อน sub-skill แต่ละตัวขึ้นมาเป็น sibling ที่
`.thclaws/skills/<sub-name>/`

#### Subpath syntax (ดึง skill เดียวออกจาก repo ที่มีหลาย skill)

ใช้ extension `#<branch>:<subpath>` เพื่อติดตั้งเฉพาะ directory
หนึ่งใน repo:

```
❯ /skill install https://github.com/anthropics/skills.git#main:skills/canvas-design
  cloned https://github.com/anthropics/skills.git (subpath: skills/canvas-design) → .thclaws/skills/canvas-design
  installed skill 'canvas-design' (single)
```

repo จะถูก clone ลง staging dir ก่อน เฉพาะ subpath ที่ขอเท่านั้น
จะถูกย้ายเข้าตำแหน่งจริง ส่วนที่เหลือทิ้งไป ชื่อ skill ที่ derive ได้
จะมาจาก segment สุดท้ายของ subpath (`canvas-design` ในตัวอย่าง)
ไม่ใช่จาก URL ของ repo

### จาก URL `.zip`

```
❯ /skill install https://agentic-press.com/api/skills/deploy-v1.zip
  downloaded https://agentic-press.com/...zip (4210 bytes) → extracted
  installed skill 'deploy-v1' (single)
```

- จำกัดขนาดที่ 64 MB
- ป้องกัน zip-slip (path ที่เป็นอันตรายใน archive จะถูกปฏิเสธ)
- คง exec bit ของ Unix ไว้ เพื่อให้ script ที่มาในชุดยังรันได้
- หากมี wrapper directory ระดับบนสุดเพียงอันเดียว (`pack-v1/...`)
  จะถูก unwrap ให้อัตโนมัติ

### Scope

`--user` จะติดตั้งลง `~/.config/thclaws/skills/` แทนที่จะลง
`.thclaws/skills/` ของโปรเจกต์ โดยดีฟอลต์จะใช้ scope โปรเจกต์ —
marketplace install ก็ใช้ flag นี้ได้:

```
❯ /skill install --user skill-creator
```

### เขียนทับชื่อที่ระบบตั้งให้เอง

```
❯ /skill install https://example.com/deploy.zip ourdeploy
```

## การเรียกใช้ skill

มีสามวิธีที่ให้ผลเทียบเท่ากัน:

1. **ให้โมเดลตัดสินใจเอง** — trigger `whenToUse` ของ skill จะปรากฏใน
   system prompt โมเดลจะเรียก `Skill(name: "…")` เองเมื่อเจอเคสที่ตรง

   ```
   ❯ make me a PDF from this data
   Using the `pdf` skill to generate a PDF...
   [tool: Skill: pdf] ✓
   [tool: Bash: .../scripts/pdf_from_data.py] ✓
   ```

2. **ใช้ทางลัด slash ตรง ๆ** — `/pdf [args]` จะถูกเขียนใหม่เป็น
   การเรียก `Skill(name: "pdf")` ให้เอง

   ```
   ❯ /pdf from the report markdown, 10pt font
   (/pdf → Skill(name: "pdf"))
   ```

3. **ระบุชัดเจนใน prompt** — เช่นสั่งว่า "ใช้ pdf skill เพื่อ…" แล้วโมเดลจะทำตาม

## กายวิภาคของ SKILL.md

```markdown
---
name: deploy-to-staging
description: Deploy the current branch to staging and run smoke tests
whenToUse: When the user asks to deploy or ship to staging
---

1. Ensure the working tree is clean (`git status`). Abort if dirty.
2. Run `{skill_dir}/scripts/build.sh` to build the production bundle.
3. Upload `dist/` via `{skill_dir}/scripts/push-to-staging.sh`.
4. Run `{skill_dir}/scripts/smoke-test.sh` — it should exit 0.
5. Report the staging URL to the user.

If any step fails, abort and report the failing step.
```

ฟิลด์ใน frontmatter:

| Field | จำเป็น | วัตถุประสงค์ |
|---|---|---|
| `name` | ใช่ | id ของ skill ที่ไม่ซ้ำกัน (ดีฟอลต์จะตรงกับชื่อไฟล์) |
| `description` | แนะนำ | คำอธิบายสั้นบรรทัดเดียวที่จะแสดงใน `/skills` |
| `whenToUse` | แนะนำ | คำใบ้ trigger ที่โมเดลใช้ตัดสินใจว่าจะเรียกเมื่อไหร่ |

`{skill_dir}` ในเนื้อหาจะถูกแทนด้วย absolute path ของไดเรกทอรี skill
ตอนโหลด ดังนั้น path ของ script จึง resolve ได้เสมอ ไม่ว่าผู้ใช้จะ
เปิด thClaws จากที่ใดก็ตาม

## เขียน skill ของคุณเอง

skill ที่เล็กที่สุดเท่าที่จะเป็นไปได้มีหน้าตาแบบนี้:

```
.thclaws/skills/hello/
  SKILL.md
```

```markdown
---
name: hello
description: Say hi in all caps
whenToUse: User asks to say hi loudly
---

Reply with exactly "HELLO!" and nothing else.
```

แค่นี้ก็ใช้งานได้แล้ว skill ที่เป็น prompt อย่างเดียวไม่ต้องมี script

สำหรับงานที่ต้องขับเคลื่อนด้วย script:

```
.thclaws/skills/greet/
  SKILL.md
  scripts/
    greet.sh
```

```markdown
---
name: greet
description: Run the greeting script with the user's name
whenToUse: User asks to greet someone
---

Run `bash {skill_dir}/scripts/greet.sh <name>` where <name> is the
person to greet.
```

```bash
# scripts/greet.sh
#!/bin/sh
echo "Hello, $1! Glad to see you."
```

ทำให้ script execute ได้ด้วย `chmod +x` แล้ว `Bash` tool จะรันให้โดยตรง

## การ refresh ขณะรันไทม์

หลังจาก `/skill install` thClaws จะค้นพบ skill ใหม่ทันที อัปเดตทั้ง
live store ของ SkillTool และตัว resolver ของทางลัด `/<skill-name>`
พร้อมสร้าง system prompt ใหม่ให้ skill ใหม่โผล่ขึ้นในส่วน
`# Available skills` — ไม่ต้อง restart ใช้ได้ทั้ง CLI REPL และ GUI
ทั้งสองแท็บ

## เขียน skill สำหรับโมเดลที่ต่างกัน

โมเดลในเครื่องที่เล็กกว่า (เช่น Gemma ผ่าน Ollama หรือ Qwen ขนาด
7-12B พารามิเตอร์) จะทำตามคำสั่ง skill ที่ชัดเจนและออกคำสั่งตรง ๆ ได้
ดีกว่าคำสั่งแบบหลวม ๆ หากต้องการให้ skill ใช้งานได้กว้าง ให้

- เขียนว่า "รัน X" แทน "พิจารณารัน X"
- ใส่เลขลำดับขั้นตอน
- รวมการจัดการความล้มเหลวไว้ที่ท้ายสุด ไม่กระจายไปในแต่ละขั้น
- เลี่ยง bullet ซ้อนชั้นใน Markdown เพราะ tokenizer บางตัวจัดการได้ไม่ดี

skill ที่ใช้งานได้บน Claude Sonnet ไม่การันตีว่าจะใช้งานได้บน
`ollama/gemma3:12b` ดังนั้นควรทดสอบกับโมเดลเป้าหมายของคุณเสมอ
