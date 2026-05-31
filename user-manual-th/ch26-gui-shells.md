# บทที่ 26 — GUI Shells

GUI Shell ให้คุณเปลี่ยน view ของ Chat / Terminal เป็น **frontend
HTML ที่ออกแบบเฉพาะ domain** — grid สำหรับ image generation,
dashboard trading, ตัวสร้าง campaign โฆษณา หรืออะไรก็ได้ Shell
ถูก render อยู่ใน iframe ที่ sandbox และคุยกับ agent ผ่าน bridge
เล็ก ๆ ชื่อ `window.thclaws.*` Built-in shell มากับ thClaws ส่วน
custom shell คือ folder ที่คุณวางลงดิสก์ shell ตัวเดียวกันยัง
serve ขึ้น cloud ที่ URL พร้อม token ได้ด้วย ทำให้ใช้จาก browser
บนมือถือหรือแชร์ให้เพื่อนร่วมทีมได้

> **สถานะ:** Tier 1 ลง v0.24 (Session Explorer + tab loader);
> Tier 2 เพิ่ม picker, custom shell, และ `--serve --gui-shell`;
> Tier 3 เพิ่ม SDK, permission, และ marketplace ดู
> [dev-plan/33](../dev-plan/33-gui-shell.md) สำหรับ roadmap เต็ม
> หัวข้อด้านล่างจะติด tag ว่าแต่ละความสามารถลงที่ tier ไหน

## ควรใช้ GUI Shell เมื่อไร

ใช้ GUI Shell เมื่องานมี **วิธีแสดงผลที่ดีกว่า chat transcript**
และ user อยากโต้ตอบผ่านการแสดงผลนั้น ไม่ใช่พิมพ์ prompt:

- สร้างรูป — grid ของรูปที่สร้างก่อนหน้าดีกว่า scroll chat เพื่อ
  ดูว่า "ฉันสร้างอะไรไปบ้าง"
- รีวิว agent session ยาว ๆ — tree ของ tool-call ดีกว่า scroll
  เป็นเส้นตรงเพื่อหา "มันเรียก `bq_query` ที่ไหน"
- สร้าง ad campaign — form ของ targeting filter ดีกว่าพิมพ์
  อธิบายเป็นข้อความ

อยู่กับ **Chat** (บทที่ 4) ต่อไปเมื่อ workflow เป็นแบบสนทนาและเป็น
text เป็นหลัก อยู่กับ **Terminal** (บทที่ 4) เมื่ออยากได้ raw
ANSI stream GUI Shell เป็นของเพิ่ม ไม่ใช่ของแทนที่ทั้งคู่

## สอง delivery mode

ทุก shell รันได้ใน 2 ที่ ผู้เขียน shell เขียน code ชุดเดียว
ผู้ใช้เลือก surface เอง

| Mode | รันที่ไหน | URL surface | Auth | Bridge transport |
|---|---|---|---|---|
| **A** Desktop tab | thClaws GUI app | `thclaws://` custom protocol | desktop session | `window.ipc.postMessage` |
| **B** Serve / cloud | `--serve` listener | `https://host/t/<token>/` | per-shell token ใน path | WebSocket |

Mode A เป็น default Mode B (Tier 2) ใช้สำหรับเรียก shell จากที่
อื่น — มือถือ เพื่อนร่วมทีม หรือ server แบบ headless

---

## Mode A — เปิด shell ใน desktop GUI

### Tier 1 — built-in อย่างเดียว

1. เปิด thClaws (`thclaws` หรือ `cargo run --features gui --bin
   thclaws`)
2. คลิก **"+ New Tab" → "Open Session Explorer"** (Tier 1 ส่ง
   built-in shell ตัวเดียวต่อตรงเข้า new-tab menu ส่วน picker
   สำหรับเลือกระหว่าง shell ลง Tier 2)
3. Tab จะเปิดพร้อม UI ของ Session Explorer คลิก session ทาง
   ซ้าย คลิก node ของ tool-call ใน tree เพื่อกาง คลิก
   "Summarise" ให้ agent อธิบาย call นั้นใน 1 บรรทัด
