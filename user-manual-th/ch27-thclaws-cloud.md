# บทที่ 27 — thClaws.cloud

thClaws.cloud คือ catalog และ hosted runtime สำหรับ agent ของ
thClaws ทำให้แนวคิด **folder-คือ-agent** (บทที่ 8) กลายเป็นของที่
browse ได้ publish ขึ้น catalog ได้ ติดตั้งลงเครื่องอื่นได้ หรือเช่า
hosted workspace มารันก็ได้ จากมุมของ desktop thClaws การใช้ cloud
จะรู้สึกเหมือน git สำหรับ AI agent — paste CLI token ครั้งเดียวใน
Settings แล้วทุก catalog op (`/cloud get`, `/cloud publish`,
`/cloud list`, …) ทำงานเป็น slash command ภายใน thClaws session

![`/cloud status` — URL ของ catalog, มี token เก็บไว้หรือยัง และโฟลเดอร์นี้ผูกกับ Agent Template ตัวไหน](../user-manual-img/ch-27/cloud-status.png)


> **ขอบเขตของบทนี้ (ฝั่ง client เท่านั้น).** การ browse catalog การ
> publish agent ของตัวเอง การติดตั้ง agent ลง folder และบล็อก
> `agent.{name, description, uuid}` ใน `settings.json` ส่วน runbook
> สำหรับการรัน catalog server เองอยู่ใน
> [`dev-plan/34`](../dev-plan/34-thclaws-cloud-control-plane.md) และ
> source tree `thclaws-cloud/` ที่ workspace-private

## Agent Templates กับโมเดล folder-คือ-agent

catalog เรียก agent ที่ publish แล้วว่า **Agent Template** — โฟลเดอร์ที่ใคร
สักคนทำเสร็จแล้วแบ่งปันไว้ พร้อมจะกลายเป็น agent ใน workspace ของคุณ พอดึงมา
มันก็ก๊อบเข้ามาเป็นของคุณ มีบทสนทนาและการตั้งค่าของตัวเอง และเทมเพลตต้นทาง
ก็หมดความสำคัญไปตั้งแต่นาทีนั้น การเอาหลายเทมเพลตมาไว้ใน workspace เดียวกัน
อยู่ใน [บทที่ 35](ch35-agents-in-a-workspace.md)


ในทุกที่ที่ thClaws รันได้ **AI agent คือโฟลเดอร์** หนึ่ง โดยที่ราก
ของโฟลเดอร์มี 3 ไฟล์หลัก:

- `AGENTS.md` — คำสั่งของ agent (system prompt + persona)
- `manifest.json` — metadata สำหรับ catalog (slug, license, icon, tag)
  ใช้เฉพาะตอนจะ publish
- `./.thclaws/` — state ภายในเครื่อง (settings, KMS, session, memory)

เวลาคุณ `cd` เข้าไปใน folder นั้นแล้วรัน thClaws คือคุณ "รัน agent
ตัวนั้น" เวลา publish catalog ก็จะแพ็คไฟล์เหล่านี้ทั้งหมดเป็น tarball
เวลาคนอื่นรัน `/cloud get <slug>` จาก session ของเขาเองก็จะได้ folder
เดียวกัน — cloud เป็นแค่ทางขนย้าย folder ระหว่างเครื่อง

## ตั้งค่า URL catalog + CLI token

ของสองอย่างที่ผูก desktop เข้ากับ catalog server:

1. **Cloud URL** — `settings.json::cloud.url` ค่า default คือ public
   instance (`https://thclaws.cloud`) จะ override ไปชี้ที่
   `http://localhost` หรือ self-hosted instance ของตัวเองก็ได้
2. **CLI token** — สตริง `thc_…` จากหน้า dashboard ของ catalog ถูก
   เก็บใน OS keychain (ไม่เคยอยู่ใน `settings.json`)

Settings → **thClaws.cloud** มีช่องให้ใส่ทั้งสองตัว วาง URL วาง token
ที่ mint จากหน้า dashboard (**+ New token**) แล้วกด Save — slash
command ทุกอันในบทนี้ก็พร้อมใช้ทันที

Token ไม่เคยผ่าน shell argument หรือ environment variable เลย — GUI
เก็บลง OS keychain (macOS Keychain / Windows Credential Manager /
Linux Secret Service) ตรง ๆ และทุก request ส่งเป็น Bearer header
จากภายใน engine process ไม่รั่วลง `ps` หรือ shell history

