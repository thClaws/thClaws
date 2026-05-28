# บทที่ 24 — Facebook Page Messenger bot

ขับ thClaws จาก inbox ของ Facebook Page เชื่อม Page ครั้งเดียว แล้ว
ทุก DM ที่คนส่งหา Page จะรันเป็น turn บน desktop ของคุณ — tool
registry เต็มชุด (Bash, Edit, KMS, MCP, skills) รันในเครื่อง แล้ว
stream คำตอบกลับมาเป็น Messenger message tool call ที่ต้อง approve
จะโผล่เป็น quick-reply chip ให้แตะจากมือถือ (dev-plan/31 Tier 1)

## ทำไมเป็น Messenger (และต่างจาก Telegram ยังไง)

[Telegram bot](ch23-telegram.md) คุยกับ `api.telegram.org` ได้ตรง ๆ
เพราะ Telegram มี long-polling (`getUpdates`) ที่ทำงานหลัง NAT ได้
แต่ Messenger เป็นแบบ **webhook อย่างเดียว** — Meta ส่ง message
ด้วยการ POST HTTPS webhook ไปที่ endpoint สาธารณะเท่านั้น — Messenger
bridge เลยต้องมี **relay server** เหมือนกับ LINE bridge
([บทที่ 21](ch21-line-and-browser-chat.md)) Tier 1 ของ thClaws ใช้
relay LINE ตัวเดิม (`line.thclaws.ai`) ที่เพิ่ม route
`/messenger/webhook` เข้าไป คุณเลยไม่ต้องรัน server เอง

desktop ไม่หายไปไหน — code, secret, tool ทั้งหมดยังอยู่ในเครื่อง
relay ทำหน้าที่ส่งต่อแค่ chat text ไม่ได้คั่นกลาง prompt ที่ส่งไป
Anthropic / OpenAI

## ทำงานยังไง (พารากราฟเดียว)

ตอน connect thClaws จะเปิด WebSocket ไป relay ด้วย **binding JWT**
ที่ผูกกับ Page ของคุณ Meta จะ POST ทุก event ของ Messenger ไปที่
relay relay verify header `X-Hub-Signature-256` แล้วหา binding ของ
Page นั้น แล้ว push message ไปให้ desktop ในรูปแบบ frame
`user_message` agent ของคุณรัน turn ในเครื่อง แล้ว assistant text
สุดท้ายจะถูก strip ANSI / tool narration ตัดเป็นชิ้นตามลิมิต 2,000
ตัวอักษรของ Messenger แล้ว POST กลับไปที่
`/messenger/reply/{request_id}` ของ relay ซึ่งจะเรียก Graph **Send
API** (`messaging_type: RESPONSE`) ด้วย Page Access Token (token นี้
อยู่ที่ relay ไม่เคยอยู่บน desktop) tool ที่ต้องขออนุมัติจะหยุด
turn ไว้แล้วโพสต์ quick-reply (**Allow / Always / Deny**) การแตะของ
คุณจะปลดล็อก gate แล้ว turn เดินต่อ

## การตั้งค่า

Messenger ต้องเตรียม 2 อย่างฝั่ง operator ที่ LINE / Telegram ไม่
ต้อง — Meta app และ webhook subscription ส่วน env var ฝั่ง relay
ตั้งครั้งเดียวต่อการ deploy relay (relay ตัวจริงตั้งไว้แล้ว)

### 1. สร้าง Meta app + Page token