4. ปิด tab → session ของ shell ถูก persist ที่
   `./.thclaws/sessions/<id>.jsonl` (ที่เดียวกับ session ของ
   Chat/Terminal เพิ่ม field `shell: { id, version }` ใน
   metadata — ยัง `cat` ได้ปกติ)
5. เปิดใหม่ภายหลัง → เลือก session เดิมจาก Sessions browser
   shell จะเปิดพร้อม state เดิม

### Tier 2 — picker + custom shell

หลัง Tier 2:

1. **"+ New Tab" → "GUI Shell"** เปิด **picker grid** แสดง shell
   ทุกตัวที่ติดตั้ง — built-in, user-level (`~/.config/thclaws/
   gui-shell/`), และ project-level (`./.thclaws/gui-shell/`)
2. ทุก card แสดง icon, ชื่อ, version, source (`builtin` / `user`
   / `project`), permission ที่ประกาศไว้ และ session ที่ผ่านมา
   ของ shell นั้น (resume ได้คลิกเดียว)
3. คลิก card → shell ที่เลือกแทนที่ picker ใน Shell tab — Mode A
   เปิดได้ทีละ shell ใน v0.24, multi-instance shell tabs ยังอยู่ใน
   release ถัดไป (ส่วน multi-tenant `--serve` สำหรับ host shell
   เดียวให้ผู้ใช้หลายคนได้ลงแล้ว — ดูหัวข้อ "Multi-tenant" ด้านล่าง)
   ใช้ breadcrumb "shells" เพื่อกลับมา picker แล้วเลือก shell อื่น
4. ปุ่ม **"Refresh shells"** rescan folder discovery โดยไม่ต้อง
   restart thClaws

### ตั้ง default shell

ถ้าอยากให้ shell ตัวใดตัวหนึ่งเปิดเสมอเมื่อคลิก "New GUI Shell"
ตั้งใน `settings.json`:

```jsonc
// ./.thclaws/settings.json  (project — ชนะ)
// หรือ ~/.config/thclaws/settings.json  (user — fall through)
{ "guiShell": "session-explorer" }
```

แบบยาว ใช้เมื่อ default ของ desktop กับ serve ต่างกัน:

```jsonc
{
  "guiShell": {
    "tabDefault":   "session-explorer",   // ใช้กับ Mode A "New Shell"
    "serveDefault": "my-image-bot"      // ใช้กับ fallback ของ Mode B --serve
  }
}
```

---

## Mode B — serve shell ขึ้น cloud  *(Tier 2)*

ใช้สำหรับเปิด shell จากมือถือ แชร์ให้เพื่อนร่วมทีม หรือรันบน
server

### เรียกใช้

```sh
thclaws --serve --gui-shell my-image-bot --port 8080
```

stdout:

```
Serving My Image Bot (v0.1.0) at
  https://localhost:8080/t/abc...xyz/
Token persisted to ~/.config/thclaws/gui-shell-tokens.json
```

เปิด URL นั้นใน browser ตัวไหนก็ได้ จะมี landing flash บอก
`Connecting to: my-image-bot v0.1.0 on <host>` แล้ว shell
render เต็มหน้า — UI เดียวกับ Mode A bridge เดียวกัน แค่
WebSocket อยู่ข้างใต้แทน Tauri IPC

### Token คือ credential

- URL `https://host:8080/t/<token>/` คือทุกอย่างที่ต้องใช้ ใครมี
  ก็เข้าได้ ใครไม่มีจะได้ 404 เงียบ ๆ (server ไม่บอกด้วยซ้ำว่ามี
  shell bound อยู่)
- Token ถูกสร้างตอน launch ครั้งแรกและ **persist** ที่
  `~/.config/thclaws/gui-shell-tokens.json` key เป็น `(shellId,
  port)` restart `--serve` จะได้ URL เดิม การแชร์ครั้งเดียวเลย
  ใช้ได้นาน
- URL ตรง ๆ อย่าง `/gui-shell/session-explorer/` หรือ `/shells/`
  จะคืน 404 มีแค่ shell ที่ launch ด้วย `--gui-shell` เท่านั้นที่
  เข้าถึงได้ และเข้าได้ผ่าน `/t/<token>/` เท่านั้น

