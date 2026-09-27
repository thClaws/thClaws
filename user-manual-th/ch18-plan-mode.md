# บทที่ 18 — Plan mode

Plan mode คือ workflow แบบ **สองเฟส** สำหรับงานที่อยากให้ model **ออกแบบแผนก่อน** แล้วค่อยดู model ทำตามแผนทีละ step ผ่าน sidebar ทางขวาที่มี checkmark อัพเดทแบบ live:

![แถบ plan หลังโมเดลเรียก `SubmitPlan` — ขั้นตอนทั้งหมด ปุ่ม Approve & execute และตัวนับขั้นตอนที่ด้านล่าง](../user-manual-img/ch-18/plan-sidebar.png)


1. **Plan phase** — model ใช้ได้แค่ tool ที่ read-only (Read, Grep, Glob, Ls) สำรวจ codebase, ออกแบบ approach, แล้วส่งแผนแบบ structured ผ่าน `SubmitPlan` **Sidebar ทางขวา**จะเด้งขึ้นพร้อมรายการ step และปุ่ม **Approve / Cancel** *ยังไม่มีไฟล์ไหนถูกแก้*
2. **Execution phase** — เมื่อกด Approve, tool ที่แก้ไขได้จะปลดล็อก model เดินตามแผนทีละ step การเปลี่ยนสถานะแต่ละ step (in_progress → done) จะอัพเดทเครื่องหมายถูกใน sidebar แบบ live

รวมกับ **sequential gating** (model จะข้ามไป step 3 ไม่ได้ถ้า step 2 ยังไม่ done) และ **stalled-turn detector** (sidebar แจ้งเตือน "Model seems stuck" ถ้าไม่มี progress 3 turn) plan mode เหมาะมากกับงานไม่เล็กที่อยากเห็นว่า model กำลังทำอะไรโดยไม่ต้องดู chat ทุกบรรทัด

## Quick start

```
คุณ:    /plan
        ↓ (mode สลับเป็น PLAN — pill ใน sidebar เปลี่ยนเป็นสีฟ้า)
คุณ:    submit a plan to add a /healthz endpoint to my web server
Model:  อ่าน routes ที่มีอยู่ ร่างแผน 4 step
        ↓ sidebar เด้งขึ้นพร้อม step + Approve / Cancel
คุณ:    [กด Approve]
Model:  step 1 (in_progress) → เขียน route handler ใหม่ → step 1 (done)
        step 2 (in_progress) → เพิ่ม test → step 2 (done)
        ...
        ↓ ทุก step ✓ → footer แสดง "All 4 steps complete"
```

## วิธีเข้า plan mode

มีสามทาง:

| ทาง | ผล |
|---|---|
| `/plan` (หรือ `/plan enter`) | Mode → **Plan** เก็บ mode เดิม (`auto` / `ask`) ไว้คืนกลับเมื่อแผนจบ |
| Model เรียก `EnterPlanMode` เอง | เหมือน `/plan` สลับ mode ทันทีโดยไม่ถาม — model ตัดสินใจเอง "งานนี้ไม่เล็กควรวางแผนก่อน" |
| `/plan exit` (หรือ `/plan cancel`) | คืน mode เดิม ล้างแผน |
| `/plan status` | แสดง mode ปัจจุบันและสรุปแผน |

แต่ละ subcommand มี alias ให้ ไม่ต้องจำคำเป๊ะ ๆ — `enter` ใช้ `on` หรือ `start` ก็ได้ `exit` ใช้ `off`, `cancel`, `stop`, `abort` ก็ได้ ส่วน `status` ใช้ `show` ได้ ถ้าพิมพ์อย่างอื่นมันจะ print บรรทัด usage ให้

ถ้าไม่มีโหมดที่ stash ไว้ — เช่นคุณถูกโยนเข้า plan mode มาตรง ๆ — การ exit จะพาไปลงที่ `ask` ซึ่งเป็นค่าปลอดภัย ไม่ใช่โหมดที่ session บังเอิญเริ่มมา

ขณะอยู่ใน plan mode pill ใน sidebar จะเป็น **PLAN** สีฟ้า ส่วน mode อื่น (`AUTO`, `ASK`) จะเป็น pill outline หม่น

## Tool อะไรที่ถูก block ใน plan mode

ไม่มี blocklist ที่ต้องมาไล่เขียนเองทีละตัว กฎมีบรรทัดเดียว