> **ทำไมไม่มี CLI subcommand?** รุ่นก่อนหน้ามี
> `thclaws cloud login --token …` แต่ถูกเอาออกเพราะ token ที่ผ่าน
> `argv` จบลงใน shell history และเครื่องมือ `ps`-style ทุกตัว
> ตอนนี้เหลือ Settings UI + keychain เป็นทางเดียว ถ้ารัน
> `thclaws cloud …` แบบเก่าจะ print error ชี้ไป flow ใหม่

## เปิดดู catalog

จาก thClaws session (REPL หรือ Chat tab):

```
❯ /cloud status
thClaws.cloud — https://thclaws.cloud (token: ✓ stored)

❯ /cloud list
- hello-world           v0.1.0  Hello-world demo agent (jimmy)
- legal-doc-reviewer    v0.4.2  Reviews contracts paragraph-by-paragraph (acme)
- weekly-research       v1.0.0  Saturday-morning newsletter writer (rin)
...

❯ /cloud list --mine
- weekly-research       v1.0.0  Saturday-morning newsletter writer (you)
```

แต่ละแถวคือ agent หนึ่งตัวใน catalog ส่วน slug คือสิ่งที่ต้องส่งให้
`/cloud get`

## ติดตั้ง agent ลง folder

`/cloud get` จะติดตั้งลง **current directory ของ session** เสมอ flow
ปกติ:

```bash
# 1. จาก shell — สร้าง folder ว่างสำหรับ agent แล้ว cd เข้าไป
mkdir my-hello && cd my-hello

# 2. เปิด thClaws session ใน folder นั้น
thclaws            # GUI default หรือ --cli ก็ได้
```

จากนั้นใน session:

```
❯ /cloud get hello-world
Downloading hello-world (v0.1.0) …
Extracted to /Users/jimmy/my-hello/
  ✓ AGENTS.md
  ✓ manifest.json
  ✓ skills/greet.md
Done. /reload to pick up the new AGENTS.md.
```

engine จะดาวน์โหลด tarball แตกไฟล์ทั้งหมดลง cwd แล้ว `/reload` ครั้ง
ถัดไปจะอ่าน `AGENTS.md` ใหม่ ไม่ต้องออกไป shell อีกรอบ

### กลไก folder-safety

`cloud get` จะไม่ยอม overwrite folder ที่ไม่ว่าง ยกเว้นจะเป็น agent
ตัว **เดียวกัน** (match ด้วย UUID ดูด้านล่าง) กฎเป็นแบบนี้:

| สถานะของ folder ปลายทาง | พฤติกรรม |
|---|---|
| ว่าง | ติดตั้งใหม่ |
| มี `AGENTS.md` / `manifest.json` และ `agent.uuid` ตรงกัน | อัปเดตทับได้ — เก็บ `.thclaws/` session state เดิมไว้ |
| มี `AGENTS.md` / `manifest.json` แต่ UUID **ไม่ตรง** | abort — folder นี้เป็นของ agent อื่น ให้ `/cloud unbind` ก่อน หรือใช้ folder อื่น |
| มีไฟล์อื่น ๆ ที่ไม่เกี่ยวข้อง (note, scratch ฯลฯ) | abort — ให้ติดตั้งลง folder ว่างแทน |

ออกแบบไว้แบบนี้โดยตั้งใจ — กันพิมพ์ผิดแล้วเขียนทับงานที่ทำค้างไว้
หรือ agent ของคนอื่นใน directory เดียวกัน ฝั่ง slash ไม่มี `--force`
override โดยตั้งใจ ถ้าไม่แน่ใจให้ `/cloud get` ลง directory ว่าง

## Publish agent

เวลาคุณสร้าง agent ใน folder หนึ่งและอยากให้มันขึ้น catalog ให้เปิด
thClaws session ใน folder นั้น แล้วใช้ slash command:

```
❯ /cloud publish              # อัปโหลด cwd
```

`/cloud publish` ทำ 3 อย่าง:

1. **Tar + gzip** folder ทั้งก้อน — secret, session, KMS page และ
   directory `./.thclaws/` ถูกตัดออกอัตโนมัติ คุณ re-publish ทุกวันได้
   โดยไม่ทำให้ประวัติแชทรั่ว
2. **Upload** ขึ้น catalog ด้วย CLI token ของคุณ
3. **Stamp identity ของ agent กลับลง `settings.json`** (ดูหัวข้อถัดไป)

