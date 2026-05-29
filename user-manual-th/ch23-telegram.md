# บทที่ 23 — Telegram bot

ขับ thClaws จาก Telegram สร้าง bot ด้วย `@BotFather` เอา token มา
วางใน thClaws แล้วทุก message ที่คุณ DM หา bot จะรันเป็น turn บน
desktop ของคุณ — tool registry เต็มชุด (Bash, Edit, KMS, MCP,
skills) รันในเครื่อง แล้ว stream คำตอบกลับมาเป็น Telegram message
tool call ที่ต้อง approve จะโผล่เป็นปุ่ม inline keyboard ให้แตะจาก
มือถือ (dev-plan/29 Tier 1)

## ทำไมเป็น Telegram (และต่างจาก LINE ยังไง)

[LINE bridge](ch21-line-and-browser-chat.md) ต้องมี relay server
(`line.thclaws.ai`) เพราะ LINE ส่ง message ด้วยการ push HTTPS
webhook ไปที่ endpoint ที่ต้องมีคน host เท่านั้น แต่ Bot API ของ
Telegram เปิด **long-polling** (`getUpdates`) ซึ่งทำงานหลัง NAT
ได้สบาย — thClaws เลยคุยกับ `api.telegram.org` **โดยตรง** ไม่ต้องมี
relay ไม่ต้องรัน server ไม่มี third party คั่นกลางใน message path
และนอกประเทศไทย Telegram ก็เป็น default ที่คนใช้มากกว่า

desktop ไม่หายไปไหน — code, secret, tool ทั้งหมดยังอยู่ในเครื่อง
Telegram เป็นแค่ surface สำหรับ chat เท่านั้น

## ทำงานยังไง (พารากราฟเดียว)

ตอน connect thClaws จะเปิด long-poll loop ไปที่ `api.telegram.org`
ดึง message ใหม่มาเรื่อย ๆ ตามที่เข้ามา message ที่ผ่านการ
authorize จะถูกป้อนให้ agent บน desktop แล้ว assistant text สุดท้าย
จะถูก HTML-escape, ตัดเป็นชิ้นให้พอดีลิมิต 4096 ตัวอักษรของ
Telegram แล้วส่งกลับด้วย `sendMessage` tool ที่แก้ไขสถานะระบบจะหยุด
turn ไว้แล้วโพสต์ inline keyboard (**Allow / Always / Deny**) การ
แตะของคุณจะปลดล็อก gate แล้วแก้ message เดิมให้แสดงผลลัพธ์ที่เลือก

## การตั้งค่า

### 1. สร้าง bot

ใน Telegram เปิด chat กับ **`@BotFather`** แล้วส่ง `/newbot` ทำตาม
ขั้นตอน (ตั้งชื่อและ username ที่ลงท้ายด้วย `bot`) BotFather จะตอบ
**token** กลับมาหน้าตาแบบนี้:

```
<your-bot-id>:<token-from-botfather>
```

(รูปแบบ: ตัวเลข แล้วตามด้วย `:` แล้วตัวอักษรประมาณ 35 ตัวจากชุด `[A-Za-z0-9_-]`)

เก็บเป็นความลับ — token คือ API key เต็มของ bot

### 2a. connect จาก GUI

1. เปิด **Settings → Telegram Connect…**
2. วาง bot token แล้วกด **Connect** thClaws จะ validate กับ Telegram
   (`getMe`) แล้วเริ่ม poll; sidebar จะแสดง pill **Telegram** พร้อม
   `@username` ของ bot
3. **กด approve ที่ thClaws ก่อน** การ connect เริ่ม bridge แล้วก็จริง
   แต่ยังไม่มีใครอยู่ใน allowlist (DM policy เริ่มต้นคือ `pairing`) bot
   จึงยังไม่ตอบใคร — *รวมถึงตัวคุณเอง* — จนกว่าจะ approve ให้ DM หา bot
   จากมือถือ จะมี **pairing request** โผล่ขึ้นใน Telegram Connect modal
   พร้อมปุ่ม **Approve** กดปุ่มนั้นแล้วถึงจะเริ่มคุยได้ ดูหัวข้อ
   **Pairing** ด้านล่างสำหรับ flow เต็ม

