# บทที่ 19 — การตั้งเวลา (Scheduling)

ฟีเจอร์ Scheduling ช่วยให้คุณรัน prompt ของ thClaws ตามตารางเวลาแบบ cron — ทุกเช้าวันจันทร์-ศุกร์ ทุกคืนวันอาทิตย์ หรือทุก 5 นาที — หรือยิงครั้งเดียวในเวลาที่คุณระบุ — โดยไม่ต้องจำว่าจะต้องพิมพ์ prompt เอง แต่ละงานที่ตั้งเวลาไว้จะ spawn เป็น subprocess `thclaws --print` ของตัวเองในไดเรกทอรีที่กำหนด ดังนั้น 2 schedule ใน 2 โปรเจกต์จึงเป็นอิสระจากกันโดยสมบูรณ์

![`/schedule` — หนึ่งแถวต่อหนึ่งงาน: สถานะ ผลรันล่าสุด ชื่อ cron expression และเวลารันครั้งถัดไป](../user-manual-img/ch-19/schedule-list.png)


ฟีเจอร์นี้แบ่งเป็น 3 ชั้น แต่ละชั้นใช้งานได้เดี่ยว ๆ:

| ชั้น | ทำอะไร | จะยิงเมื่อไหร่ |
|---|---|---|
| **Store + manual run** | Schedule ที่มีชื่อจะถูกบันทึกไว้ในไฟล์ JSON ระดับผู้ใช้ และ `thclaws schedule run <id>` จะยิงงานหนึ่งครั้งแบบ synchronous | เฉพาะตอนที่คุณ (หรือ `cron` / `launchd`) เรียก `run` เท่านั้น |
| **In-process scheduler** | Background tokio task จะ tick ทุก 30 วินาทีขณะที่ surface ของ thclaws เปิดอยู่ และยิงงานที่ถึงเวลาแบบอัตโนมัติ | ขณะที่ `thclaws --gui`, `--cli`, หรือ `--serve` กำลังรันอยู่ |
| **Native daemon** | Process ที่รันยาวภายใต้ supervisor (launchd บน macOS, systemd-user บน Linux) host scheduler โดยไม่ต้องมีคนดูแล | ตลอดเวลา — แม้ไม่มี GUI/CLI session ใดเปิดอยู่ |

เลือกชั้นที่ตรงกับว่าคุณต้องการให้งานยิงโดยไม่มีคนเฝ้ามากแค่ไหน: in-process tick พอเพียงถ้าคุณมัก ๆ เปิด `thclaws --gui` ทิ้งไว้ระหว่างวัน, daemon เหมาะกับสถานการณ์ "ยิงขณะที่ปิดฝา laptop"

## เริ่มต้นใช้งาน

```
$ thclaws schedule add morning-brief \
    --cron "30 8 * * MON-FRI" \
    --cwd ~/projects/web \
    --prompt "สรุป commit วันนี้และ PR ที่เปิดอยู่ลงไฟล์ ~/Desktop/brief.md" \
    --timeout 600
added schedule 'morning-brief'

$ thclaws schedule list
on        morning-brief             30 8 * * MON-FRI      never  /Users/jimmy/projects/web

$ thclaws schedule run morning-brief
[schedule] 'morning-brief' ran in 38.412s, log: /Users/jimmy/.local/share/thclaws/logs/morning-brief/2026-05-06T08-30-04Z.log
```

นี่คือ workflow ทั้งหมด: เพิ่ม, รันด้วยมือเพื่อทดสอบถ้าต้องการ, จากนั้นปล่อยให้ in-process tick ยิง หรือ install daemon เพื่อให้ยิงโดยไม่ต้องมีคนเฝ้า

## ฟิลด์ของ schedule

แต่ละ schedule เก็บข้อมูลตามฟิลด์ต่อไปนี้ `id` กับ `prompt` ต้องระบุเสมอ บวกกับ **trigger อีกหนึ่งตัวเท่านั้น** คือ `cron` (แบบทำซ้ำ) หรือ `run_at` (ยิงครั้งเดียว)