### Pin token (สำหรับ deployment)

สำหรับ k8s manifest หรือ systemd unit ที่ต้องการ URL คงที่:

```sh
thclaws --serve \
        --gui-shell my-image-bot \
        --gui-shell-token "$MY_TOKEN" \
        --gui-shell-token-ttl 90d \
        --port 8080
```

### Rotate

ถ้า URL หลุดหรืออยากยกเลิกการแชร์:

```sh
thclaws shell rotate-token my-image-bot
# → พิมพ์ URL ใหม่ URL เก่าหยุดทำงานทันที
```

### No-auth mode (localhost / intranet เท่านั้น)

```sh
thclaws --serve --gui-shell my-image-bot --gui-shell-no-auth
```

Route ขึ้นที่ `/` ตรง ๆ — ไม่มี prefix `/t/<token>/` default จะ
ปฏิเสธการ bind บน address ที่ไม่ใช่ loopback ถ้าอยากเปิด
unauthenticated บน public IP (ควรรู้ว่ากำลังทำอะไร — ปกติต้องมี
auth proxy ของคุณเองอยู่หน้า):

```sh
thclaws --serve --gui-shell my-image-bot \
        --gui-shell-no-auth --gui-shell-no-auth-allow-public \
        --bind 0.0.0.0 --port 8080
```

Pattern guardrail เดียวกับ `--dangerously-skip-permissions`
(บทที่ 5)

### Serve default จาก `settings.json`

ถ้าไม่ใส่ `--gui-shell` launcher จะอ่าน `guiShell.serveDefault`
(หรือ shorthand `guiShell` ถ้าเป็น string) จาก `settings.json`
ถ้าไม่ได้ตั้ง `--serve` จะทำงานเดิม — serve React frontend ปกติ

### Multi-tenant — shell เดียว, ผู้ใช้หลายคน

ทุกอย่างข้างต้น ("Mode B") เป็น **single-tenant** — ทุกคนที่เข้า
URL เดียวกันจะแชร์ agent / session / storage ก้อนเดียว
เหมาะตอนแชร์ shell ให้เพื่อนร่วมทีมหรือใช้กับมือถือตัวเอง

ถ้าอยาก *host* shell ให้ผู้ใช้หลายคน — ต่างคนต่างมีบทสนทนา
gui-shell storage และไฟล์ output ของตัวเอง — เพิ่ม `--multi-tenant`
กับ HMAC secret ที่แชร์กัน:

```sh
thclaws --serve --gui-shell my-image-bot \
        --multi-tenant \
        --multi-tenant-secret "$THCLAWS_CLOUD_HMAC_SECRET" \
        --port 8080
```

(`--multi-tenant-secret` รับจาก env `THCLAWS_CLOUD_HMAC_SECRET`
ได้ด้วย — รูปแบบที่ใช้ตอน deploy จริง)

โหมดนี้คาดว่า request มาจาก routing layer ที่เชื่อถือได้
(ปกติคือ thClaws.cloud) ซึ่งจะแนบ 3 header ที่เซ็นแล้วมาทุก request:

```
X-Thclaws-User:       <user_id>           # filesystem-safe, [a-zA-Z0-9_-], ≤64 ตัว
X-Thclaws-User-Ts:    <unix_seconds>
X-Thclaws-User-Proof: hex(HMAC-SHA256(secret, "<user_id>:<ts>"))
```

สิ่งที่จะได้:

- **Agent + session แยกต่อ user** — alice กับ bob ใน pod เดียวกัน
  ต่างคนต่างมีบทสนทนา
- **Storage แยกต่อ user** — `thclaws.storage.set("notes", …)` ของ
  alice ไป `users/alice/storage/<shell>/…` ของ bob ไป
  `users/bob/...` ไม่ชนกันบน key เดียวกัน
- **Output แยกต่อ user** — ไฟล์ที่ agent สร้างไปอยู่ที่
  `output/users/<id>/...` และ file-asset URL จะไม่ serve subtree
  ของ user อื่นแม้จะเดา URL ได้