ถ้า `manifest.json` หายหรือ invalid `publish` จะ abort พร้อม error
ที่ชัด minimum field ที่ต้องมี: `id`, `name`, `description`, `version`

## Publish หน้า HTML

นอกเหนือจากเรื่อง agent แล้ว thClaws.cloud ยังโฮสต์ **ไฟล์ HTML ไฟล์เดียวที่
สมบูรณ์ในตัว** ให้คุณที่ URL ส่วนตัวได้ด้วย:

```
❯ /publish dashboard.html
published → https://k3m9qp2wv7xa.thclaws.app  (expires in 3 days)
```

ไฟล์ต้องยืนอยู่ได้ด้วยตัวเอง — ฝัง CSS กับ JS ไว้ในไฟล์เลย เพราะไม่มีอะไร
ข้าง ๆ ถูกอัปโหลดขึ้นไปด้วย สิ่งที่ได้กลับมาคือ subdomain 12 ตัวอักษรที่
*เดาไม่ถูก* มากกว่าจะเป็น *ความลับ*: ใครที่มีลิงก์ก็เปิดได้ และมันจะหยุด
ทำงานหลังผ่านไปสามวัน

ต้องมี token ของ thClaws.cloud และยอดเครดิตมากกว่าศูนย์ ด้วยเหตุผลเดียวกับ
ทุกอย่างในบทนี้ — บัญชีคือสิ่งที่จ่ายค่าโฮสต์

นี่คือปลายทางตามธรรมชาติของเซสชันแบบ "ช่วยทำ dashboard ให้หน่อย": agent เขียน
หน้าเว็บ คุณดูผลในแท็บ Files ([บทที่ 4](ch04-desktop-gui-tour.md)) แล้ว publish
เพื่อส่งลิงก์ให้คนอื่น

## บล็อก agent identity ใน `settings.json`

บล็อก top-level `agent` ใน `./.thclaws/settings.json` เก็บ identity
ของ folder นี้บน catalog:

```json
{
  "agent": {
    "id": "my-research-bot",
    "name": "My Research Bot",
    "description": "Saturday-morning newsletter writer",
    "uuid": "1f9c1d70-3a26-43c4-9c40-1b1b6e3e3a01"
  }
}
```

- **id / name / description** — คัดลอกมาจาก `manifest.json` ตอน
  publish ใช้โดย catalog UI และโดย safety check ของ `/cloud get`
- **uuid** — assign โดย catalog ครั้ง **แรก** ที่ publish จาก folder
  นี้ แล้วเขียนกลับลง `settings.json` ครั้งต่อไปที่ publish จะไปลง
  catalog row เดิม (เพิ่ม version) UUID คือสิ่งที่ `/cloud get` ใช้
  match ว่า "folder นี้คือ agent ตัวเดียวกันมั้ย"

ปกติไม่ต้องแก้บล็อกนี้เอง GUI Settings → **Agent identity** มี
panel ให้แก้ `name` / `description` (สะดวกก่อน publish — description
จะไปโผล่ในรายการ catalog) แต่ตั้งใจซ่อน `uuid` ไว้

### Fork agent ที่ดาวน์โหลดมา

ถ้า `/cloud get` agent ของคนอื่นมาแล้วอยาก fork ในชื่อตัวเอง จาก
session ของ folder agent นั้น:

```
❯ /cloud unbind            # ล้าง settings.json::agent.uuid
❯ # ใน session เดิม: แก้ AGENTS.md, manifest.json — เปลี่ยน `id` เป็นชื่อที่ว่าง
❯ /cloud publish           # ได้ UUID ใหม่
```

ถ้าไม่ `/cloud unbind` ก่อน publish ครั้งต่อไปจะพยายาม update catalog
row ของเจ้าของเดิม (และจะ fail ด้วย permission error — catalog gate
การ publish ตาม author)

## Visibility — public / unlisted / private

ทุก agent ที่ publish มีสถานะ **visibility** สามแบบ:

| Visibility | ใครเห็น / ติดตั้งได้ |
|---|---|
| `public` | ทุกคน — โผล่ใน `/browse`, หน้า `/a/<slug>`, ติดตั้งด้วย `/cloud get` ได้ **ต้องผ่านการรีวิวโดย superadmin เท่านั้น** (author ทั่วไปเลื่อนเป็น public เองไม่ได้) |
| `unlisted` (ค่า default) | ใครก็ตามที่มี **ลิงก์** — ไม่โผล่ใน `/browse` แต่หน้า `/a/<slug>` เปิดได้และ `/cloud get <slug>` ติดตั้งได้ เป็นค่า default แบบ share-link ของ agent ที่ผู้ใช้ publish เอง |
| `private` | เฉพาะ **เจ้าของ** (author) และ root เท่านั้น |

agent ที่ผู้ใช้ (verified, ไม่ใช่ superadmin) publish ใหม่จะเป็น
**`unlisted` โดย default** — เข้าถึงได้ด้วยการแชร์ลิงก์หน้า ไม่โผล่ใน
catalog สาธารณะ ส่วน agent ที่เป็น `private` จะถูกซ่อนจากทุกจุด (list,
detail, `/cloud get`, fork ตอบ **404** — ไม่ใช่ 403 เพื่อให้ slug เดาไม่ได้)

**เปลี่ยน visibility ยังไง** — ไปที่หน้า agent ของคุณบนเว็บ
(`https://thclaws.cloud/templates/<slug>`) จะมีปุ่ม toggle ตอนคุณเป็นเจ้าของ
(หรือ root) author สลับระหว่าง `private` กับ `unlisted` ได้อิสระ แต่
**การเลื่อนเป็น `public` ทำได้เฉพาะ superadmin** (ไม่งั้นตอบ 403) *ยังไม่มี*
คำสั่ง `/cloud` ฝั่ง desktop สำหรับเรื่องนี้ — เป็น web-only (เบื้องหลังเรียก
`PATCH /api/agents/<slug>/visibility`)

## Hosted workspace (เช่าแทนที่จะติดตั้ง)

ถ้าไม่อยากติดตั้ง agent บนเครื่อง laptop ตัวเอง catalog ก็รัน agent
เป็น **hosted workspace** ให้ได้ — หนึ่ง container ต่อหนึ่ง
workspace มี URL ให้เปิดในเบราว์เซอร์ มี chat UI จริงที่ backend ใช้
engine ตัวเดียวกับที่คุณรันใน local

จาก web UI ของ catalog:

1. browse ไปหน้า detail ของ agent
2. กด *Install on hosted* แล้วเลือก (หรือสร้าง) workspace ที่ **ready**
3. catalog จะติดตั้ง agent ลง workspace ที่มีอยู่นั้น — **เขียนทับ** agent
   definition เดิมของมัน — แล้ว redirect ไป chat UI ที่ `/u/<handle>/<slug>/`

Hosted workspace รองรับทั้ง BYOK (วาง provider key เองที่ *Settings
→ Hosted keys*) และ **thClaws.cloud gateway** (proxy แบบ pay-per-use
ที่มี credit billing ดูด้านล่าง) ตอนสร้าง workspace มี radio
toggle ให้เลือก

### Sync folder ในเครื่องกับ hosted workspace

hosted workspace ไม่ได้เป็นทางตันที่เข้าถึงได้แค่ผ่านเบราว์เซอร์ คุณจับคู่
folder ในเครื่องกับมันแล้วย้ายไฟล์ได้สองทาง — แก้บนแล็ปท็อปแล้ว push ไปทำต่อ
ในเบราว์เซอร์ หรือกลับทางก็ได้

```
❯ /cloud push                 # folder ในเครื่อง → hosted workspace
❯ /cloud pull                 # hosted workspace → folder ในเครื่อง
❯ /cloud revision             # แต่ละฝั่งอยู่ revision ไหน และมีอะไร dirty ไหม
```

`/cloud push` ครั้งแรกจาก folder ที่ยังไม่จับคู่จะทำการจับคู่ให้ หลังจากนั้น
binding ถูกจำไว้ `push` / `pull` เปล่า ๆ จึงคุยกับ workspace เดิม ถ้าจะเจาะจง
ตัวอื่นใช้ `--workspace <slug>`

**`/cloud revision` คือคำสั่งที่ควรรันก่อน** มันอ่านอย่างเดียวล้วน ๆ และจะ
ไม่ปลุก workspace ที่หลับอยู่ เพราะคำถามเชิงสถานะไม่ควรทำให้คุณเสียค่า resume
pod และครึ่งที่เป็นข้อมูลในเครื่องจะพิมพ์ออกมาเสมอ แม้ออฟไลน์หรือยังไม่ล็อกอิน