> **ใน plan mode tool ทุกตัวที่ปกติต้องขออนุญาตคุณ จะถูกปฏิเสธแทน**

tool ที่ `requires_approval` เป็น true จะ **ถูก block แข็ง ๆ ที่ dispatch gate** และ model จะได้ tool result แบบ structured ว่า "Blocked: {tool} is not available in plan mode. Use Read / Grep / Glob / Ls to explore the codebase. When you have enough context, call SubmitPlan…" model อ่านแล้วเปลี่ยนไปสำรวจแบบ read-only

การผูกสองเรื่องนี้เข้าด้วยกันทำให้ list ไม่มีวัน drift — tool ตัวใหม่ที่ขออนุญาต จะใช้ไม่ได้ระหว่างวางแผนโดยอัตโนมัติ ไม่ต้องมีใครมานั่งจำว่าต้องไปเพิ่มชื่อที่ไหน

ผลที่ออกมาคือ

**ที่ถูก block** — `Write`, `Edit`, `Bash` **ทั้งหมด**, document tool ตระกูล `Docx*` / `Xlsx*` / `Pptx*`, `WebFetch`, `WebSearch`, `TodoWrite` และ MCP tool ตัวไหนก็ตามที่ขออนุญาต

**ที่ใช้ได้** — `Read`, `Grep`, `Glob`, `Ls`, plan tool ทั้งสี่ (`SubmitPlan` / `UpdatePlanStep` / `EnterPlanMode` / `ExitPlanMode`) และ `AskUserQuestion` เพื่อให้ model ยังถาม clarify scope ระหว่างวางแผนได้

มีสองตัวที่ควรพูดให้ชัด

**`Bash` ถูก block ทั้งดุ้น ไม่ได้เลือกเฉพาะคำสั่งอันตราย** เพราะ approval gate ของมันเป็น true แบบไม่มีเงื่อนไข ระหว่าง plan phase model จึงรัน shell command *ไม่ได้เลย* ไม่ว่าจะ `ls`, `git log` หรือ `cargo check` ถ้าแผนของคุณต้องอาศัยผลลัพธ์ของคำสั่งไหน ให้บอก model ก่อนเข้า plan mode หรือ approve แผนแล้วปล่อยให้มันไปรู้เอาใน step 1 จุดนี้ทำให้หลายคนงงเพราะคาดว่า shell แบบอ่านอย่างเดียวน่าจะผ่านได้

**`TodoWrite` ไม่ได้เป็นกรณีพิเศษ** มันขออนุญาต จึงถูกปฏิเสธด้วยกฎเดียวกับตัวอื่น ผลลัพธ์ตรงกับที่เราต้องการ — `SubmitPlan` คือวิธีที่ถูกต้องกว่าเมื่อ user เห็นแผนแบบ live — แต่มันไม่ใช่ข้อยกเว้นที่เขียนไว้ด้วยมือ

### Subagent ระหว่าง plan phase

`Task` กับ `Skill` ไม่ต้องขออนุญาต model จึง *spawn subagent ได้* ระหว่างวางแผน ซึ่งปลอดภัย เพราะ subagent สืบทอด permission mode ของ parent จึงเริ่มต้นใน Plan mode เหมือนกันและชน gate เดียวกัน มันอ่านและค้นแทน parent ได้ แต่เขียนอะไรที่ parent เขียนไม่ได้ ไม่ได้เหมือนกัน

## Plan sidebar

เมื่อ `SubmitPlan` ถูกเรียก sidebar ทางขวาจะเปิดอัตโนมัติ แต่ละ step คือหนึ่งแถว:

| สัญลักษณ์ | ความหมาย |
|---|---|
| ☐ | Todo — ยังไม่เริ่ม จาง ๆ ถ้า step ก่อนหน้ายังไม่ done |
| ◉ | In progress (กระพริบเบา ๆ) |
| ✓ | Done หัวข้อมีขีดฆ่า |
| ✕ | Failed มีหมายเหตุข้อผิดพลาดสีแดงอยู่ใต้หัวข้อ และมีปุ่ม Retry / Skip / Abort |

### Header