- **LRU + idle eviction** — `--multi-tenant-max-users 1000` (default)
  กับ `--multi-tenant-idle-timeout 30m` (default) คุม resource
- **Restart-resumable** — session JSONL ของ alice รอด pod restart
  เมื่อ alice เชื่อมต่อใหม่บทสนทนาเดิมจะโหลดกลับมาจาก disk

Shell author เขียน **shell เหมือนเดิม** เป๊ะกับ single-tenant Mode B
— ไม่ต้องแก้ code อะไร bridge จะ route storage / file-asset
ผ่าน prefix ต่อ user ให้อัตโนมัติ

นี่คือสิ่งที่อยู่เบื้องหลัง thClaws.cloud (dev-plan/34)
สำหรับ contract เต็ม — สูตรเซ็น HMAC, layout บน disk, semantics
ของ registry, curl smoke recipe, และสิ่งที่ Tier 1 ยังไม่มี
(object storage, cross-pod state portability, cgroup-style
resource limits) — ดู
[`thclaws-technical-manual/multi-tenant-serve.md`](../thclaws-technical-manual/multi-tenant-serve.md)

---

## ติดตั้ง custom shell  *(Tier 2)*

Shell ก็คือ folder วางที่ใดที่หนึ่งใน 2 ที่:

```
~/.config/thclaws/gui-shell/<id>/      # cross-project ทุก workspace เห็น
./.thclaws/gui-shell/<id>/              # repo-scoped project override โดย id
```

Folder ต้องมี:

```
<id>/
  manifest.json         # ดูด้านล่าง
  index.html            # entry point — bridge ถูก inject ตอน serve
  ...                   # CSS / JS / รูป / font อะไรก็ได้
```

`manifest.json` ขั้นต่ำ:

```json
{
  "id": "hello-shell",
  "name": "Hello Shell",
  "version": "0.1.0",
  "description": "Smallest possible shell.",
  "entry": "index.html",
  "icon": "icon.svg",
  "minBridgeVersion": "1",
  "permissions": ["agent.run"]
}
```

ใน GUI: เปิด picker คลิก **"Refresh shells"** — shell ของคุณจะ
ปรากฏข้าง built-in

**Project shell จะ override user shell** ที่ id เดียวกัน เหมาะ
เมื่อทีมอยากแจกเวอร์ชัน customise ของ public shell ให้ทุกคนใน
repo

### Tier 3 — ติดตั้งจาก git URL

```sh
thclaws shell install https://github.com/someone/cool-shell
thclaws shell install ./mything --scope project   # default: user
thclaws shell list
thclaws shell remove cool-shell
```

ตอน install ครั้งแรก จะมี permission prompt สรุปสิ่งที่ shell
ประกาศว่าต้องใช้:

> *"Shell ตัวนี้ต้องการ: รัน agent, เรียก
> `mcp__pinn_ai__text2image`, เก็บข้อมูลใน
> `<shell-root>/state/`, อ่าน session ของคุณ อนุญาตหรือไม่?"*

Grant ถูก persist ที่ `~/.config/thclaws/gui-shell-grants.json`
(user-scoped — เพื่อนที่ clone repo ไม่ inherit การตัดสินใจของ
คุณ) เพิกถอนได้จาก context menu ของ picker หรือผ่าน
`thclaws shell remove`

---

## เขียน shell ของตัวเอง  *(Tier 3)*

Shell คือ HTML + CSS + JS ไม่ต้องมี build step

### Starter template

```sh
git clone https://github.com/thclaws/gui-shell-template my-shell
cd my-shell
make dev          # ใต้ฮูด: thclaws shell dev .
```

`make dev` mount folder ของคุณเป็น shell ชั่วคราวพร้อม file-watch
+ auto-reload แก้ `index.html` / `main.js` / `manifest.json`
save แล้ว iframe refresh เอง ไม่ต้อง rebuild thClaws

### Bridge — `window.thclaws.*`

JavaScript ของ shell จะได้ global ตัวเดียว ทุกอย่างเป็น async