| ฟิลด์ | ต้องระบุ | ค่าดีฟอลต์ | ทำอะไร |
|---|---|---|---|
| `id` | ✅ | — | คีย์สำหรับค้นหา ใช้เป็นชื่อโฟลเดอร์ log ด้วย |
| `cron` | trigger | — | Cron expression แบบ POSIX 5 ฟิลด์ สำหรับงานที่ทำซ้ำ ตรวจสอบความถูกต้องตอน add ถ้าเป็น one-shot จะเว้นว่าง |
| `run_at` | trigger | — | timestamp แบบ RFC 3339 สำหรับงาน **ยิงครั้งเดียว** — ยิงแล้วปิดตัวเองอัตโนมัติ ตั้งผ่าน `--at` หรือ `--in` ดู [Schedule แบบยิงครั้งเดียว](#one-shot) ใช้ร่วมกับ `cron` ไม่ได้ |
| `prompt` | ✅ | — | ข้อความที่จะส่งให้ `thclaws --print` หลายบรรทัดได้ |
| `cwd` | — | ไดเรกทอรีปัจจุบัน | Working directory ที่งานจะรัน เป็นตัวกำหนดว่าจะใช้ `.thclaws/settings.json`, sandbox, memory, และ MCP config ระดับโปรเจกต์ของไดเรกทอรีไหน |
| `model` | — | ตามที่ `cwd` กำหนด | Override ชื่อโมเดล (`claude-opus-5`, `gpt-5` ฯลฯ) |
| `maxIterations` | — | ตามที่ `cwd` กำหนด | จำกัดรอบการเรียก tool ของ agent loop |
| `resumeSession` | — | ไม่ตั้ง (stateless) | **โหมด Heartbeat** (v0.88.0+): ใส่ `--resume-session last` แล้วทุกการยิงจะต่อ session เดียวที่โตขึ้นเรื่อยๆ แทนการเริ่มใหม่ — ดู [Heartbeat](#heartbeats) |
| `timeoutSecs` | — | 600 (10 นาที) | Timeout แบบ hard ถ้าเกินจะ kill งานและบันทึกเป็น `timed_out` ใส่ `--timeout 0` ตอน add ถ้าไม่ต้องการ timeout |
| `enabled` | — | `true` | ถ้าเป็น `false` scheduler จะข้าม และ `schedule run` จะปฏิเสธไม่ยิง ใส่ `--disabled` ตอน add เพื่อเริ่มแบบปิดไว้ก่อน ส่วน one-shot ที่ยิงแล้วจะตั้งค่าตัวเองเป็น `false` |
| `watchWorkspace` | — | `false` | ถ้า `true` daemon จะยิงงานเมื่อมีไฟล์ใน `cwd` เปลี่ยนแปลง (debounce ~2 วินาที) — ดู [Trigger เมื่อ workspace เปลี่ยนแปลง](#trigger-เมื่อ-workspace-เปลี่ยนแปลง) ด้านล่าง รองรับเฉพาะ daemon เท่านั้น in-process scheduler จะข้าม flag นี้ |
| `lastRun` / `lastExit` | — | ไม่มี | ตั้งค่าอัตโนมัติหลังการยิงครั้งแรก |

## Cron expression

POSIX แบบ 5 ฟิลด์มาตรฐาน: `minute hour day-of-month month day-of-week`

| Expression | ความหมาย |
|---|---|
| `*/5 * * * *` | ทุก 5 นาที |
| `0 * * * *` | นาที 0 ของทุกชั่วโมง |
| `30 8 * * MON-FRI` | 08:30 ของวันจันทร์-ศุกร์ |
| `0 21 * * SUN` | 21:00 ของทุกวันอาทิตย์ |
| `0 0 1 * *` | เที่ยงคืนของวันที่ 1 ทุกเดือน |
| `0 9,13,17 * * *` | 09:00, 13:00, 17:00 ทุกวัน |

รองรับ syntax แบบ range/list (`MON-FRI`, `1,15`) Cron expression จะถูกตรวจสอบตอนรัน `schedule add` — พิมพ์ผิดจะแสดง error ที่อ่านง่ายแทนที่จะล้มเหลวเงียบ ๆ ตอนงานยิง

## Schedule แบบยิงครั้งเดียว — ยิงแล้วจบ {#one-shot}

cron ไม่ใช่รูปทรงที่เหมาะกับ "ทำอันนี้ทีเดียว ตอนหลัง" เพราะ `0 30 15 24 5 *` จะกลับมา match อีกทีเดือนพฤษภาปีหน้า และถ้าเครื่องหลับอยู่ตอน 15:30 ช่องเวลานั้นก็หายไปเฉย ๆ schedule จึงถือเวลายิงแบบสัมบูรณ์แทน cron expression ได้

```sh
# ระบุเวลาตรง ๆ — RFC 3339 ใส่ offset ได้
thclaws schedule add ship-reminder \
  --at "2026-05-24T15:30:00Z" \
  --prompt "check whether the release notes for v0.99 are still a draft"

# ระบุแบบสัมพัทธ์ — 15m, 2h, 90s, 1d (ตัวเลขเปล่า ๆ นับเป็นวินาที)
thclaws schedule add recheck-ci \
  --in 45m \
  --cwd ~/projects/web \
  --prompt "check whether the CI run on main went green; if not, summarise the failure"
```

`--at` กับ `--in` ใช้ร่วมกันเองไม่ได้ และใช้ร่วมกับ `--cron` ก็ไม่ได้ ส่วน `--in` เป็นแค่น้ำตาลที่ถูกแปลงเป็น `run_at = now + duration` ตั้งแต่ตอน add ระบบรับ offset และแปลงเป็น UTC ให้ `2026-05-24T22:30:00+07:00` กับ `2026-05-24T15:30:00Z` จึงหมายถึงขณะเดียวกัน

หลังยิงเสร็จ entry จะตั้ง `enabled` ของตัวเองเป็น `false` แต่ยังอยู่ใน store คุณจึงอ่าน log ดู exit code และเปิดกลับมาใหม่ด้วยมือได้ถ้าอยากให้ยิงซ้ำ มันไม่ได้ถูกลบทิ้ง

**One-shot ตั้งใจให้ catch-up** นี่เป็นที่เดียวที่กฎ skip-catch-up ถูกกลับด้านโดยเจตนา ถ้า daemon ดับอยู่ตอนถึงเวลายิง `run_at` ที่เลยมาแล้วยังนับว่าถึงกำหนดและจะยิงใน tick ถัดไป แทนที่จะหายไปเงียบ ๆ เพราะ one-shot ที่พลาดไปคือ *งาน* ที่พลาด ส่วนนาที cron ที่พลาดไปก็แค่นาทีหนึ่งที่พลาด

## Heartbeat — schedule ที่จำได้ {#heartbeats}

โดยปกติทุกการยิงเป็นแบบ **stateless**: scheduler spawn `thclaws --print`
ใหม่ agent รู้เฉพาะสิ่งที่อ่านได้จาก disk (KMS, ไฟล์, memory) — บทสนทนา
ของการยิงครั้งก่อนหายไปหมด ซึ่งเหมาะกับงาน maintenance แต่ผิดรูปสำหรับ
loop ต่อเนื่องที่อยากให้ agent ฉลาดขึ้นทุกรอบ เช่น กลยุทธ์เทรดที่ปรับตัว,
monitor ที่จำได้ว่าเคยแจ้งอะไรไปแล้ว, งานวิจัยข้ามคืนที่ต่อยอดเหตุผลเดิม

**โหมด Heartbeat** (v0.88.0+) ต่อทุกการยิงเข้า session เดียว:

```bash
thclaws schedule add trade-loop \
  --cron "*/15 * * * *" \
  --prompt "ตรวจพอร์ต แล้วปรับกลยุทธ์จากผลของการตัดสินใจครั้งก่อนๆ" \
  --resume-session last
```

`--resume-session last` ทำให้แต่ละการยิงรัน
`thclaws --print --resume last` — ยิงแรกเริ่ม session, ยิงถัดๆ ไปโหลด
history ทั้งหมด เพิ่มหนึ่ง exchange แล้วบันทึก agent จำการตัดสินใจ
ของตัวเองและผลลัพธ์ได้จริง (จะใส่ session id ตรงๆ เพื่อต่อบทสนทนา
ที่มีอยู่ก็ได้)

ข้อควรรู้:

- **แยก working directory ให้ heartbeat โดยเฉพาะ** (`--cwd`) — `last`
  หมายถึง session ล่าสุดของ *cwd นั้น* ถ้าคุณแชทเองใน workspace
  เดียวกัน แชทของคุณจะกลายเป็น "last" แล้ว heartbeat จะไปต่อผิดเส้น
- **ค่า token โตตาม history** — ทุกการยิงอ่านบทสนทนาทั้งหมดซ้ำ
  (compaction ช่วยอัตโนมัติเมื่อยาว) loop 24/7 จึงแพงกว่าแบบ stateless
  พอสมควร นี่คือราคาของความจำ — งานที่ KMS บน disk เพียงพอ
  ใช้แบบ stateless เถอะ
- **เปิดดูบทสนทนาได้ตลอด** — เป็น session ปกติ เปิดจาก session list
  ใน GUI หรือ `thclaws --cli` → `/resume <id>` ใน cwd เดียวกัน
- ใช้ได้กับ trigger ทั้งสามชั้น (manual `schedule run`, in-process, daemon)

## Trigger เมื่อ workspace เปลี่ยนแปลง

นอกเหนือจาก (หรือแทนที่) cron แล้ว schedule สามารถถูกยิงเมื่อมีไฟล์ใน working directory เปลี่ยนแปลงได้ ตั้ง `watchWorkspace: true` ใน JSON, ใส่ `--watch` ในคำสั่ง `thclaws schedule add`, หรือติ๊กช่องที่เขียนว่า **"Run when file in workspace changes"** ใน modal ของ GUI

```sh
thclaws schedule add doc-summary \
  --cron "0 9 * * *" \
  --cwd ~/projects/blog \
  --prompt "สรุป diff ของวันนี้ลงไฟล์ ~/Desktop/blog-changes.md" \
  --watch
```

Schedule นี้จะยิงทุกวันเวลา 09:00 **และ** ทุกครั้งที่มีไฟล์ใน `~/projects/blog` เปลี่ยน — ทั้ง 2 trigger ใช้เส้นทาง `run_once` เดียวกัน

**Debounce + cooldown** การ save ไฟล์ในโปรแกรมแก้ไขทั่วไป (Vim/VS Code) มักจะปล่อย event ของระบบไฟล์ 3-5 ครั้งภายใน 100 ms (เนื่องจากการใช้ atomic-rename + swap-file) ตัว watcher จะรวบ event เหล่านี้ภายในกรอบ debounce 2 วินาทีและส่งออกเป็น 1 event หลังจาก fire เริ่ม จะมี cooldown 60 วินาทีที่ดูดกลืน event ที่เข้ามา — นานพอที่ `--print` ที่ถูก spawn ออกไปจะเขียนไฟล์ลงใน workspace โดยไม่ trigger ตัวเองซ้ำทันที

**รายการที่ ignore แบบตายตัว** Watcher จะมองข้าม path ใดก็ตามที่มี segment ต่อไปนี้อยู่ระหว่าง `cwd` กับไฟล์ปลายทาง:

| Segment | เหตุผลที่ ignore |
|---|---|
| `.thclaws/` | ที่ที่ `thclaws --print` เขียน session JSONL ของงานที่ถูก spawn — ถ้าไม่ ignore จะ loop ไม่จบ |
| `.git/` | มีการเปลี่ยนแปลงตลอดเวลาขณะใช้ git ปกติ (`git status`, `git fetch`, `git checkout`) |
| `node_modules/`, `target/`, `dist/`, `build/`, `.next/`, `.cache/` | Build output ที่ไม่ได้ขอให้ agent ตอบสนอง |
| `.DS_Store` | Metadata ของ Finder บน macOS ที่ไม่มีประโยชน์ |

รายการนี้ hardcode ไว้ใน v1 — ยังไม่อ่าน `.gitignore` ถ้าต้องการ control แบบละเอียดกว่านี้ ให้ใช้ `watchWorkspace: false` แล้วใช้ cron อย่างเดียว

**Daemon-only** Trigger แบบ workspace-change ถูก wire-up โดย daemon (Step 3) เท่านั้น — `thclaws schedule install` คือคำสั่งที่เปิดใช้งาน in-process scheduler (Step 2) จะข้าม `watchWorkspace` เพราะจุดประสงค์ทั้งหมดของการยิงด้วย filesystem โดยอัตโนมัติคือไม่มีคนคอยดูแล

**อะไรนับเป็น "การเปลี่ยนแปลง"** Recursive ทั่วทั้ง `cwd` — file create, write, rename, delete (ตาม semantic ของแต่ละ OS ที่ `notify` exposes ออกมา) การเปลี่ยนแปลงในโฟลเดอร์ย่อยก็นับด้วย ยกเว้น path จะผ่าน segment ที่ ignore ไว้

## Layout ของ storage

| Path | เนื้อหา |
|---|---|
| `~/.config/thclaws/schedules.json` | Schedule store แก้ด้วยมือได้ |
| `~/.local/share/thclaws/logs/<id>/<ts>.log` | ไฟล์ log 1 ไฟล์ต่อการยิง 1 ครั้ง — รวม stdout + stderr ของ `thclaws --print` ที่ spawn ออกมา |
| `~/.local/state/thclaws/scheduler.pid` | PID file ของ daemon ใช้โดย `schedule status` และตัวกัน double-daemon |
| `~/Library/LaunchAgents/sh.thclaws.scheduler.plist` (macOS) | ไฟล์ supervisor ของ launchd เขียนโดย `schedule install` |
| `~/.config/systemd/user/thclaws-scheduler.service` (Linux) | Unit ของ systemd-user เขียนโดย `schedule install` |
| `~/.local/share/thclaws/daemon.log` | Log ของตัว daemon เอง (แยกจาก log ของแต่ละงาน) |

ไฟล์ store เป็น JSON เล็ก ๆ ที่อ่านง่าย — แก้ด้วยมือได้สบาย:

```json
{
  "version": 1,
  "schedules": [
    {
      "id": "morning-brief",
      "cron": "30 8 * * MON-FRI",
      "cwd": "/Users/jimmy/projects/web",
      "prompt": "สรุป commit วันนี้และ PR ที่เปิดอยู่",
      "timeoutSecs": 600,
      "enabled": true,
      "lastRun": "2026-05-06T08:30:04Z",
      "lastExit": 0
    }
  ]
}
```

การแก้ไขจะมีผลภายใน 30 วินาที (in-process scheduler อ่าน store ใหม่ทุก tick) — ไม่ต้องสั่ง reload daemon

## ชั้นที่ 1 — รันด้วยมือเท่านั้น

หลัง `schedule add` คุณหยุดที่ตรงนี้ก็ได้ เชื่อม `thclaws schedule run <id>` เข้ากับ scheduler ที่คุณใช้อยู่แล้ว (`crontab`, `launchd`, GitHub Actions, …) แล้วให้ตัวนั้นจัดการเรื่องเวลาเอง

```sh
# crontab -e
30 8 * * 1-5 /usr/local/bin/thclaws schedule run morning-brief
```

แนวทางนี้เหมาะถ้าคุณมี crontab ที่ชอบใช้อยู่แล้ว หรืออยากให้ตรรกะเรื่องเวลาอยู่นอก thclaws ทั้งหมด

## ชั้นที่ 2 — In-process scheduler

ถ้าคุณเปิด `thclaws --gui` หรือ `thclaws --cli` ทิ้งไว้ระหว่างวันเป็นปกติ in-process scheduler จะทำงานอัตโนมัติ เมื่อ surface ของ thclaws (ยกเว้น `--print`) เริ่มทำงาน คุณจะเห็นบรรทัดประกาศบน stderr:

```
[schedule] in-process scheduler running (tick 30s)
```

จากนั้นมันจะ tick ทุก 30 วินาที เมื่อ schedule ถึงเวลา มันจะ spawn subprocess และพิมพ์:

```
[schedule] 'morning-brief' fired — exit=0 duration=38.412s log=/Users/jimmy/.local/share/thclaws/logs/morning-brief/2026-05-06T08-30-04Z.log
```

หากต้องการปิด in-process scheduler (เช่น คุณใช้ daemon อยู่แล้วและไม่อยากให้มีอันที่ 2 ใน CLI session):

```sh
thclaws --cli --no-scheduler
```

โหมด `--print` ไม่ spawn scheduler เลย เพราะ print เป็น short-lived และเสียงรบกวนจาก subprocess ก็ไม่ได้มีประโยชน์อะไร

### Cursor semantics: ข้าม catch-up เป็นค่าดีฟอลต์

เมื่อ scheduler เห็น schedule ครั้งแรก มันจะ seed cursor ที่อยู่ใน memory เป็น `lastRun` ของ schedule (ถ้ามี) หรือเป็น "ตอนนี้" (ถ้าไม่มี) แต่ละ tick จะถามตัว parser ของ cron ว่า fire ครั้งแรกหลัง cursor คือเมื่อไหร่ และยิงถ้าเวลานั้นผ่านมาแล้ว

ผลที่เกิดในทางปฏิบัติ: schedule ที่เพิ่งเพิ่มจะ **ไม่** ยิงย้อนหลังสำหรับ cron event ที่ "พลาด" ก่อนถูกเพิ่ม ถ้าต้องการบังคับให้ยิงย้อนหลัง ให้แก้ `lastRun` ใน `schedules.json` เป็น timestamp ก่อน event ที่ต้องการ replay

ถ้างานก่อนหน้ายังรันค้างอยู่ตอน next fire time มาถึง scheduler จะข้ามครั้งนั้น (ตรงกับ `--no-overlap` ของ `cron`) การยิงพร้อมกันของ schedule เดียวกันจะไม่ถูก queue ไว้

## ชั้นที่ 3 — Native daemon

หากต้องการให้ schedule ยิงแม้ไม่มี GUI หรือ CLI session เปิดอยู่ — รวมถึงข้ามคืน ระหว่างประชุม หรือตอนล็อกหน้าจอ — ให้ install daemon

```sh
$ thclaws schedule install
wrote /Users/jimmy/Library/LaunchAgents/sh.thclaws.scheduler.plist
daemon bootstrapped — `thclaws schedule status` to verify

$ thclaws schedule status
daemon: running (pid 88294)

recent fires:
  ok   morning-brief             2026-05-06T08:30:04Z
  —    deps-audit                never
```

`install` ทำอะไรบ้าง:

- **macOS:** เขียน `~/Library/LaunchAgents/sh.thclaws.scheduler.plist` แล้วรัน `launchctl bootstrap gui/$UID` เพื่อให้ daemon เริ่มทันทีและทุกครั้งที่ login `KeepAlive=true` และ `RunAtLoad=true` ทำให้มัน restart เองอัตโนมัติถ้า crash
- **Linux:** เขียน `~/.config/systemd/user/thclaws-scheduler.service` และพิมพ์คำสั่ง next-step (`systemctl --user daemon-reload && systemctl --user enable --now thclaws-scheduler.service`) ให้ดูก่อนเปิดใช้งาน

หากต้องการหยุดและลบ supervisor entry:

```sh
$ thclaws schedule uninstall
daemon uninstalled
```

Schedule ใน store จะถูกเก็บไว้แม้จะ install/uninstall — คุณแค่เปิด/ปิดกลไก auto-fire เท่านั้น

### สถานะของ daemon

```sh
$ thclaws schedule status
daemon: running (pid 88294)
```

มี 3 สถานะ:

| สถานะ | ความหมาย |
|---|---|
| `running (pid X)` | Daemon มีชีวิตอยู่ |
| `stale PID file (last pid Y not alive)` | Daemon ก่อนหน้าตายโดยไม่ได้ cleanup ตัวต่อไปจะเก็บ PID file คืนอัตโนมัติ |
| `not running` | ไม่มี PID file รัน `schedule install` (สำหรับยิงโดยไม่มีคนเฝ้า) หรือ `thclaws daemon` (foreground สำหรับทดสอบ) |

### โหมด foreground (ทดสอบโดยไม่ต้อง install)

```sh
$ thclaws daemon
[daemon] thclaws scheduler started (pid 12345, pid file ~/.local/state/thclaws/scheduler.pid)
[schedule] in-process scheduler running (tick 30s)
[schedule] 'morning-brief' fired — exit=0 duration=38.412s log=...
^C
[daemon] SIGINT received — shutting down
[daemon] stopped cleanly
```

ใช้โหมดนี้ตรวจว่า schedule ยิงจริงก่อน install เป็น launchd/systemd entry

## Preset สำเร็จรูปสำหรับการดูแล KMS

template schedule สี่ตัวพร้อมใช้ติดมาในตัว ครอบคลุม cadence การดูแล KMS ที่เจอบ่อย ๆ ได้แรงบันดาลใจมาจาก scheduled agent สี่ตัวของ obsidian-second-brain (nightly close, weekly review, contradiction sweep, vault-health) packaging มาให้ instantiate ตรง ๆ

```
❯ /schedule preset list
schedule presets:
  ID                     CRON           DESCRIPTION
  nightly-close          0 23 * * *     Wrap up the day — lint + auto-fix + stale-marker review (KMS '{kms}')
  weekly-review          0 9 * * SUN    Sunday-morning consolidation across active KMSes
  contradiction-sweep    0 12 * * *     Daily noon reconcile — auto-resolve clear-winner contradictions in '{kms}'
  vault-health           0 6 * * *      Morning lint summary at 06:00 for KMS '{kms}'

add via: /schedule preset add <id> --kms <name> [--cwd <path>]
```

แต่ละ preset เป็น cron expression รวมกับ prompt template ที่อ้าง `{kms}` (ระบบ substitute ตอน instantiate) ตัวอย่าง `nightly-close` รัน `/kms wrap-up <name> --fix`; `contradiction-sweep` รัน `/kms reconcile <name> --apply`

```
❯ /schedule preset add nightly-close --kms mynotes
✓ schedule 'nightly-close-mynotes' created from preset 'nightly-close' (cron: 0 23 * * *)
  Wrap up the day — lint + auto-fix + stale-marker review (KMS 'mynotes')
```

format ของ schedule id คือ `<preset-id>-<kms>` ดังนั้น preset เดียวกัน target หลาย KMS ได้ไม่ชน (`nightly-close-foo`, `nightly-close-bar`) หลัง instantiate แล้ว ตัวที่ได้คือ schedule ปกติ — แก้ cwd / cron / model ผ่าน `/schedule` คำสั่งทั่วไปได้ หรือ `/schedule rm <id>` เพื่อลบ

| Preset | เมื่อไหร่ | ทำอะไร |
|---|---|---|
| `nightly-close` | ทุกวัน 23:00 | เดิน pages, แก้ broken markdown link, append index entry ที่ขาด, refresh STALE page |
| `weekly-review` | อาทิตย์ 09:00 | consolidate page ที่ overlap เข้าเป็น canonical (ใส่ pointer ไม่ใช่ลบ) + ทำ hygiene pass |
| `contradiction-sweep` | ทุกวันเที่ยง | สแกน 4 pass (claims / entities / decisions / source-freshness), auto-resolve clear winner พร้อม `## History`, ทำ `Conflict — <topic>.md` สำหรับเคส ambiguous |
| `vault-health` | ทุกวัน 06:00 | health report read-only — broken link, orphan, missing-from-index, missing frontmatter, STALE marker |

> [!IMPORTANT]
> prompt ของ preset เป็น **คำสั่งภาษาธรรมชาติ** ที่บอก agent ให้ใช้ KMS tools (KmsRead/Search/Write/Append) ตรง ๆ scheduler ยิง preset ผ่าน `thclaws --print` ซึ่งไม่ run slash-command dispatch — preset prompt จึงใช้ slash command (เช่น `/kms reconcile`) ไม่ได้ `.thclaws/settings.json` ของ cwd ต้องมี KMS เป้าหมายอยู่ใน `kms_active` เพื่อให้ KMS tools register ก่อน agent เริ่ม

## สูตรการใช้งานจริง

### Briefing ตอนเช้าทุกวันทำงาน

```sh
thclaws schedule add morning-brief \
  --cron "30 8 * * MON-FRI" \
  --cwd ~/projects/web \
  --prompt "อ่าน git log ตั้งแต่เมื่อวาน ระบุ PR ที่ต้องรีวิว ตรวจ CI status เขียนลง ~/Desktop/morning-brief.md" \
  --timeout 600
```

### งานวิจัยระยะยาว สะสมข้ามคืน

ตั้ง `lastRun` ครั้งแรกผ่าน JSON แล้ว prompt แบบ resume-aware จะสะสมความคืบหน้าข้ามการยิงแต่ละครั้ง:

```sh
thclaws schedule add research-harness \
  --cron "0 * * * *" \
  --cwd ~/research \
  --prompt "ทำงานต่อใน harness-engineering.md หาแหล่งข้อมูลใหม่ 1 แหล่ง ผนวกเข้าเอกสาร และบันทึกความคืบหน้า" \
  --timeout 1800
```

### สแกน hygiene ทุกคืน

```sh
thclaws schedule add nightly-hygiene \
  --cron "0 2 * * *" \
  --cwd ~/projects/myapp \
  --prompt "สแกนหา TODO เก่ากว่า 30 วัน, clippy warning ที่เพิ่มเข้ามาสัปดาห์นี้, doc drift เขียนลง dev-log/hygiene-{date}.md" \
  --timeout 1200
```

### Auto-summarize-on-save (workspace watch ไม่ใช้ cron)

เฝ้าดูโฟลเดอร์ docs; ทุกครั้งที่มีไฟล์เปลี่ยน ให้ regenerate summary ใหม่ ตั้ง cron expression เป็นค่าในอนาคตไกลๆ เพื่อให้ trigger เฉพาะ watch ทำงาน — daemon-only, in-process scheduler จะข้าม flag นี้

```sh
thclaws schedule add docs-summary \
  --cron "0 0 1 1 *" \
  --cwd ~/projects/docs \
  --prompt "อ่านทุกอย่างใต้ . แล้วอัปเดต SUMMARY.md ด้วยภาพรวม 200 คำ" \
  --watch \
  --timeout 600
```

Cooldown 60 วินาทีหลังแต่ละ fire กันไม่ให้การเขียน `SUMMARY.md` ของ agent เอง re-trigger ทันที Debounce 2 วินาทีรวบ atomic-rename burst ของ editor ให้กลายเป็น 1 fire

### CI babysitter (ทุก 5 นาทีในเวลาทำงาน)

```sh
thclaws schedule add ci-watch \
  --cron "*/5 9-18 * * MON-FRI" \
  --cwd ~/projects/myapp \
  --prompt "ตรวจว่า CI ของ PR #42 ผ่านหรือไม่ ถ้าล้มเหลว ดึง log มาดู ระบุ test ที่ fail เขียน triage note ลง /tmp/ci-triage.md" \
  --timeout 300
```

## Slash command ภายใน thClaws

การจัดการ schedule ส่วนใหญ่สามารถทำได้โดยไม่ต้องออกจาก thClaws ไปที่ shell จาก CLI REPL หรือแท็บ Chat ของ GUI พิมพ์ `/schedule` (หรือใช้ตัวย่อ `/sched`):

| Slash | พฤติกรรม |
|---|---|
| `/schedule` หรือ `/schedule list` | แสดงรายการ schedule ทั้งหมดพร้อม flag on/off และข้อมูลการยิงล่าสุด |
| `/schedule show <id>` | พิมพ์รายละเอียดของ schedule หนึ่งรายการเป็น JSON |
| `/schedule run <id>` | ยิง schedule หนึ่งครั้งแบบ synchronous (ไม่ block — REPL ยังตอบสนองได้) |
| `/schedule status` | สถานะ daemon + สรุปการยิงล่าสุดของทุก schedule |
| `/schedule pause <id>` / `/schedule resume <id>` | เปลี่ยนค่า `enabled` โดยไม่ต้องลบ entry |
| `/schedule rm <id>` (หรือ `remove` / `delete`) | ลบ schedule ออกจาก store |
| `/schedule install` | Install daemon (launchd plist บน macOS, systemd-user unit บน Linux) |
| `/schedule uninstall` | หยุด daemon และลบ supervisor entry |
| `/schedule add` (หรือ `new` / `create`) | **GUI:** เปิด modal สำหรับกรอกข้อมูล (อธิบายด้านล่าง) **CLI:** พิมพ์คำแนะนำว่าควรใช้ shell subcommand |
| `/schedule preset list` (หรือ `ls`) | แสดง template สำเร็จรูปสำหรับดูแล KMS ทั้งสี่ตัว |
| `/schedule preset add <id> --kms <name>` | สร้าง schedule จาก template ตัวนั้น — ดู [preset](#preset-สำเร็จรูปสำหรับการดูแล-kms) |

`/schedule add` เป็นคำสั่งเดียวที่ทำงานต่างกันในแต่ละ surface เพราะ prompt หลายบรรทัดและ flag หลายตัวไม่เหมาะกับการพิมพ์บน REPL บรรทัดเดียว ดังนั้น:

- **ในแท็บ Chat ของ GUI** `/schedule add` จะเปิด form modal ที่เติม cwd ปัจจุบันและตัวอย่าง cron ไว้ให้ก่อน มีตัวช่วย 3 อย่างที่ลดการพิมพ์:
  - **Cron preset chips** (`Every 5 min`, `Hourly`, `Daily 9am`, `Weekdays 8:30`, `Weekly Mon 9am`, `Monthly 1st`) เติมค่า cron ในคลิกเดียว Chip ที่ตรงกับค่าใน field จะถูกไฮไลต์ด้วยสี accent
  - **Live next-fire preview** ตรวจสอบ cron 300 ms หลังหยุดพิมพ์และแสดง 3 ครั้งถัดไปที่จะยิง (เช่น `next fires: Tue, May 6 9:00 AM · Wed, May 7 9:00 AM · …`) ถ้าพิมพ์ผิด จะแสดง error inline — ไม่มี surprise ตอน submit
  - **Checkbox "Run when file in workspace changes"** ตั้งค่า `watchWorkspace: true` ใน entry เพื่อให้ daemon ยิงงานเมื่อมีการเปลี่ยนแปลงไฟล์ใน `cwd` ไม่ใช่แค่ตามตาราง cron (ดู [Trigger เมื่อ workspace เปลี่ยนแปลง](#trigger-เมื่อ-workspace-เปลี่ยนแปลง))
  กรอก `id`, `cron`, `prompt` และฟิลด์อื่น ๆ ที่ต้องการ จากนั้นคลิก **Save** Backend จะตรวจสอบ field ที่จำเป็น, syntax ของ cron, และว่า `cwd` มีอยู่จริง — ข้อผิดพลาดจะแสดง inline ใน form ถ้าสำเร็จ modal จะแสดงข้อความยืนยันสีเขียวและปิดอัตโนมัติ
- **ใน CLI REPL** `/schedule add` จะพิมพ์ syntax ของ shell subcommand พร้อม flag ทั้งหมดให้คัดลอกไปวางใน terminal ได้

Slash และ CLI surface ใช้ store เดียวกัน — schedule ที่เพิ่มผ่านเส้นทางใดก็ตามจะปรากฏใน `list` ของอีกฝั่ง, ยิงจาก daemon ตัวเดียวกัน, และเขียนลง `~/.config/thclaws/schedules.json` ไฟล์เดียวกัน

## การตรวจสอบการยิง

Log ของแต่ละงานอยู่ที่ `~/.local/share/thclaws/logs/<id>/`:

```sh
ls -lt ~/.local/share/thclaws/logs/morning-brief/ | head -5
tail -f ~/.local/share/thclaws/logs/morning-brief/$(ls -t ~/.local/share/thclaws/logs/morning-brief | head -1)
```

Log ของตัว daemon (ข้อความตอน startup, การประกาศ tick ของ scheduler) อยู่ที่ `~/.local/share/thclaws/daemon.log`

## การแก้ปัญหา

**Schedule ไม่ยิงทั้งที่ควรจะยิง**
1. ตรวจ `thclaws schedule status` — daemon รันอยู่ไหม?
2. ดู `~/.local/share/thclaws/daemon.log` มี error อะไรไหม
3. ตรวจ `enabled: true` ใน store
4. จำไว้ว่า: skip-catch-up เป็นค่าดีฟอลต์ schedule ที่เพิ่มตอน 09:00 ด้วย cron `30 8 * * *` จะไม่ยิง 08:30 ของวันนี้ย้อนหลัง — มันจะยิงพรุ่งนี้ 08:30

**Daemon ปฏิเสธไม่ยอม start: "another daemon is already running"**
มี daemon ก่อนหน้าที่ยังมีชีวิต `thclaws schedule status` จะแสดง PID จะปล่อยให้รันต่อก็ได้ หรือ `kill <pid>` แล้วลองใหม่

**PID file ค้างหลังจาก crash**
`thclaws schedule status` จะรายงาน `thclaws daemon` ครั้งต่อไปจะเก็บไฟล์คืนอัตโนมัติ — ไม่ต้อง cleanup ด้วยมือ

**Laptop sleep**
บน macOS LaunchAgent จะไม่ยิงขณะ laptop sleep (ปิดฝา) Schedule ที่ควรจะยิงระหว่าง sleep ต้องการ plumbing แบบ `WakeMonitor` (เลื่อนไว้ — ยังไม่รองรับ) ตอนนี้ให้คาดหวัง semantics แบบ "ยิงตอนใช้งาน laptop"

**มี scheduler 2 ตัวรันพร้อมกัน**
ถ้าคุณ install daemon และเปิด `thclaws --cli` โดยไม่ใช้ `--no-scheduler` ทั้ง 2 surface จะ tick store เดียวกัน ทั้งคู่ใช้ guard skip-overlap เดียวกัน ดังนั้นการยิงซ้ำจึงเกิดได้น้อยมาก แต่ setup ที่สะอาดกว่าคือใช้ `--no-scheduler` ทุกครั้งที่ install daemon

## ข้อจำกัดที่ควรทราบ

- **ยังไม่รองรับ daemon บน Windows** `schedule install` จะแสดง error "not yet supported on this platform" บน Windows ชั้นที่ 1 และ 2 (manual run + in-process scheduler) ใช้งานข้ามแพลตฟอร์มได้ มีเฉพาะ daemon (และ `watchWorkspace` ที่ผูกกับ daemon) ที่รองรับ macOS/Linux ในตอนนี้
- **ไม่มี IPC** Daemon และ CLI สื่อสารกันผ่าน store บนดิสก์ + PID file เท่านั้น `schedule logs --tail` แบบ live, `schedule reload`, และ metric ฝั่ง daemon ถูกเลื่อนไว้ การแก้ไข `schedules.json` จะมีผลภายใน 30 วินาทีผ่าน polling tick (ตัว reconciler ของ watcher ก็ใช้ cadence เดียวกันสำหรับการ toggle `watchWorkspace`)
- **ไม่มีฟิลด์ catch-up policy สำหรับ cron** Skip-catch-up เป็น policy เดียวสำหรับ schedule แบบทำซ้ำ การ catch-up แบบ manual ผ่านการแก้ `lastRun` คือ workaround ส่วน one-shot (`--at` / `--in`) เป็นข้อยกเว้นที่ตั้งใจไว้ — `run_at` ที่เลยมาแล้วจะยิงเสมอ
- **ไม่มี log rotation** `~/.local/share/thclaws/daemon.log` และ `~/.local/share/thclaws/logs/<id>/*.log` จะโตขึ้นเรื่อย ๆ ตอนนี้ให้ตัดทิ้งด้วยมือ หรือเขียน cron entry ของคุณเอง
- **รายการ ignore ของ workspace watch ถูก hardcode** `.thclaws/`, `.git/`, `node_modules/`, `target/`, `dist/`, `build/`, `.next/`, `.cache/`, `.DS_Store` ยังไม่อ่าน `.gitignore` ถ้าต้องการ control ละเอียดกว่านี้ ให้ใช้ cron อย่างเดียว
- **ข้อจำกัดของ OS watch** `inotify` ของ Linux ค่าเริ่มต้นรองรับ 8192 watches ต่อ user; recursive watch บน tree ขนาดใหญ่อาจทะลุได้ Daemon จะ log error และข้าม watcher ตัวนั้น schedule อื่นยังทำงานต่อได้