ที่ [Meta for Developers](https://developers.facebook.com/apps):

1. **Create App** → type **Business** เพิ่ม product **Messenger**
2. ที่ **Messenger → Settings** generate **Page Access Token**
   สำหรับ Page ที่จะใช้ ขอแบบ long-lived เก็บไว้เป็นความลับ
3. **App → Settings → Basic** copy **App Secret**
4. สุ่ม **Verify Token** ตัวหนึ่ง (เช่น `openssl rand -hex 16`) —
   ใช้แค่ตอน webhook handshake
5. หา **Page ID** เป็นตัวเลข (Page → About หรือ
   `curl 'https://graph.facebook.com/me?access_token=<PAGE_TOKEN>'`)

> **ความเป็นจริง:** ใน **Development mode** Meta จะส่ง message ให้
> เฉพาะคนที่มี role บน app (admin / developer / tester) เท่านั้น
> เพียงพอสำหรับ test end-to-end — เพิ่มตัวเองเป็น tester ก่อน
> หากต้องการ message ประชาชนทั่วไปต้องผ่าน **App Review** +
> **Business Verification** สำหรับ permission `pages_messaging` (ใช้
> เวลาเป็นวัน–สัปดาห์) อย่าให้ขั้นนี้บล็อกการทดสอบ

### 2. ชี้ webhook ของ Meta ไปที่ relay (ฝั่ง operator)

App → Messenger → Settings → Webhooks → **Add Callback URL**:

| ฟิลด์ | ค่า |
|---|---|
| Callback URL | `https://<relay>/messenger/webhook` |
| Verify Token | สตริงสุ่มเดียวกันกับขั้น 1.4 |
| Subscribe to | `messages` (อย่างน้อย) |

หลัง Meta verify URL ผ่าน ติ๊ก subscribe Page ของคุณที่
**Webhooks → Add Subscriptions → Page → messages**

ฝั่ง relay อ่าน env เหล่านี้ (production ตั้งไว้แล้วบน
`line.thclaws.ai` ส่วนนี้สำคัญเฉพาะถ้าคุณ host relay เอง):

```sh
MESSENGER_APP_SECRET=<app secret>
MESSENGER_VERIFY_TOKEN=<สตริงที่คุณสุ่ม>
MESSENGER_PAGE_ACCESS_TOKEN=<page token>
MESSENGER_PAGE_ID=<page id ตัวเลข>
```

### 3a. connect จาก GUI

1. เปิด **Settings → Messenger Connect…**
2. DM ไปที่ Page ของคุณจากบัญชี Facebook ส่วนตัวที่มี role บน app
   (admin / tester) relay จะตอบกลับเป็น **pairing code 6 หลัก** ที่
   ส่งผ่าน Send API
3. เอา code ไปวางใน modal แล้วกด **Connect** thClaws จะ exchange
   code เป็น binding JWT save ลง local แล้วเปิด WebSocket sidebar
   จะแสดง pill **Messenger** พร้อมชื่อ Page

### 3b. …หรือรันแบบ headless

Headless ใน Tier 1 ต้องมี binding JWT บน disk แล้ว (pair ผ่าน GUI
ก่อน) แล้วค่อย:

```bash
thclaws --messenger
```

`--messenger` รัน agent loop ของตัวเอง (ไม่ต้องมี GUI) พิมพ์
`connected to <relay>` แล้วรับ turn ของ Messenger ไปจนกว่าจะกด
Ctrl-C ใช้ `.thclaws/settings.json` ตัวเดียวกับ REPL

> การ redeem pairing code แบบ headless (ใส่เลข 6 หลักโดยไม่ผ่าน GUI
> modal) เป็น follow-up ตอนนี้ทำการ pair ครั้งเดียวผ่าน GUI บน
> เครื่องไหนก็ได้ แล้ว copy `~/.config/thclaws/messenger.json` ไปยัง
> เครื่อง headless ได้เลย

## Configuration

state ตอน run อยู่ที่ `~/.config/thclaws/messenger.json` (GUI modal
เป็นคนเขียน) ตั้งใจให้เล็ก — ของ sensitive ทั้งหมดอยู่ที่ relay:

```json
{
  "binding_token": "<HS256 JWT ที่ relay ออกให้>",
  "server_url": null,
  "page_name": "My Test Page",
  "page_id": "1234567890"
}
```

| ฟิลด์ | ความหมาย |
|---|---|
| `binding_token` | JWT ที่ relay ออกให้ตอน pair desktop ใช้ตัวนี้ authenticate WS และ `/messenger/reply` — *ไม่ใช่* Page Access Token |
| `server_url` | URL ของ relay `null` จะ fallback ไป `$THCLAWS_MESSENGER_SERVER` แล้ว `https://line.thclaws.ai` |
| `page_name` | ชื่อ Page ที่ cache ไว้สำหรับ GUI pill |
| `page_id` | Page ID ตัวเลขที่ cache ไว้ ใช้ sanity check event ขาเข้า |

**สิ่งที่ *ไม่* อยู่ในนี้:** Page Access Token, App Secret, Verify
Token ทั้งหมดอยู่ใน k8s Secret ของ relay ไม่เคยแตะ desktop

## CLI

```
thclaws --messenger          รัน bridge แบบ headless จนกว่าจะ Ctrl-C
thclaws messenger status     แสดง binding ที่ resolve ได้ (token โชว์แค่ prefix)
thclaws messenger pair       พิมพ์คำแนะนำการตั้งค่าฝั่ง Meta
```

`messenger status` ยืนยันว่า binding ติดตั้งถูก:

```
$ thclaws messenger status
Messenger adapter status
  relay:          https://line.thclaws.ai
  binding token:  eyJhbG… (present)
  page:           My Test Page
  page id:        1234567890
```

`messenger pair` พิมพ์ runbook ฝั่ง operator (Meta app, webhook URL,
env var, pairing handshake) — มีประโยชน์ตอน bootstrap Page ใหม่หรือ
สร้าง binding ใหม่

## การ approve tool call จากมือถือ

ตอน Messenger ต่ออยู่ permission mode จะเป็น `messengergated`
(ดูบทที่ [5](ch05-permissions.md)) — semantic เดียวกับ `ask` แต่
**ทุก** approval prompt จะถูก route ไปที่ Messenger thread เป็น
quick-reply chip Page จะ DM:

```
🔐 thClaws wants to run: Bash

Input: {"command":"ls -la ~/Downloads"}

แตะ chip ด้านล่าง (auto-deny ใน 60 วินาที)
[ ✅ Allow ]   [ ♾️ Always ]   [ 🚫 Deny ]
```

- **Allow** — รันครั้งนี้ครั้งเดียว
- **Always** — รันครั้งนี้และทุกครั้งใน session นี้ (เทียบเท่า
  "allow for session")
- **Deny** — agent ได้รับการปฏิเสธแล้ว turn ดำเนินต่อ

ถ้าไม่แตะภายใน **60 วินาที** จะ auto-deny คุณพิมพ์ `approve` /
`deny` แทนได้ quick-reply chip จะหายเมื่อ turn เดินต่อ; prompt
เก่ายังเห็นใน thread แต่ใช้ไม่ได้แล้ว

**เลิก route approval ไป Messenger:** disconnect จาก GUI (จะ
restore mode เดิมก่อน connect) หรือพิมพ์ `/permissions auto`

## รูปแบบ output

- คำตอบเป็น **plain text** — Messenger ไม่ render Markdown fence
  และตัวหนาจะโผล่เป็น literal ANSI escape ถูก strip ก่อนส่ง
- คำตอบยาวถูกแบ่งเป็นหลาย message ตามลิมิต **2,000 ตัวอักษร** (ขั้น
  hard ของ Messenger ต่ำกว่า Telegram 4,096 และ LINE 5,000) แบ่งที่
  ขอบบรรทัดถ้าทำได้ UTF-8 ปลอดภัย — ภาษาไทย, emoji, CJK ไม่ถูกตัด
  กลางตัวอักษร
- tool-call narration (`[tool: Bash …]`, ANSI status line) ถูก strip
  ก่อนส่ง ส่งเฉพาะ assistant text สุดท้ายไป Messenger

## Privacy และขอบเขตความน่าเชื่อถือ

- **relay เห็น chat text** เพราะ Messenger ต้องใช้ webhook path ของ
  Messenger ↔ desktop จึงเป็น Meta → relay → เครื่องคุณผ่าน WSS
  relay ตัวจริง log แค่ที่จำเป็นต่อการ route และ debug; prompt ที่
  ส่งไป Anthropic / OpenAI ไม่ผ่าน path นี้
- **Page Access Token + App Secret อยู่ที่ relay** ไม่เคยอยู่บน
  desktop binding JWT ใน `messenger.json` ผูกกับ Page เดียว
  revoke ที่ฝั่ง relay ได้โดยไม่ต้องแตะ Meta
- **LLM call upstream ไม่ผ่าน Messenger** prompt ไป Anthropic /
  OpenAI โดยตรงจาก desktop Messenger ส่งเฉพาะ chat ของ Page
- **pairing code เป็นตัวเลข 6 หลัก อยู่ใน memory TTL 1 ชั่วโมง** ใน
  relay; relay restart จะล้าง code ที่ค้าง (DM Page อีกครั้งเพื่อ
  ขอใหม่) binding ที่ approve แล้วเก็บใน Postgres ของ relay

## ยังไม่มีใน Tier 1 (จะมาทีหลัง)

บทนี้ครอบคลุม Tier 1 — DM + plain text + 6-digit pairing +
quick-reply approval + connect ผ่าน GUI/headless ที่จะมาเพิ่ม:

- **Page Inbox handoff** (Meta's `pass_thread_control`) ให้คนเข้า
  มาคุยต่อจาก agent ได้แบบ seamless
- **Per-PSID session routing** (Tier 1 ผูก 1 Page = 1 shared session
  ทุก end-user ที่ DM Page เดียวกันใช้ state ร่วมกัน)
- **media (รูป/ไฟล์/voice) up/download, sticker vision, streaming
  preview edit, headless pairing redemption, gateway host แบบ
  neutral**

จนกว่าจะมีให้ใช้ รูป/voice/sticker ขาเข้าจะถูกข้าม (รับเฉพาะ text)
และ approval prompt จะเล็งไปที่ **PSID ล่าสุดที่ส่ง message เข้ามา**

## Troubleshooting

| อาการ | สาเหตุที่น่าจะเป็น | วิธีแก้ |
|---|---|---|
| Meta UI ขึ้น webhook verify fail | Verify Token ไม่ตรงกัน | ดูว่า `MESSENGER_VERIFY_TOKEN` บน relay ตรงกับที่พิมพ์ใน Meta webhook form เป๊ะ ๆ |
| Page เงียบเมื่อ DM | App ยังเป็น Development mode แล้วผู้ส่งไม่ใช่ tester | เพิ่ม FB account ของผู้ส่งเป็น tester ใน App Roles หรือยื่น App Review |
| Page เงียบทั้งที่คุณเป็น tester แล้ว | webhook ยังไม่ subscribe `messages` หรือ Page ยังไม่ subscribe app | เช็ค **Webhooks → Add Subscriptions → Page → messages** ติ๊กแล้วหรือยัง |
| "binding token rejected" ตอน connect | JWT เก่า / โดน revoke | pair ใหม่ผ่าน GUI; binding row เก่า revoke ที่ฝั่ง relay ได้ |
| pairing code ไม่มา | `MESSENGER_PAGE_ACCESS_TOKEN` ที่ relay ไม่ถูก | log ของ relay จะแสดง error ของ Send API; generate token ใหม่ที่ Meta แล้ว update relay |
| คำตอบมาแต่ถูกตัด | ลิมิตต่อ message (2,000) | ตามปกติ — คำตอบยาวจะมาเป็นหลาย message Messenger รักษาลำดับให้ |
| chip approval ไม่โผล่ | `dmPolicy` กันผู้ส่งไว้ หรือ permission mode ไม่ใช่ `messengergated` | เช็ค `thclaws messenger status` + `/permissions` ใน REPL |
| message เดียวตอบหลายครั้ง | webhook re-delivery (Meta retry ตอน deliver fail) | dedup ทำที่ relay ด้วย `mid` ถ้ายังเจอ ดู log ของ relay |

## สิ่งที่ *ไม่* อยู่ในบทนี้

- ภายในของ relay (verify Meta-graph webhook, broker routing,
  Send API client, route `/messenger/{webhook,reply,push}`) —
  ดูเทคนิคัลแมนนวล
  [`messenger-bridge.md`](../../thclaws-technical-manual/messenger-bridge.md)
- LINE OA และ browser chat — [บทที่ 21](ch21-line-and-browser-chat.md)
- Telegram (long-polling ไม่มี relay) — [บทที่ 23](ch23-telegram.md)