```js
// Identity
thclaws.shell.id          // "hello-shell"
thclaws.shell.sessionId   // session ที่ tab นี้ bound กับ
thclaws.transport         // "tauri" (Mode A) หรือ "ws" (Mode B)

// รัน agent — loop เดียวกับที่ขับ Chat/Terminal
const { runId } = await thclaws.run("Summarise this in one line.");

// ยกเลิก turn ที่กำลังรัน (เทียบเท่า Cmd+. ใน Chat)
thclaws.cancel(runId);

// Subscribe event streaming
const unsubscribe = thclaws.on("text", (chunk) => render(chunk));
thclaws.on("tool_call",   (call)   => …);   // Tier 2
thclaws.on("tool_result", (result) => …);   // Tier 2
thclaws.on("done",        ()        => …);
thclaws.on("error",       (err)     => …);

// เรียก tool ตรง ๆ — bypass agent loop สำหรับ action ที่ deterministic
// (Tier 2; manifest ต้องประกาศ `tools.invoke:<name>` ใน Tier 3)
// `<name>` คือ tool ที่ register แล้ว — ส่วนใหญ่จะเป็น MCP tool
// เช่น `mcp__pinn_ai__text2image` (sanitised จาก server name) หรือ
// built-in เช่น `Ls` ส่วนใหญ่แนะนำให้ใช้ thclaws.run() + AGENTS.md
// แทน — ใช้ provider stack ของ user ได้ทันที
const result = await thclaws.tools.invoke("mcp__your_server__your_tool", { … });

// Storage ของ shell แยกตาม session
// (Tier 2; เก็บเป็นไฟล์ที่ <shell-root>/state/<sessionId>.json)
await thclaws.storage.set("last_query", query);
const last = await thclaws.storage.get("last_query");
```

Bridge คือ **API ทั้งหมด** Shell แตะ filesystem ของ workspace
ไม่ได้ แตะ network ไม่ได้ (ถ้าไม่ประกาศ `network.outbound:<host>`
ใน Tier 3) และแตะ storage ของ shell อื่นไม่ได้ namespace
`storage` ของ shell 2 ตัวแยกกันโดย id

### Permission (Tier 3)

ประกาศใน `manifest.json::permissions` ว่า shell ทำอะไรบ้าง:

| Permission | อนุญาตให้ |
|---|---|
| `agent.run` | เรียก `thclaws.run()` และ subscribe event |
| `tools.invoke:<name>` | เรียก `thclaws.tools.invoke("<name>", …)` ตรง ๆ ทีละ tool |
| `session.read` / `session.list` | อ่านข้อมูล session sidecar |
| `fs.shell-scoped` | read/write ภายใน root ของ shell ตัวเอง |
| `network.outbound:<host>` | `fetch()` ไปยัง host นั้น (CSP inject ตอน serve) |

User จะเห็น list นี้ก่อนติดตั้ง อะไรที่ไม่ประกาศจะ throw ตอน call

### Doctor

```sh
thclaws shell doctor my-shell
# ตรวจ: manifest ถูกต้อง, entry มีจริง, permission สมเหตุสมผล,
# ไม่มี Tauri-only API ที่จะพังใน Mode B, ไม่มี external link ที่
# ทำให้ token leak ทาง Referer
```

---

## Session และ persistence

Shell session คือ session ของ thClaws ปกติ format JSONL เหมือนกัน
ที่เดียวกัน (`./.thclaws/sessions/<id>.jsonl`) กลไก `--resume`
เดียวกัน เพิ่มแค่ field `shell: { id, version }` ใน session
header ที่เป็น optional — session ที่ไม่ใช่ shell ยังเขียน JSONL
เหมือนเดิมทุกตัวอักษร ดังนั้น `cat` ยังใช้ได้กับทุก session

```sh
# ดู session ของ shell เหมือน session อื่น
cat ./.thclaws/sessions/sess-abc123.jsonl | head -3
# {"type":"header","id":"sess-abc123","shell":{"id":"image-generator","version":"0.1.0"},…}
# {"type":"user","content":"generate a picture of a sunset"}
# {"type":"assistant","content":[…]}
```