ส่วนหัวของ sidebar แสดง:
- **PLAN** pill (สีฟ้าตอนอยู่ใน plan mode, แบบ outline หม่นเมื่อเป็น `AUTO` / `ASK` หลัง Approve)
- **↻ REPLANNED** chip (สีเหลือง 5 วินาที) — โผล่ขึ้นเมื่อ `SubmitPlan` แทนที่แผนเก่า เพื่อให้สังเกตว่า model จัดเรียง step ใหม่
- ปุ่ม **×** ปิด — พับ sidebar ลงเหลือ tab เล็ก ๆ ทางขอบขวา คลิกเพื่อเปิดใหม่ แผนยังอยู่ background

### Approve / Cancel (เห็นเฉพาะตอน plan phase)

เมื่อ model ส่งแผน sidebar จะแสดงแถวปุ่มเหนือ step:

- **Approve & execute** (ปุ่มสีหลัก) — สลับ permission mode เป็น `Auto` และ auto-nudge ให้ agent turn ใหม่เริ่มทันที ไม่มี approval popup ต่อ tool ตอน execute
- **Cancel** (ปุ่ม subtle) — ทิ้งแผน คืน permission mode เดิม

เมื่อ step ใดเริ่ม execute แล้ว (`in_progress` หรือ `done`) ปุ่มจะหายไป — เลย window ของ approval แล้ว

### Failure recovery

ถ้า model marking step เป็น `failed` แถว step นั้นจะมีแถวปุ่มอยู่ข้างใต้:

- **Retry** — re-enter step (Failed → InProgress) แล้ว auto-nudge ให้ model ลองใหม่
- **Skip** — บังคับให้ step เป็น `done` พร้อม note "skipped by user" model จะไป step ถัดไป
- **Abort** — ทิ้งแผน คืน permission mode เดิม (เหมือน Cancel)

### คำเตือน Stalled-turn

ถ้า model ทำงาน 3 turn ติด ๆ โดยไม่มีความคืบหน้าในแผน (ไม่เรียก `UpdatePlanStep` ขณะ step เป็น `in_progress`) sidebar จะแสดง banner สีเหลือง:

> **Model seems stuck**
> 3 turns without progress on step "Install dependencies"
> [Continue] [Abort]

- **Continue** reset ตัวนับ และ prompt ให้ model commit ไปยัง step transition (advance เป็น done หรือ mark failed)
- **Abort** ล้างแผน คืน mode เดิม

threshold ตั้งใจไว้ — งานที่ใช้ turn เดียวยาว ๆ (Bash command ช้า, refactor หนัก) เกิดใน *หนึ่ง turn* เลยไม่ trigger จะถูกตรวจจับเมื่อ model วน loop จริง ๆ (อ่าน คิด ตอบ อ่านอีก คิด ตอบ ไม่ commit) เกิน 3 turn

**banner ขึ้นครั้งเดียว ไม่ได้ขึ้นทุก turn** มันจะโผล่ใน turn ที่ตัวนับแตะ 3 เป็นครั้งแรก แล้วเงียบไป การเปลี่ยนแปลงแผนใด ๆ จะ re-arm มันใหม่ ไม่ว่าจะเป็น `UpdatePlanStep`, การกด Skip หรือการที่คุณกด Continue เอง มันจึงเตือนคุณได้อีกครั้งหลังผ่านไปอีกสาม turn ที่ไม่คืบหน้า แทนที่จะจู้จี้ทุก turn ระหว่างนั้น

### Footer

แสดงตัวเลขรวม:
- *ระหว่าง execute:* "2 of 7 steps complete" (สีเทาหม่น)
- *เมื่อจบ:* "✓ All 7 steps complete" (สีหลัก ตัวหนา)

## Driver: เกิดอะไรขึ้นหลังคุณกด Approve

Approve ไม่ได้แค่ปลด block tool แล้วภาวนาให้ model ทำต่อเอง ตัว shared
session จะรัน **driver** ที่ดันแผนไปข้างหน้าทีละ step หลังจบทุก turn ของ
agent มันจะดูแผนแล้วตัดสินใจว่าจะทำอะไรต่อ

1. ถ้า step แรกสุดที่ยังไม่เสร็จอยู่ในสถานะ **Failed** มันจะหยุดรอ
   ปุ่ม Retry / Skip / Abort ใน sidebar เป็นของคุณ driver จะไม่ดัน
   ข้าม step ที่คุณยังไม่ได้ตัดสินใจ (เวอร์ชันก่อนหน้าเคยดันข้าม แล้ว
   เผา retry budget ของ step ถัดไปทั้งก้อนไปกับงานที่ยังไม่เคยถูก
   unblock ตั้งแต่แรก)