```
❯ /cloud revision
Local:     rev 12, pushed 2 hours ago, pulled 3 days ago
Cloud:     rev 12
Changes:   clean — no work changed since rev 12 (3 runtime state file(s) did)
```

มีสองอย่างในผลลัพธ์นี้ที่ควรเข้าใจ

**revision ที่ตรงกันแปลว่าสองฝั่ง *ตกลงกันครั้งล่าสุด* ที่เลขนั้น ไม่ได้แปลว่า
ไฟล์วันนี้เหมือนกัน** บรรทัด `Changes:` ต่างหากที่บอกว่า folder นี้ drift ไป
จากตอนนั้นหรือยัง และมันคำนวณจากการ hash working tree ในเครื่อง จึงตอบได้
แม้ workspace จะหลับอยู่

**runtime state ถูกนับแยก** session, cache และอื่น ๆ ใน `.thclaws/state/`
เขียนทับตัวเองอยู่แล้วแค่จากการที่ engine รัน ถ้านับเป็น "การเปลี่ยนแปลงของคุณ"
ทุก folder จะ dirty ตลอดกาล ของพวกนี้ยังถูก push ไปด้วยในรอบถัดไป — แค่ถูก
แยกนับ ไม่ได้ถูกซ่อน

#### Flag

| Flag | ผล |
|---|---|
| `--dry-run` | รายงานว่าอะไรจะถูกย้าย แต่ไม่ย้ายจริง |
| `--delete` | ลบไฟล์ที่ปลายทางซึ่งหายไปจากต้นทางด้วย ถ้าไม่ใส่ sync จะเพิ่มและอัปเดตเท่านั้น |
| `--force` | ดันผ่าน **divergence guard** — ตัวที่กันไม่ให้คุณเขียนทับสิ่งที่อีกฝั่งแก้ไปหลังจากที่สองฝั่งตกลงกันครั้งล่าสุด |
| `--force-rebind` | ชี้ folder นี้ไปยัง workspace ตัวอื่น โดยข้ามการตรวจ binding/identity **มันกิน `--force` ไปในตัว** เพราะการจงใจชี้ folder ใหม่*คือ*การเขียนทับอยู่แล้ว |
| `--workspace <slug>` | ทำงานกับ workspace ที่ระบุ แทนตัวที่ผูกไว้ |

เริ่มด้วย `--dry-run` เสมอ ส่วน `--force` มีไว้สำหรับตอนที่คุณรู้ว่าของอีกฝั่ง
ทิ้งได้ ถ้าไม่แน่ใจ ให้ `/cloud pull --dry-run` ดูก่อนว่าจะเสียอะไรไปบ้าง

> **ใช้บน multiuser pod ไม่ได้** คำสั่งกลุ่มนี้ทำงานกับ directory tree เดียว
> ที่อ้างอิงจาก working directory ของ process แต่ multiuser pod ใช้ process
> เดียวร่วมกันทุก tenant — directory นั้นคือ pod root ที่เก็บ
> `workspace-<id>/` ของทุกคน การ push จากตรงนั้นจะส่งไฟล์ของทุก tenant เข้า
> workspace ของ tenant คนเดียว ส่วน pull จะเขียนทับของทุกคน ระบบจึงปฏิเสธ
> ไปเลย แทนที่จะพยายามเดาใจ

## Gateway แบบ pay-per-use (ทางเลือกแทน BYOK)

สำหรับผู้ใช้ที่ไม่อยาก manage account ของ Anthropic / OpenAI / Gemini
เอง thClaws.cloud มี **gateway** ให้ — เติม credit ครั้งเดียวแล้ว
เรียก model อะไรก็ได้ผ่าน `gateway.thclaws.cloud/<provider>/...` โดย
ใช้ token `gw_v1_…` Gateway จะ forward ไป upstream meter response
แล้วหักจาก balance

วิธีใช้ gateway จาก thClaws **desktop**:

1. mint gateway access key ใน catalog UI: **/gateway/keys** → *Mint
   new gateway key* → copy สตริง `gw_v1_…`
2. เติม credit: **/credit** → เลือก pack ($5 / $20 / $100) pack ใหญ่
   มี bonus credit
