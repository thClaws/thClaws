# บทที่ 25 — Workflows

Workflows คือ **orchestration tier ที่สี่** ของ thClaws — model
เขียนสคริปต์ JavaScript ที่กระจายงานไปยัง subagent หลายตัว แล้ว JS
engine ในตัวสคริปต์รันแบบ deterministic บนเครื่องของคุณ ต่างจาก
subagent (บทที่ 15), `/agent` side-channel, หรือ Agent Teams (บทที่
17) ตรงที่ตัวสั่งการคือ **code** ไม่ใช่ model — ซึ่งหมายความว่ารัน
workflow เดิมซ้ำจะได้รูปทรงงานเหมือนเดิมทุกครั้ง และงานยาว ๆ จะ
เหลือ checkpoint ไว้บนดิสก์

Workflows เป็น **Tier 1** ใน v0.23 — fan-out ใช้ได้แล้ว ส่วน schema
validation กับ resume เป็นเรื่องของ Tier 2 (ดู "สิ่งที่ยังไม่มีใน
Tier 1" ด้านล่าง)

## ควรใช้ workflows เมื่อไร

ใช้ workflows กับ **งาน bulk ที่อิสระจากกันและต้องการความแน่นอน**:

- "rewrite test file 800 ไฟล์ให้ใช้ fixture ใหม่"
- "แปลทุก `.md` ใต้ `kms/bug/` เป็นภาษาไทย"
- "audit `Cargo.toml` ของแต่ละ crate แล้ว flag deps ที่ deprecated"

ใช้ `Task` tool (บทที่ 15) กับ **side-quest ที่ model ตัดสินใจสร้าง
ขึ้นมาเอง** กลางเทิร์น — นั่นคือสิ่งที่ subagent ทำต่อไป

ใช้ `/agent` (บทที่ 15) เมื่อ **คุณ** รู้ชัดว่าจะให้ specialist ทำ
อะไร และอยากให้ทำงานคู่ขนานกับ session หลัก

ใช้ Agent Teams (บทที่ 17) เมื่อ teammate ต้อง **ร่วมมือกัน** —
แลก message ถกเถียงสมมติฐาน ประสานงานบน task list ร่วมกัน
Workflows เป็น stateless fan-out ส่วน team เป็น stateful collaboration

## เริ่มใช้

```text
/workflow run summarize each .rs file under src/ in one line
```

ลำดับเหตุการณ์:

1. **Author phase** model เขียนสคริปต์ JavaScript ที่ใช้ API
   `thclaws.*` (รายละเอียด API อยู่ใน system prompt ของ model
   อยู่แล้ว ดังนั้นสคริปต์ที่ได้กลับมารู้ว่ามีอะไรให้ใช้บ้าง)
2. **Review** สคริปต์ถูก print พร้อมเลขบรรทัด แล้วถาม:
   ```text
   [a]pprove · [c]ancel · [r]e-author:
   ```
   - `a` — รันตามนี้
   - `c` — ยกเลิก
   - `r` — ใส่ note บรรทัดเดียวบอกว่าให้แก้อะไร ("ใช้ read tool ไม่
     ใช่ bash cat") แล้ว model เขียนสคริปต์ใหม่ตาม feedback วน
     จนกว่าจะกด `a` หรือ `c`
3. **Execute** แสดง workflow id (`wf-…`) จากนั้นทุก subagent call
   จะมีบรรทัด progress:
   ```text
   ✓ w0  List every .rs file under src/, recursively. Return o…   2s
   ✓ w1  Read crates/core/src/agent.rs and write ONE sentence …   3s
   ✓ w2  Read crates/core/src/repl.rs and write ONE sentence d…   4s
   …
   workflow done — 47 workers, total 1m 12s
   crates/core/src/agent.rs — the streaming agent loop
   crates/core/src/repl.rs — REPL command parser + rustyline I/O
   …
   ```

ถ้า worker error จะเห็น `✗ wN  …` และสคริปต์มักจะ catch แล้วทำงาน
ต่อ (แล้วแต่ model เขียน)

## API `thclaws.*`

สคริปต์ของคุณได้ global ตัวเดียว — `thclaws` — มี field ต่อไปนี้:

```js
thclaws.subagent({
  prompt: string,           // จำเป็น — งานของ worker
  budget?: {                // Stage G + I: enforce แล้วทั้งคู่
    time?: number | string, //   "60s" / "2m" / "1m30s" / 60 (เป็นวินาที)
    tokens?: number,        //   เพดาน input + output ต่อ worker
  },
  schema?: object,          // Stage H: JSON Schema worker ถูกขอ JSON ที่
                            //   ตรง schema เมื่อสำเร็จจะคืน parsed value
                            //   (ไม่ใช่ text)
  retry?: number | {        // Stage H: retry เมื่อ hard error + schema fail
    max: number,
    backoff?: string,       //   "exponential" / "linear" / "500ms" / ฯลฯ
  },
  caps?: {                  // Stage M: grant แบบชัดเจน — default DENY ของ KMS write
    kms?: { write?: string[] },
  },
  // model? — Stage L
}) → string | parsed_value
```

**`caps.kms.write` ควบคุมว่า worker เขียน KMS อะไรได้** นอก workflow
KMS write tool ทำงานปกติ ใน `/workflow run` worker default = **deny**
ทั้งหมด ต้องผ่าน `caps: { kms: { write: ["scratch", "audit-log"] } }`
จึงจะ grant เป็นราย call grant ถูกบันทึก `worker_caps` ใน
state.jsonl และ **ไม่ transitive** — Task spawn ของ worker เองจะได้
caps เปล่าใหม่ ถ้าไม่ grant ใหม่อีกครั้ง

Time budget ครอบ worker call ด้วย `tokio::time::timeout` พอเกินจะ
throw Schema validation รันหลังทุก attempt — ถ้า worker output ไม่
parse เป็น JSON หรือไม่ตรง schema จะ retry ตาม `retry.max` ด้วย
`backoff` ที่เลือก ทุก retry บันทึก `worker_retry` event ให้
`/workflow inspect <id>` เห็น chain Worker จะ inherit provider, model, system prompt,
tool registry, memory, KMS, และ permission mode จาก session แม่ —
ดังนั้น worker ใช้ `Bash`, `Read`, `Edit`, search KMS, MCP server
ได้หมด การ recurse ของ subagent (worker เรียก Task เอง) ถูกจำกัด
ด้วย `DEFAULT_MAX_DEPTH = 3` เหมือนกับ subagent ปกติ

**Async syntax ใช้ได้แล้ว** — script ที่ใช้ `await` / `async` /
`Promise.all` จะถูก route ผ่าน Boa Module mode `thclaws.subagent`
ยัง synchronous ภายใน (Stage J MVP) ดังนั้น `Promise.all([...])`
resolve ได้แต่ worker รันตามลำดับใน source (ทีละตัว) parallelism
จริงผ่าน tokio JobExecutor เป็นเรื่อง Stage J.2

### เขียนอะไรในสคริปต์ได้บ้าง

JS control flow: `for`, `while`, `if`/`else`, `try`/`catch`, `await`,
`async` function, `Promise.all`, destructuring, template literal,
array/string method, regex, JSON parsing

### เขียนอะไรไม่ได้

- `eval`, `Function` (ถูกปลดจาก sandbox)
- `fetch`, `require`, `process`, DOM, `console.log`

ของที่จะ I/O ต้องผ่าน subagent

### ตัวอย่างสั้น ๆ

```js
// Workflow: list .rs files, summarise each
const list = await thclaws.subagent({
  prompt: "List every .rs file under src/, recursively. Paths only."
});
const paths = list.split("\n").map(s => s.trim()).filter(Boolean);

const summaries = await Promise.all(
  paths.map(p => thclaws.subagent({
    prompt: `Read ${p} and write ONE sentence describing what it does.`
  }))
);

paths.map((p, i) => `${p} — ${summaries[i]}`).join("\n");
```

สำหรับ script แบบ sync **expression สุดท้าย** คือผลลัพธ์ ส่วน script
แบบ async (Module mode) ใช้ expression สุดท้ายที่ auto-wrapper หา
เจอ หรือใส่ `globalThis.__wf_result = …` เองก็ได้ ถ้าไม่มีทั้งสอง
อย่างจะคืน `undefined`

## State บนดิสก์

ทุกครั้งที่รัน workflow จะเขียน JSONL log ลง:

```text
.thclaws/workflows/wf-<id>/state.jsonl
```

หนึ่ง event ต่อบรรทัด flush หลังเขียนทุกครั้งเพื่อให้ Ctrl-C ไม่
ทิ้งไฟล์ค้างกลางคัน รูปแบบ event:

```jsonl
{"ts":"…","kind":"start","id":"wf-…","prompt":"…","script_sha":"…","script_chars":234}
{"ts":"…","kind":"worker_start","id":"wf-…","worker":"w0","prompt":"…"}
{"ts":"…","kind":"worker_done","id":"wf-…","worker":"w0","output":"…"}
{"ts":"…","kind":"worker_error","id":"wf-…","worker":"w1","error":"…"}
{"ts":"…","kind":"done","id":"wf-…","result":"…"}
```

`cat`, `grep`, `jq` ไฟล์ได้ตลอดเวลา — เป็น JSONL ธรรมดา ไม่มี
ฟอร์แมตปิด ไฟล์ `script.js` (JS ที่ approve แล้ว) ก็ถูกเขียนไว้
ข้างกัน เผื่อ `/workflow resume <id>` จะ replay จาก source เดิม

Slash command สำหรับจัดการ run (REPL-only ใน Tier 2):

```text
/workflow list             หนึ่งบรรทัดต่อ run จากใหม่ไปเก่า
/workflow inspect <id>     dump state.jsonl events
/workflow resume <id>      รันใหม่ replay worker ที่เสร็จจาก cache
                           ส่วน fresh spawn เลขต่อไปเรื่อย ๆ
/workflow rm <id>          ถาม y/N แล้วลบทั้ง directory
```

`resume` match ด้วย **prompt** ที่ทุก `thclaws.subagent` call cache
entry จะถูกใช้ก็ต่อเมื่อ prompt ตรง mismatch จะ fall-through ไป
spawn ใหม่ (script อาจถูกแก้หรือ path เปลี่ยน) ถ้ามี cache เหลือ
ตอนจบ script จะรายงาน "diverged" ให้รู้

ถ้า `.thclaws/` เขียนไม่ได้ (read-only volume, permission)
workflow ยังรันแต่จะ print:
```text
/workflow run: state.jsonl unavailable — proceeding without checkpoint
```
audit trail หายไป แต่ run ไม่หาย

## Headless mode

`thclaws -p "/workflow run <goal>"` **ถูกปฏิเสธ** Author phase
สร้างสคริปต์ที่ต้องให้คุณรีวิวก่อนรัน `-p` ไม่มี surface ให้รีวิว
และการ default-approve สคริปต์ที่ไม่ได้ดูเป็นเรื่องอันตราย

สคริปต์ที่เขียนไว้ล่วงหน้ารัน headless ได้ผ่าน `thclaws --workflow
<file.js>` (Stage L) — ข้าม author phase ทั้งหมด ไฟล์ผ่านการรีวิว
จาก operator แล้ว เหมาะกับ CI, cron job (บทที่ 19), deploy hook
ของ dev-plan/28

```sh
# รันใหม่:
thclaws --workflow ./scripts/audit-crates.js

# Resume จาก id เดิม (หรือ prefix):
thclaws --workflow ./scripts/audit-crates.js --resume wf-18b3fa

# stdout = ค่าสุดท้ายของ script; stderr = id + done summary
# pipe stdout เข้า jq, redirect ลงไฟล์ ฯลฯ ได้:
thclaws --workflow ./scripts/audit-crates.js > result.txt
```

Exit code = 0 เมื่อสำเร็จ, 1 เมื่อ script fail Headless mode
auto-approve tool call ทุกตัวของ subagent (เหมือน
`--dangerously-skip-permissions` — ถือว่า operator วาง script
ไว้แล้ว)

## สิ่งที่ยังไม่มีใน Tier 1

นี่คือช่องว่างที่รู้อยู่ ไม่ใช่ bug — จะมาใน Tier 2 / 3 ตาม
[dev-plan/32](../dev-plan/32-dynamic-workflows.md) (workspace-only):

- **`Promise.all` resolve ได้แต่ยังไม่ขนานจริง (Stage J MVP)** Boa
  รัน script ที่ใช้ `await` / `Promise.all` ใน Module mode แล้ว
  syntax parse ได้และ `await thclaws.subagent(...)` คืน text ของ
  worker ได้ แต่ host function ยัง block JS thread per-call ดังนั้น
  subagent call ใน `Promise.all` ก็ยังรันตามลำดับ wall clock =
  ผลรวม latency ไม่ใช่ค่ามากที่สุด Stage J.2 จะใส่
  tokio-integrated JobExecutor ให้ worker ขนานจริง
- **ยังไม่มี budget cap** Per-worker `budget: { tokens, time }`
  ignored Tier 2 จะ enforce
- **ยังไม่มี verification phase** `thclaws.verify({...})` ยังไม่มี
  — Tier 3
- **ยังไม่มี GUI worker grid** จาก chat tab `/workflow run` ถูก
  ปฏิเสธพร้อมข้อความ 1 บรรทัด UX ของรีวิวแบบ interactive ไม่
  เหมาะกับ chat bubble และ grid ของ worker progress แบบ real time
  เป็นงาน frontend ของ Tier 3

## เรื่อง cost

ทุก `thclaws.subagent` call เป็น model turn แยก — ปกติไม่กี่วินาที
และไม่กี่ร้อยถึงไม่กี่พัน token Workflow 200 worker อาจกิน $5–$20
ของ API token ได้ง่าย ๆ ขึ้นกับ model มี 2 guard ใช้งานจริง:

- **จำกัด fan-out ก่อนเขียนสคริปต์** ถ้าเป้าหมายไม่มีขอบเขต ("ทุก
  ไฟล์") ให้ discovery subagent คืน list ก่อนจะได้เห็น cardinality
  ก่อน approve สคริปต์
- **ดูบรรทัดสรุปปิดท้าย** `workflow done — N workers, total Xs, In
  tokens / Out tokens (≈$Y.YY)` พิมพ์ทุกครั้งหลัง `/workflow run`
  โมเดลที่เป็น tier-billed หรือไม่รู้ราคาจะแสดง `(cost unknown)`
  แทนตัวเลข

## ตารางอ้างอิงเร็ว

| | Subagent (`Task`) | `/agent` | Agent Teams | Workflow |
|---|---|---|---|---|
| ตัวสั่งการ | Model | คุณ (one-shot) | Team-lead model | Code |
| จำนวน worker | 1 (blocking) | 1 (concurrent) | 3–5 collaborator | สิบถึงร้อย |
| worker คุยกันเอง | ไม่ได้ | ไม่ได้ | ได้ (mailbox) | ไม่ได้ (stateless) |
| Determinism | Model-driven | Model-driven | Model-driven | Deterministic execution |
| Resume ได้ | ไม่ | ไม่ | จำกัด | บันทึก log (Tier 2 อ่านกลับ) |
| เหมาะกับ | Side-quest กลางเทิร์น | Specialist ทำงานคู่ขนาน | ถกเถียง / ร่วมมือ | Bulk fan-out |

## Troubleshooting

**"workflow: state.jsonl unavailable — proceeding without checkpoint"**
— `.thclaws/workflows/` สร้างหรือเขียนไม่ได้ ตรวจ permission ของ
`.thclaws/` ใน project root

**Script error: `ReferenceError: thclaws is not defined`** — คุณ
น่าจะรันสคริปต์นอก `/workflow run` global `thclaws.*` มีอยู่เฉพาะ
ใน workflow sandbox

**Workflow ค้างหลังบรรทัด `⠋ wN  …`** — worker ตัวนั้นกำลังใช้เวลา
นาน Tier 1 ยังไม่มี timeout ต่อ subagent call กด Ctrl-C จะหยุดทั้ง run

**Re-author loop ได้สคริปต์เดิมซ้ำ ๆ** — model อาจมอง revision
note ของคุณข้าม ลองยกเลิกแล้วรันใหม่โดยเขียน goal ให้ชัดขึ้น แทน
การพึ่ง `r`-loop