### 2b. …หรือรันแบบ headless

ไม่ต้องใช้ GUI — ตั้ง token ใน environment แล้วสั่งรัน bot loop:

```bash
export TELEGRAM_BOT_TOKEN="123456789:AA…"
export TELEGRAM_OWNER_ID="<Telegram user id ที่เป็นตัวเลขของคุณ>"   # ไม่บังคับแต่แนะนำ
thclaws --telegram
```

`--telegram` รัน agent loop ของตัวเอง (GUI worker ใช้ได้เฉพาะบน
desktop) พิมพ์ `connected as @yourbot` แล้วเสิร์ฟ message จนกด
Ctrl-C ใช้ `.thclaws/settings.json` ของโปรเจกต์เดียวกับ REPL

> **หา user id ของตัวเอง:** ทัก `@userinfobot` (หรือ bot "what's my
> id" ตัวไหนก็ได้) บน Telegram `TELEGRAM_OWNER_ID` จะเพิ่มคุณเข้า
> allowlist ตั้งแต่ตอน start ทำให้ DM bot ได้ทันที — headless ไม่มี
> GUI ให้กด approve pairing code (ดูด้านล่าง)

## Pairing — ใครได้รับอนุญาตให้คุยกับ bot

ใครก็ตามที่รู้ `@username` ของ bot ทัก bot ได้ ฉะนั้นโดย default
thClaws จะ **ไม่** ตอบคนแปลกหน้า DM policy เริ่มต้นคือ `pairing`:

1. user ใหม่ DM หา bot
2. bot ตอบ: *"คุณยังไม่ได้ pair รหัส pairing ของคุณคือ `123456`
   ขอให้เจ้าของ thClaws approve ให้"*
3. ใน **Settings → Telegram Connect…** เจ้าของจะเห็น request (ชื่อ +
   รหัส) พร้อมปุ่ม **Approve** / **Reject**
4. พอกด **Approve** user id จะถูกเพิ่มเข้า `allowFrom` เซฟลงดิสก์
   แล้ว bot จะ DM ไปว่า *"You're approved!"*

รหัสหมดอายุใน **1 ชั่วโมง** ในโหมด headless ไม่มี GUI ให้กด approve —
ใช้ `TELEGRAM_OWNER_ID` (เข้า allowlist ทันที) หรือใส่ `allowFrom` ใน
ไฟล์ config ไว้ล่วงหน้า

ตั้ง `dmPolicy: "allowlist"` แทนถ้าอยากให้เพิกเฉยคนแปลกหน้าเงียบ ๆ
โดยไม่มี pairing prompt เลย

## approve tool call จากมือถือ

ขณะที่ Telegram connect อยู่ permission mode ตอนรันคือ
`telegramgated` (ดู [บทที่ 5](ch05-permissions.md)) — ความหมาย
เหมือน `ask` แต่ approval prompt **ทุกอัน** จะ route ไปที่ Telegram
chat ของคุณ ไม่ว่าจะพิมพ์ request ต้นทางจาก surface ไหน (Terminal,
Chat, REPL, Telegram) bot จะโพสต์:

```
🔐 thClaws wants to run: Bash

Input: {"command":"ls -la ~/Downloads"}

Tap a button (auto-denies in 60s).
[ ✅ Allow ] [ ♾️ Always ] [ 🚫 Deny ]
```

- **Allow** — รันครั้งนี้ครั้งเดียว
- **Always** — รันครั้งนี้และทุกครั้งถัด ๆ ไปใน session นี้ (= "allow
  for session")
- **Deny** — agent ได้รับการปฏิเสธแล้วทำ turn ต่อ

หลังแตะ ปุ่มจะหายไปและ message ถูกเขียนใหม่ให้แสดงผลลัพธ์ ไม่แตะ
ภายใน **60 วินาที** จะ auto-deny พิมพ์ `approve` / `deny` แทนการแตะ
ก็ได้

**อยากเลิกให้ approval route ไป Telegram:** disconnect จาก GUI (คืน
ค่า mode `auto` / `ask` ก่อน connect) หรือสั่ง `/permissions auto`

## กลุ่ม (group)

เพิ่ม bot เข้ากลุ่ม โดย default (`groupPolicy: "allowlist"`) bot จะ
เพิกเฉยกลุ่มนั้นจนกว่าคุณจะ opt-in เพิ่ม chat id ของกลุ่ม (เลขจำนวน
เต็มติดลบ) ใต้ `groups` ใน config หรือตั้ง `groupPolicy: "open"` เพื่อ
เสิร์ฟทุกกลุ่มที่ bot ถูกเพิ่มเข้าไป ใน Tier 1 กลุ่มหนึ่งใช้ session
เดียว (ไม่แยกราย user) ส่วน **channel** แบบ broadcast และ
forum-topic routing เป็นเรื่องของ tier ถัดไป (ดูด้านล่าง)

> bot ใน Telegram โดย default จะรับ message ในกลุ่มเฉพาะตอนถูก
> mention หรือเป็น command ("privacy mode") ปรับใน BotFather
> (`/setprivacy`) ถ้าอยากให้ bot เห็น text ทั้งหมดในกลุ่ม

## การตั้งค่า (Configuration)

state ตอนรันอยู่ใน `~/.config/thclaws/telegram.json` (เขียนโดย GUI
modal) โปรเจกต์ใส่ block `telegram` ใน `.thclaws/settings.json` ก็ได้
ฟิลด์:

```json
{
  "enabled": true,
  "botToken": "123456789:AA…",
  "dmPolicy": "pairing",
  "allowFrom": ["111111111"],
  "groupPolicy": "allowlist",
  "groups": { "-1001234567890": { "label": "Team room" } },
  "outputCeiling": 4000
}
```

| ฟิลด์ | ความหมาย |
|---|---|
| `enabled` | auto-reconnect ตอน launch เมื่อเป็น `true` และมี token |
| `botToken` | token จาก BotFather **ไม่บังคับ** — env `TELEGRAM_BOT_TOKEN` ชนะค่านี้ |
| `dmPolicy` | `pairing` (default) หรือ `allowlist` |
| `allowFrom` | Telegram user id (string) ที่อนุญาตให้ DM |
| `groupPolicy` | `allowlist` (default) หรือ `open` |
| `groups` | chat id ของกลุ่มที่ allowlist → `{ label? }` |
| `outputCeiling` | ลิมิตตัวอักษรต่อ message ก่อนตัดเป็นชิ้น (default 4000) |

**ลำดับความสำคัญของ token:** env `TELEGRAM_BOT_TOKEN` → `botToken` ใน
ไฟล์ → ไม่มี การที่ env ชนะหมายความว่าคุณไม่ต้อง commit token ลงดิสก์
สำหรับงาน CI / container การตรวจก่อน upload จะปฏิเสธ token ที่ฝังมา
ใน config ที่ deploy

## CLI

```
thclaws --telegram          รัน bot แบบ headless จนกด Ctrl-C
thclaws telegram status     พิมพ์ config ที่ resolve แล้ว (ปิดบัง token)
thclaws telegram pair       พิมพ์วิธีตั้งค่ากับ @BotFather
```

`telegram status` ใช้เช็กว่า token ถูกตรวจเจอไหม:

```
$ thclaws telegram status
Telegram adapter status
  enabled:        true
  bot token:      123456789:<redacted> (present)
  dm policy:      Pairing
  group policy:   Allowlist
  allow_from:     1 user(s)
  groups:         0 allowlisted
  output ceiling: 4000 chars
```

## การจัดรูปแบบ output

- ส่งคำตอบใน **HTML parse mode** — escape แค่ `<`, `>`, `&` (foot-gun
  น้อยกว่า MarkdownV2 มาก) fenced code block กลายเป็น `<pre>` block
  ส่วน markdown อื่น ๆ แสดงตามตัวอักษร
- คำตอบยาว ๆ ถูกแบ่งเป็นหลาย message ให้ต่ำกว่า `outputCeiling`
  (default 4000) ตัวอักษร ตัดที่ขอบบรรทัดเท่าที่ทำได้ รักษา UTF-8 —
  ภาษาไทย, emoji, CJK ไม่ถูกตัดกลางตัวอักษร
- ANSI escape sequence และบรรทัด tool-call narration ของ GUI
  (บรรทัด `⏺`/`🔧`) ถูกตัดทิ้งก่อนส่ง

## ความเป็นส่วนตัวและขอบเขตความเชื่อใจ

- **ไม่มี relay** thClaws คุยกับ `api.telegram.org` ตรง ๆ ไม่มีใคร
  นอกจาก Telegram กับ desktop ของคุณใน message path
- **token คือกุญแจ** ใครมี token ก็ขับ API ของ bot ได้ ควรใช้
  `TELEGRAM_BOT_TOKEN` (env) มากกว่าเขียนลง `telegram.json` ส่วนการ
  เก็บใน keychain จะมาใน tier ถัดไป
- **LLM call ขาออกไม่ผ่าน Telegram** prompt ของคุณไป desktop →
  Anthropic / OpenAI / ฯลฯ โดยตรง Telegram แค่ขนตัว chat text
- **pairing code อยู่ใน memory, TTL 1 ชั่วโมง** restart process แล้ว
  code ที่ค้างจะหาย (DM ใหม่เพื่อขอ code ใหม่) user ที่ approve แล้ว
  ค้างอยู่ใน `allowFrom`

## ยังไม่มีใน Tier 1 (มาทีหลัง)

บทนี้อธิบาย Tier 1 — DM + กลุ่มพื้นฐาน + plain text + pairing +
approval แบบ inline keyboard สิ่งที่วางแผนไว้สำหรับ tier ถัดไป:

- **broadcast channel + linked discussion group + forum-topic
  routing** (Tier 2) — "agent research เบื้องหลังโพสต์สถานะไป channel
  ที่ฉันชำเลืองดู"
- **streaming preview edit, media (photo/document) up/download, voice
  transcription, sticker vision, webhook mode, multi-account, proxy
  support** (Tier 3)

จนกว่าจะถึงตอนนั้น photo/voice/sticker ขาเข้าจะถูกเพิกเฉย (text
อย่างเดียว)

## การแก้ปัญหา

| อาการ | สาเหตุที่น่าจะเป็น | วิธีแก้ |
|---|---|---|
| "token rejected by Telegram (401)" ตอน connect | token ผิด/หมดอายุ | copy ใหม่จาก `@BotFather` เช็ก space ท้ายบรรทัด |
| bot ไม่ตอบ DM ของคุณ | คุณยังไม่ได้อยู่ใน allowlist | approve pairing code ใน GUI หรือตั้ง `TELEGRAM_OWNER_ID` (headless) |
| bot เพิกเฉย message ในกลุ่ม | กลุ่มไม่ได้ allowlist หรือ privacy mode ของ BotFather เปิดอยู่ | เพิ่ม chat id ใน `groups` (หรือ `groupPolicy: "open"`); `/setprivacy` ใน BotFather |
| ปุ่ม approval หมดเวลา | ไม่แตะภายใน 60 วินาที | แตะใหม่ที่ request ใหม่ หรือพิมพ์ `approve` |
| headless: เตือน "no allowlisted users yet" | `allowFrom` ว่างและไม่มี owner id | `export TELEGRAM_OWNER_ID=<your id>` แล้ว restart |
| message เก่า replay ตอน start | — | ไม่เกิด: backlog ก่อน launch ถูก drain ทิ้งโดยตั้งใจ |
| bot สอง bot ตีกัน ("Conflict") | มี `getUpdates`/webhook อีกตัวรันด้วย token เดียวกัน | รัน thClaws (หรือ webhook) ตัวเดียวต่อ token |

## สิ่งที่ไม่อยู่ในบทนี้

- สถาปัตยกรรมภายใน (wire type, long-poll loop, approver state
  machine, pairing manager) — ดู
  [`telegram-bridge.md`](../../thclaws-technical-manual/telegram-bridge.md)
  ในคู่มือเทคนิค
- LINE bridge และ browser chat — [บทที่ 21](ch21-line-and-browser-chat.md)