3. ตั้งให้ thClaws ชี้ไปที่ gateway:
   ```bash
   export ANTHROPIC_API_KEY=gw_v1_…
   export ANTHROPIC_BASE_URL=https://thclaws.cloud/gateway/anthropic
   export OPENAI_API_KEY=gw_v1_…
   export OPENAI_BASE_URL=https://thclaws.cloud/gateway/openai/v1
   # …ทำเหมือนกันกับ GEMINI_*, OPENROUTER_*
   ```
   (หรือใช้ช่อง `*_API_KEY` / `*_BASE_URL` ใน GUI Settings →
   Providers ก็ได้)
4. รัน thClaws ตามปกติ call จะไปผ่าน gateway ค่าใช้จ่ายโผล่ใน
   **/credit/usage**

สำหรับ workspace **hosted** gateway จะถูก wire ให้อัตโนมัติเมื่อเลือก
*Gateway* ตอนสร้าง workspace — runner จะได้ env var ที่ inject ให้
แล้วโดยไม่ต้อง copy-paste

### credit balance คือ gate เดียว

**ไม่มี tier ladder แล้ว** — ระบบ gating แบบ `starter`/`pro`/`enterprise`
เดิมถูกยกเลิกไป **มี credit เป็นบวก = เรียก model ที่ active ตัวไหนก็ได้**
จะขึ้น `402 Payment Required` ก็ต่อเมื่อ balance เหลือ `0` เท่านั้น
ส่วนต่างของราคาเป็นตัวจัดการเอง: การเรียก Opus หนัก ๆ ก็แค่ราคาต่อครั้ง
แพงกว่า Haiku จึงเผา credit เร็วกว่าตามธรรมชาติ (`model_pricing.tier`
ยังอยู่ในสคีมาแต่ใช้แค่จัดเรียงหน้า pricing — ไม่ได้บล็อกอะไร)

## Shared agent (agent บริษัทตัวเดียว หลายคนใช้ร่วมกัน)

**Shared agent** คือ agent ของบริษัทตัวเดียวที่หลายคนใช้พร้อมกัน — เช่น
บอทซัพพอร์ต ผู้ช่วยวิจัยภายใน หรือ agent ช่วย onboarding ที่อยากให้ทั้ง
ทีมใช้ร่วมกัน **โดยไม่ต้อง** ติดตั้งหรือตั้งค่าเองทีละคน เป็นฟีเจอร์ฝั่ง
**hosted cloud** (รันเป็น hosted workspace ไม่ใช่บนเครื่องคุณ) จัดการ
จากแผง **Dashboard → Shared agents**

โครงสร้างเป็นแบบนี้:

- **"สมอง" บริษัทแบบอ่านอย่างเดียวหนึ่งชุด workspace ส่วนตัวหลายชุด**
  เจ้าของอัปโหลดสมองของ agent — `AGENTS.md`, KMS บริษัท, skill, slash
  command และ `mcp.json` (เป็น *config* ของ tool ไม่ใช่ credential)
  workspace ของสมาชิกแต่ละคน mount สมองนี้แบบ **อ่านอย่างเดียว** แล้ว
  ประกอบเข้ามา ส่วนสมาชิกแต่ละคนยังมีพื้นที่ส่วนตัวของตัวเอง: ประวัติแชต
  ไฟล์ KMS เพิ่มเติมของตัวเอง และ MCP login ของตัวเอง — ซึ่งคนอื่นและ
  เจ้าของมองไม่เห็น
- **ล็อกให้ตรงกับชุดของบริษัท** ในโหมด shared engine จะอ่านคำสั่งจาก
  `AGENTS.md` ของบริษัท **เท่านั้น** (`AGENTS.md` / `~/.config` /
  `~/.claude` ของสมาชิกถูกข้าม) และเจ้าของ pin model ได้ ทำให้ทุกคนใช้
  agent ตัวเดียวกันจริง ๆ
- **gateway อย่างเดียว เจ้าของจ่าย** shared agent ไม่มี BYOK และไม่มี
  `.env` — inference ทั้งหมดวิ่งผ่าน gateway ของ thClaws.cloud และคิด
  เงินไปที่ **เจ้าของ** เจ้าของตั้ง **เพดานใช้จ่ายต่อสมาชิกต่อวัน**
  (หน่วยเซ็นต์ รีเซ็ตทุกวันตาม UTC) บนเพดานรวมต่อ workspace ต่อวันอีกชั้น
  สมาชิกที่ถึง cap จะถูกบล็อกจนกว่าจะรีเซ็ต กันไม่ให้คนเดียวใช้จนบิลบานปลาย