2. ถ้าไม่ใช่ มันจะหา step แรกที่ยังเป็น `Todo` หรือ `InProgress` แล้ว
   ส่ง continuation prompt ให้ model สำหรับ step นั้นโดยเฉพาะ
3. เมื่อ step สุดท้ายขึ้น `Done` driver จะหยุด และ permission mode จะ
   restore กลับเอง

"Approve ครั้งเดียวแล้วเดินไปทำอย่างอื่น" จึงเป็นการรับประกันจริง ๆ ไม่ใช่
การหวังว่า model จะประพฤติดี เพราะ loop อยู่ใน engine ไม่ได้อยู่ใน prompt

### Retry budget ต่อ step

แต่ละ step มี **3 ครั้ง** ถ้า model ใช้ครบสามครั้งโดยไม่เปลี่ยนสถานะ step
เป็น `Done` หรือ `Failed` driver จะบังคับ mark เป็น `Failed` พร้อม note

> `max retries per step exceeded (3 attempts) — the agent looped without committing to done or failed. Use the sidebar Retry / Skip / Abort buttons to recover.`

ซึ่งเป็นสถานะ Failed แบบเดียวกับกรณีอื่น คุณจึงได้แถวปุ่ม Retry / Skip /
Abort เหมือนกัน และ **Retry จะรีเซ็ตตัวนับ** ให้ step นั้นได้อีกสามครั้งใหม่

budget นี้คิดเป็นราย step ไม่ใช่รวมทั้งแผน step ที่มีปัญหาตัวเดียวจึงกิน
โควตา iteration ของทั้งรอบจนอดสำหรับ step ที่เหลือไม่ได้

### Compaction ที่รอยต่อระหว่าง step

การข้ามจาก step หนึ่งไปอีก step หนึ่งเป็นจังหวะที่เหมาะกับการสลัด history
ทิ้ง driver จึง compact ตรงนั้น — ครั้งเดียวต่อหนึ่งรอยต่อ และทำก็ต่อเมื่อ
มี step ที่เสร็จแล้วอย่างน้อยหนึ่งตัว (ก่อนหน้านั้นยังไม่มีอะไรให้ compact)

ผลลัพธ์ของ plan tool จะถูกเก็บไว้ครบเสมอ เพราะเป็นเบรดครัมบ์ที่ model ใช้รู้
ว่าตัวเองทำอะไรไปแล้วบ้าง มีแต่ tool result ธรรมดาจากก่อนรอยต่อเท่านั้นที่
ถูกแทนด้วย placeholder สั้น ๆ

มีสอง strategy ตั้งได้ใน `.thclaws/settings.json`

```json
{ "planContextStrategy": "compact" }
```

| ค่า | ทำอะไร |
|---|---|
| `"compact"` | **ค่า default** ย่อเชิงโครงสร้าง — tool result ที่ไม่ใช่ plan ของเก่ากลายเป็น placeholder แต่รูปทรงของ history ยังอยู่ |
| `"clear"` | ล้าง history ทิ้งทั้งหมด เหลือไว้แค่ข้อความแรกของ user เพื่อเป็นหลักยึด |

`clear` เป็นตัวเลือกที่ดุ คุ้มเฉพาะกับแผนยาวมาก ๆ (20+ step) ที่ compaction
อย่างเดียวเอาไม่อยู่ เพราะมันบังคับให้ model พึ่ง `output` ที่บันทึกไว้ของแต่ละ
step กับโครงแผนใน system reminder ล้วน ๆ แผนที่ step ท้าย ๆ ต้องใช้
รายละเอียดจากบทสนทนาก่อนหน้าจึงจะทำได้แย่ลงภายใต้ค่านี้ ส่วนค่าอื่นที่ไม่ใช่
สองตัวนี้จะตกกลับไปเป็น `compact`

## Sequential gating

แผนเรียงตามลำดับเสมอ Model **ห้าม** เริ่ม step 3 ขณะที่ step 2 ยัง `Todo` หรือ `InProgress` ถ้าพยายาม gate จะตอบกลับ:

> `cannot start step 2 ("Install dependencies") — step 1 ("Scaffold project") is currently Todo, not Done. Finish or fail the previous step first.`