ปิด tab shell → session ถูก persist เปิดใหม่จาก "Past sessions"
ใน picker → resume ได้ session ที่ stamp `shell.id` ไว้จะเปิดได้
ใน shell ตัวนั้นเท่านั้น ไม่มี view fallback แบบ chat ทั่วไปใน
v1 (เป็น open question ระดับ Tier 3+)

---

## เรื่อง cost

Shell ที่เรียก `thclaws.run()` กิน token เท่ากับ turn ของ Chat
tab Shell ที่เรียก `thclaws.tools.invoke()` ตรง ๆ ข้าม agent loop
ทั้งหมด — ไม่กิน model token สำหรับ call นั้น แค่ค่า tool เอง
(เช่น provider image generation คิดเงิน)

ใน Tier 3 manifest ประกาศ daily token budget ได้ และ permission
prompt จะแสดง ("อนุญาตให้ใช้ได้ถึง 50k tokens/day หรือไม่?") กลไก
budget accounting เดิมจะ track usage shell ที่ใช้เกิน budget จะ
ได้ rejected promise จาก `thclaws.run()`

---

## สิ่งที่ยังไม่มีใน Tier 1

Tier 1 ส่ง Mode A พร้อม built-in shell 1 ตัว (Session Explorer)
และ bridge surface `run` / `cancel` / `on("text"|"done"|"error")`
ช่องที่ขาดอยู่ลง Tier 2 / 3 ตาม
[dev-plan/33](../dev-plan/33-gui-shell.md):