- **อ่านอย่างเดียว = ต้อง fork ถ้าจะปรับ** สมาชิกแก้สมองที่แชร์หรือเขียน
  ลง KMS บริษัทไม่ได้ — จะขึ้นข้อความ "shared KMS is read-only — fork to
  edit" ถ้าอยากปรับแต่งให้ **fork** shared agent ออกเป็น agent ส่วนตัว
  ของตัวเอง (เป็น catalog agent ปกติที่คุณเป็นเจ้าของและแก้ได้อิสระ)

ใน **Dashboard** แผง Shared agents จะแยก agent ที่ **คุณเป็นเจ้าของ**
(จัดการสมาชิก, cap, อัปโหลดสมอง และดู usage ได้) ออกจาก agent ที่ **ถูก
แชร์มาให้คุณ** (แค่ launch แล้วใช้) เจ้าของเพิ่มสมาชิกด้วย handle ตั้ง cap
ของแต่ละคน อัปโหลดหรือรีเฟรชสมอง (`brain.tgz` ที่มี `AGENTS.md`, `kms`,
`skills`, `commands`, `mcp.json` — หรือสร้างจาก agent ที่มีอยู่แล้ว) และ
ดูค่าใช้จ่ายรายคนใน usage breakdown

## สรุปอ้างอิงคำสั่ง

catalog op ทุกอันรันใน thClaws session ที่เปิดอยู่ — ถ้ารัน
`thclaws cloud …` แบบ CLI เก่าทุกตัวจะ print error ชี้ไป
slash-command equivalent

| คำสั่ง | ที่ใช้ | ทำอะไร |
|---|---|---|
| Settings → **thClaws.cloud** | GUI | Cloud URL + CLI token (paste / clear) — ทางเดียวที่ใช้ login/logout |
| `/cloud status` | In-session slash | แสดง URL ที่ resolve + state ของ token |
| `/cloud list [--mine]` | In-session slash | browse catalog |
| `/cloud get <slug>` | In-session slash | ติดตั้งลง cwd ของ session (abort ถ้า folder ไม่ว่าง/UUID ไม่ตรง) |
| `/cloud publish` | In-session slash | อัปโหลด cwd ของ session |
| `/cloud unbind` | In-session slash | ล้าง `agent.uuid` ให้ publish ครั้งต่อไปสร้าง row ใหม่ใน catalog |
| `/cloud revision` (หรือ `rev`) | In-session slash | revision ของฝั่งเครื่องเทียบกับ cloud + folder นี้ dirty ไหม อ่านอย่างเดียว ไม่ปลุก workspace ที่หลับ |
| `/cloud push` / `/cloud pull` | In-session slash | sync ไฟล์กับ hosted workspace `--dry-run` · `--delete` · `--force` · `--force-rebind` · `--workspace <slug>` |
| Settings → **Agent identity** | GUI | แก้ `agent.name` / `description` ของ folder นี้ |
| `/credit` (web) | Catalog UI | เติม credit + ดู balance + ดูราคา model |
| `/gateway/keys` (web) | Catalog UI | mint access key `gw_v1_…` |
| `/credit/usage` (web) | Catalog UI | ค่าใช้จ่ายรายการ + แยกตาม workspace |

## thClaws.cloud ไม่ใช่อะไร

ตั้งความคาดหวังเรื่องสำคัญสองสามข้อ:

- **ไม่ใช่ที่ host model** Agent ใน catalog ยังคงเรียก inference จาก
  Anthropic / OpenAI / Gemini อยู่ — ผ่านทั้ง BYOK key ของคุณเอง หรือ
  cloud gateway ในฐานะ proxy เก็บเงิน thClaws.cloud ไม่ได้ train หรือ
  serve LLM เอง
- **ไม่ใช่ที่เก็บ session** ประวัติแชทยังคงอยู่ใน
  `./.thclaws/state/sessions/` บนเครื่องที่ run agent ตัวนั้น cloud เก็บ
  ไฟล์ agent ไม่ใช่ประวัติบทสนทนา
- **ไม่จำเป็นต้องใช้** ทุกบทก่อนหน้าบทนี้ทำงานได้โดยไม่ต้องใช้
  network เลย cloud เป็นของเสริม — ติดตั้ง thClaws เขียน `AGENTS.md`
  ก็ได้ agent ใช้งานได้แล้วโดยไม่ต้องสมัครอะไร