Model อ่าน error นี้ใน turn ถัดไปและแก้ตัวเอง รวมกับการแสดงผล step ในอนาคตแบบจาง ๆ ใน sidebar (จางจนกว่า step ก่อนหน้าจะ done) gate ถูกบังคับใช้แบบ structural ไม่ใช่แค่หวังให้ model ทำตาม

Transition ที่ legal มีแค่:

| จาก | ไป | หมายเหตุ |
|---|---|---|
| Todo → InProgress | เฉพาะเมื่อ step ก่อนหน้าเป็น Done (หรือเป็น step 1) |
| InProgress → Done | path ปกติ |
| InProgress → Failed | ควรมี note สั้น ๆ |
| Failed → InProgress | path สำหรับ retry |
| Done → InProgress | **ไม่อนุญาต** — submit แผนใหม่แทน |

ปุ่ม Skip บน failed step bypass กฎเหล่านี้โดยจงใจ — เป็น override ของ user ที่บันทึกเป็น note "skipped by user" สำหรับ audit

## Permission mode dance

| Event | Mode หลังจากนั้น |
|---|---|
| `/plan` หรือ `EnterPlanMode` | **Plan** (เก็บ mode เดิมไว้) |
| `SubmitPlan` | **Plan** (ไม่เปลี่ยน) |
| กด **Approve** | **Auto** (mode เดิมยัง stash อยู่ จะคืนตอน plan complete) |
| กด **Cancel** | คืน mode เดิม ล้างแผน |
| Model เรียก `ExitPlanMode` | คืน mode เดิม แผนยังอยู่ |
| Step สุดท้าย → `Done` | คืน mode เดิมอัตโนมัติ |

flow ปกติ `Ask → /plan → submit → Approve → execute → all done → Ask` วนกลับมาที่จุดเริ่มต้นพอดี ไม่มี `Auto` ตกค้างไปกระทบงานอื่นที่ไม่เกี่ยว

## CLI parity

ใน CLI mode (`thclaws --cli`) plan mode ทำงานเหมือนกันที่ระดับ data model — `/plan`, `/plan status`, ฯลฯ ใช้ได้ปกติ ไม่มี sidebar ทางขวา แต่ทุกครั้งที่ plan tool ถูกเรียก (SubmitPlan / UpdatePlanStep / EnterPlanMode / ExitPlanMode) จะ print ANSI block สีสันแสดงสถานะแผนแบบ inline:

```
─── plan: 4 steps · 2 done · current step 3 ───
  ✓ 1. Scaffold project
  ✓ 2. Install dependencies
  ◉ 3. Run tests
    4. Deploy
─────────────────────────────────────────────
```

สัญลักษณ์เหมือนใน GUI (`✓` done, `◉` in progress, `✕` failed) note ของ failure แสดงเป็นตัวเอนหม่น ๆ ใต้ step

**CLI ไม่มี driver** นี่คือความต่างที่แท้จริง และใหญ่กว่าเรื่องไม่มี sidebar เสียอีก ทั้ง loop เดินทีละ step, retry budget ต่อ step, compaction ที่รอยต่อ step และ stalled-turn banner ล้วนอยู่ใน shared-session worker ที่ GUI กับ `--serve` รัน — CLI REPL ไม่มีสักอย่าง ตัว plan state, sequential gate และ plan tool ทำงานเหมือนกันเป๊ะ แต่ *คุณ* เป็นคนดัน step ด้วยการพิมพ์ prompt แทนที่จะกด Approve แล้วเดินจากไป ใช้ CLI ทำ plan mode ตอนที่อยากได้โครงสร้างกับ checklist ที่มองเห็น ใช้ GUI ตอนที่อยากให้แผนเดินเอง

## Persistence ข้าม `/load`

Plan state ถูก mirror ลง session JSONL ผ่าน event `plan_snapshot` ทุกครั้งที่มีการเปลี่ยนแปลง โหลด session กลับมาจะคืน:
- แผนพร้อมสถานะของทุก step
- Sidebar เปิดที่ step เดิมที่ session ก่อนหน้าทิ้งไว้

ดังนั้นจะ interrupt บันทึก กลับมาวันถัดไปก็ทำต่อจากที่ค้างไว้ได้ system reminder ของ model จะถูกสร้างใหม่จาก state ที่ restore ได้พอดี model จึงเห็น context "อยู่ที่ step N" ที่ถูกต้อง