- **ยังไม่มี picker UI** new-tab menu มีตัวเลือกเดียว ("Open
  Session Explorer") Tier 2 เพิ่ม grid
- **ยังไม่มี custom shell** discover ได้แค่ built-in ที่ฝังมา
  Tier 2 เพิ่ม discovery จาก `~/.config/thclaws/gui-shell/` +
  `./.thclaws/gui-shell/`
- **ยังไม่มี `tools.invoke` / `storage` ใน bridge** Tier 1 มีแค่
  `run` / `cancel` / `on` Tier 2 ขยาย surface
- **ยังไม่มี serve mode** Mode B (`--serve --gui-shell`) ลง
  Tier 2
- **ยังไม่ enforce permission** manifest ประกาศ permission ได้ใน
  Tier 1 แต่ไม่ check ตอน call Tier 3 enforce
- **ยังไม่มี SDK / dev mode** `thclaws shell dev` + starter
  template ลง Tier 3

---

## Security model — แต่ละ mode ป้องกันอะไรจริง

- **iframe sandbox ของ Mode A** — ทุก shell รันใน `<iframe
  sandbox="allow-scripts allow-same-origin">` shell ที่ buggy เรียก
  `document.location = "…"` จะ navigate parent GUI ไม่ได้ การแยก
  origin ระดับ shell (subdomain ใน custom protocol) กัน shell 2
  ตัวอ่าน cookie / localStorage ของกันและกัน
- **token-in-path ของ Mode B** — token 160-bit ต่อ shell 404
  เงียบเมื่อ token หาย/ผิด (ไม่บอกว่ามี auth) rate limit ต่อ IP
  ใน prefix token Referer ถูกตัด (Permissions-Policy header +
  `<meta name="referrer">`) กัน token หลุดเมื่อ shell link ออก
  ข้างนอก
- **Path traversal** — ทั้ง 2 mode เรียก `Sandbox::check_in
  (&shell_root, &rel)` ตัวเดียวกัน sequence `..` ที่ URL-decode
  แล้วจะ collapse ผ่าน lexical normalize → canonicalize →
  `starts_with` check
- **เรียก tool** — permission gating ของ Tier 3 ทำให้ shell
  เรียก tool ที่ไม่ได้ประกาศไว้ไม่ได้ Permission grant เป็น
  per-shell-per-user เก็บที่ `~/.config/thclaws/gui-shell-grants
  .json` revoke ได้จาก picker

สิ่งที่ **ไม่** ได้ป้องกัน:

- ผู้เขียน shell คุณกำลัง trust code ของเขากับ agent session ของ
  คุณ ไม่มีการ verify จาก marketplace ใน v1 Tier 3 เพิ่ม
  marketplace catalog kind แต่ governance สุดท้ายขึ้นกับว่าคุณ
  ติดตั้งจากใคร
- การเปิดสู่ network ของ `--gui-shell-no-auth-allow-public` flag
  ตั้งชื่อแบบนี้มีเหตุผล — อ่านบทที่ 5 ก่อน

---

## ตารางอ้างอิงเร็ว

| เป้าหมาย | คำสั่ง / ที่ตั้ง |
|---|---|
| ลอง Session Explorer ตอนนี้ (Tier 1) | thClaws GUI → New Tab → Open Session Explorer |
| เปิด shell picker (Tier 2) | thClaws GUI → New Tab → GUI Shell |
| ตั้ง shell default ของ "New Shell" | `"guiShell": "<id>"` ใน `settings.json` |
| ติดตั้ง shell คนอื่น (manual) | วาง folder ใน `~/.config/thclaws/gui-shell/<id>/` → Refresh |
| ติดตั้งจาก git (Tier 3) | `thclaws shell install <git-url>` |
| Serve shell ทาง HTTP (Tier 2) | `thclaws --serve --gui-shell <id> --port 8080` |
| Pin URL ของ serve | เพิ่ม `--gui-shell-token <token>` |
| Rotate URL ที่หลุด | `thclaws shell rotate-token <id>` |
| List shell ที่ติดตั้ง (Tier 3) | `thclaws shell list` |
| เขียน shell ใหม่ (Tier 3) | clone template, `make dev` |
| ลบ shell (Tier 3) | `thclaws shell remove <id>` |
| ดู session ของ shell | `cat ./.thclaws/sessions/<id>.jsonl` |

---

## Troubleshooting

**"Tab ของ shell ว่างเปล่า / spinner ค้าง"** — เปิด WebView
devtools (`THCLAWS_DEVTOOLS=1 thclaws`) แล้วดู console ของ iframe
สาเหตุที่พบบ่อย: `index.html` ของ shell มี CSP เข้มที่บล็อก
bridge script ที่ inject เข้าไป (Tier 3 เพิ่ม field manifest
`cspMode: "managed"`) หรือ JS ของ shell throw ก่อนเรียก
`thclaws.on()` ทำให้ไม่ได้ bind event

**"URL ของ Mode B คืน 404"** — ตรวจว่า URL มี prefix
`/t/<token>/` พร้อม trailing slash หรือไม่ token ถูก print ที่
stdout ของ launcher ถ้าหาย ดูที่ `~/.config/thclaws/
gui-shell-tokens.json` URL ที่ไม่มี token จะ 404 ตาม design
(ไม่มี auth challenge บอก)

**"Shell เรียก tool ไม่ได้"** — Tier 3: manifest ไม่ได้ประกาศ
`tools.invoke:<name>` เพิ่มเข้าไป restart thClaws (หรือกด
`Refresh shells`) แล้วอนุมัติ permission ใหม่

**"Shell 2 ตัวใช้ storage ร่วมกัน"** — ไม่ควรเป็นไปได้ ตรวจว่า
`manifest.json::id` ต่างกัน `storage` namespace แยกตาม id ถ้า id
ต่างกันแล้ว storage ยัง leak อยู่ ให้แจ้ง bug — เป็นความล้มเหลว
ของ sandbox

**"Serve แบบ headless ปฏิเสธ start ด้วย `--gui-shell-no-auth`"**
— ตั้งใจ `--gui-shell-no-auth` อนุญาต bind เฉพาะ loopback เพิ่ม
`--gui-shell-no-auth-allow-public` *และ* ยืนยันอีกครั้งว่ามี auth
ของคุณเองอยู่ข้างหน้า

**"`Sandbox::check_in` ปฏิเสธ asset"** — path resolve ออกนอก
folder ของ shell ปกติเกิดจาก URL relative ที่มี `../` เยอะเกิน
หรือ symlink ชี้ออกข้างนอก ทั้ง 2 mode ใช้ check ตัวเดียวกัน —
ถ้าใน desktop tab fail ก็จะ fail ใน serve mode ด้วยเหตุผล
เดียวกัน