`/new` ล้างแผน เช่นเดียวกับ `/fork` และ `/cwd <new>` ตามด้วยการเปลี่ยน model ปุ่ม Cancel และ Abort ก็ล้างแผนเช่นกัน

## ทำงานกับ `TodoWrite` นอก plan mode

`TodoWrite` คือ **scratchpad casual** สำหรับ task tracking ส่วนตัวของ model มันเขียน `.thclaws/state/todos.md` เป็น checklist แบบ markdown User เห็นเฉพาะเมื่อเปิดไฟล์เอง ไม่มี UI live

ใช้ `TodoWrite` เมื่อ:
- model อยากจดสิ่งที่กำลังทำให้ตัวเอง
- user ไม่ได้ขอแผนเป็นทางการ
- งานสั้น ๆ หลวม ๆ

ใช้ plan mode + `SubmitPlan` เมื่อ:
- user ต้องการเห็น ("show me your plan, then do it step by step")
- งานมีลำดับ phase ชัดเจน
- ต้องการ structural enforcement ของ sequential gating
- ต้องการให้ user แทรกเข้ามาได้ระหว่าง execute (Cancel, Retry, Skip)

ลองเรียก `TodoWrite` ระหว่าง plan mode จะได้ error "use SubmitPlan instead" — ทั้งสอง tool exclusive ต่อกันโดยตั้งใจ

## เปรียบเทียบ thclaws กับ Claude Code plan mode

| | Claude Code | thclaws |
|---|---|---|
| Plan mode เป็น permission mode | ✓ | ✓ |
| Block tool ที่แก้ไขช่วง plan phase | ✓ (ผ่าน `behavior: 'ask'` fallback) | ✓ (ผ่าน structured `Blocked:` tool_result, ไม่มี popup) |
| แสดง progress live ตอน execute | ❌ (modal ครั้งเดียวแล้วหาย) | ✓ (sidebar ขวาคงอยู่ พร้อม checkmark) |
| Sequential step gating | ❌ (เชื่อ model ว่าทำตามลำดับ) | ✓ (Layer-1 gate reject การข้าม) |
| UI สำหรับ failed step recovery | ❌ (ใน chat อย่างเดียว) | ✓ (ปุ่ม Retry / Skip / Abort) |
| Stalled-turn detector | ❌ | ✓ (threshold 3 turn + banner Continue / Abort) |
| Persist แผนเมื่อ `/load` | ✓ (`plans/<id>.md`) | ✓ (event `plan_snapshot` ใน session JSONL) |

thclaws optimize เพื่อ **visibility live + structural enforcement** flow modal Approve ของ Claude Code เหมาะกับแผน one-shot สั้น ๆ ส่วน sidebar ของ thclaws ดีกว่าสำหรับแผนที่อยากดูทุกขั้นตอนคลี่คลาย

## Workflow ที่พบบ่อย

**"Show me your plan first"** พิมพ์ "/plan" หรือ "submit a plan to do X" — model รู้ convention และจะใช้ SubmitPlan ดู sidebar กด Approve

**"เปลี่ยนใจกับ approach นั้น"** กด Cancel ที่ sidebar — แผนถูกทิ้ง mode คืนค่าเดิม พิมพ์ prompt ใหม่ตามทิศทางใหม่

**"step นั้น fail เพราะปัญหา network ชั่วคราว"** กด Retry ที่ failed step Model จะ re-enter InProgress และลองใหม่

**"step นั้นทำไม่ได้เพราะไม่มี credential ตอนนี้ — ข้าม"** กด Skip step จะถูกบันทึก note `"skipped by user"` (audit trail) แผนเดินไป step ถัดไป

**"Model วนซ้ำกับ step นี้"** รอ banner stalled-turn (3 turn ไม่มี progress) กด Continue เพื่อกระทุ้ง model หรือ Abort ถ้าติดจริง ๆ

**"กด Approve ครั้งเดียวเดินไปทำอย่างอื่น กลับมาดูแผนเสร็จ"** นั่นคือดีไซน์ หลัง Approve mode เป็น `Auto` ไม่มี popup ต่อ tool sidebar ให้ภาพรวม "นี่คือสิ่งที่ทำเสร็จไป" แบบเหลือบดูได้ตอนกลับมา เมื่อทุกอย่าง done mode คืนค่าเดิมอัตโนมัติ ไม่บังเอิญติด `Auto` ในงานต่อไป
