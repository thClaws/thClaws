# บทที่ 1 — thClaws คืออะไร?

![logo](../user-manual-img/logo/thClaws-logo-line-art-banner.png)

thClaws คือ **AI Agent Platform ที่เขียนด้วย Rust แบบ native** รันบน
เครื่องของคุณเอง สำหรับสร้าง AI Agent มาช่วยคุณทำงานหลากหลาย เช่น
เขียนโปรแกรม ทำงานอัตโนมัติ ตรวจสอบและจัดระเบียบเอกสาร จัดการ
Knowledge Base หรือสร้างทีม AI Agent ทำงานร่วมกัน — ทั้งหมดรวมอยู่ใน
binary เดียว แค่บอกเป็นภาษาธรรมชาติว่าต้องการอะไร แล้ว agent จะอ่านไฟล์
รันคำสั่ง ใช้ tool และพูดคุยโต้ตอบกับคุณระหว่างทำงาน

เก้า surface รวมอยู่ใน binary เดียว ใช้ `Agent` loop, `Session` และ tool
registry ชุดเดียวกัน — แปด surface แรกสำหรับ "คน" คนหนึ่ง (รวมถึงคุย
ผ่าน LINE, Telegram หรือ Facebook Messenger บนมือถือ), surface ที่เก้า
ให้ "ซอฟต์แวร์อื่น" เรียกใช้ thClaws ไปทำงาน นอกเหนือจาก binary แล้ว
**[thClaws.cloud](#thclawscloud)** ยังเพิ่ม catalog ให้เลือกใช้และ
hosted runtime ให้เช่า — ดู bullet ด้านล่างและ [บทที่ 27](ch27-thclaws-cloud.md):

- **Desktop GUI** (`thclaws` โดยไม่ใส่ flag) — หน้าต่าง native ประกอบด้วย
  แท็บ Terminal ที่รัน REPL ตัวเดียวกับโหมด `--cli`, แท็บ Chat แบบ
  streaming, Files browser และแท็บ Team (ตัวเลือกเสริม)
- **CLI REPL** (`thclaws --cli`) — prompt โต้ตอบใน terminal เหมาะกับการใช้
  ผ่าน SSH, เซิร์ฟเวอร์ headless หรือเมื่อไม่ต้องการ overhead ของ GUI
- **โหมดไม่โต้ตอบ** (`thclaws -p "prompt"` รูปแบบเต็มคือ `--print`)
  — รันแค่หนึ่ง turn แล้วออก สะดวกสำหรับสคริปต์ CI pipeline หรือ
  one-liner ใน shell ใส่ `-v` / `--verbose` เพื่อให้แสดง token usage
  ของ turn บน stderr โดยไม่รบกวน stdout
- **Webapp** (`thclaws --serve --port 7878` + เปิด browser) — engine
  ตัวเดียวกันผ่าน WebSocket/HTTP เปิดจาก laptop คุณเอง เข้าถึงระยะไกล
  ผ่าน SSH tunnel ได้ — "thClaws ทุกที่" โดยไม่ต้องเปิด port
- **LINE Chat** (GUI → Line Connect modal เท่านั้น ไม่มี flag `--line`
  ต่างจาก Telegram และ Messenger) —
  คุยกับ agent ผ่าน LINE OA ของคุณเอง ทำงานผ่าน relay tunnel ที่
  `line.thclaws.ai` ซึ่งเชื่อมระหว่าง LINE platform กับ thClaws ที่รัน
  บนเครื่องคุณ — agent อยู่ในเครื่องคุณ แต่เรียกใช้ได้จากที่ไหนก็ได้
  ผ่านมือถือ (ดู [บทที่ 21](ch21-line-and-browser-chat.md))
- **Telegram bot** (`thclaws --telegram` หรือ GUI Telegram Connect
  modal) — สร้างบอทด้วย `@BotFather` วาง token แล้วทุกข้อความที่ DM หา
  บอทจะรันเป็น turn บนเครื่องคุณ tool call ที่ต้องอนุมัติจะมาเป็นปุ่ม
  inline-keyboard กดจากมือถือได้ (ดู [บทที่ 23](ch23-telegram.md))
- **Facebook Page Messenger** (`thclaws --messenger` หรือ GUI Messenger
  Connect modal) — เชื่อม Facebook Page ครั้งเดียว แล้วทุก DM Messenger
  ถึง Page จะรันเป็น turn บนเครื่องคุณ การอนุมัติแสดงเป็น quick-reply
  chip ที่กดจากมือถือได้ (ดู [บทที่ 24](ch24-messenger.md))
- **thClaws Remote** (จับคู่กับบัญชี thClaws.cloud ผ่าน GUI) — เครื่องของ
  คุณเป็นฝ่าย *โทรออก* แล้วค้าง tunnel ไว้ คุณจึงคุยกับ agent บนเครื่อง
  ตัวเองผ่าน dashboard บน cloud ได้ โดยไม่ต้องมี public IP และไม่ต้องเปิด
  port เข้า ต่างจาก chat bridge ตรงที่ผูกกับ *บัญชี cloud* ของคุณ ไม่ใช่
  แพลตฟอร์มแชท
- **AI Agent (API Server)** (`thclaws --serve` + HTTP API) — ให้
  *ซอฟต์แวร์อื่น* (orchestrator, external client, scheduler) เรียกใช้
  thClaws เป็น agent ผ่าน HTTP API เดียวกัน — รายละเอียดอยู่ในบทถัด ๆ ไป

## สิ่งที่ทำให้ thClaws แตกต่าง

- **thClaws.cloud — เลือกใช้ รัน และโฮสต์ agent** — AI agent ใน thClaws
  คือ "โฟลเดอร์" ([บทที่ 8](ch08-memory-and-agents-md.md)) และ
  thClaws.cloud เปลี่ยนโมเดลโฟลเดอร์นี้ให้เป็น *git สำหรับ AI agent*
  **เลือกใช้** จาก catalog ที่
  [thclaws.cloud/templates](https://thclaws.cloud/templates) **ติดตั้ง** ลง
  โฟลเดอร์ในเครื่องด้วยคำสั่งเดียว (`/cloud get <slug>`) **เผยแพร่** ของ
  ตัวเอง (`/cloud publish`) — คุณเป็นเจ้าของโฟลเดอร์และใช้ provider key
  ของคุณเอง ผูกเดสก์ท็อปกับ catalog ด้วยการวาง CLI token ครั้งเดียวที่
  Settings → thClaws.cloud จากนั้นทุก catalog op เป็น slash command ใน
  session ที่เปิดอยู่ สำหรับทีมมี **hosted runtime** (managed runner
  ไม่ต้องตั้งค่า — ตอนนี้อยู่ในช่วง closed beta) และ **shared agent**:
  agent ของบริษัทที่หลายคนใช้ร่วมกัน คิดเงินผ่าน gateway ไปที่เจ้าของ
  พร้อม knowledge base ของบริษัทแบบอ่านอย่างเดียว
  ดู [บทที่ 27](ch27-thclaws-cloud.md) <a id="thclawscloud"></a>
- **Self-improving AI Agent (auto-learn)** — เปิด `autoLearn: true` ใน
  settings แล้ว agent จะเรียนรู้จากตัวเองอัตโนมัติ ทุก session ที่จบลง
  จะถูกบันทึกเป็น KMS page ใน `self_learn` (แยกจาก KMS ที่คุณ curate
  เอง) และตามรอบ (default ทุก 6 ชั่วโมง) จะ reconcile contradictions
  ในนั้น สร้างจากปริมิทีฟที่มีอยู่แล้ว (`/kms ingest`, `/kms reconcile`)
  — ไม่มี prompt agent ใหม่ แค่เปิด/ปิดด้วย flag เดียว ถ้าอยากเริ่มใหม่
  ลบโฟลเดอร์ `self_learn/` ทิ้งได้เลย ([บทที่ 9 §Self-improving AI Agent](ch09-knowledge-bases-kms.md#self-improving-ai-agent-auto-learn))
- **4 ระดับของ agent orchestration** —
  **`Task` tool** (model ตัดสินใจ block parent's turn),
  **`/agent <name>`** (user สั่งเอง รันขนานกับ main ไม่เข้า history),
  **Agent Team** (หลาย process, mailbox + task queue, แต่ละคนมี
  worktree ของตัวเอง) และ **Workflow (`/workflow`)** — ตัว
  orchestrate เป็น *โค้ด* ไม่ใช่ model: LLM เขียนสคริปต์ JavaScript ที่
  fan-out งานไปยัง subagent หลายตัว แล้ว JS engine แบบ sandbox รัน
  อย่าง deterministic บนเครื่องคุณ รันซ้ำได้ผลรูปแบบเดิม งานยาว ๆ ทิ้ง
  checkpoint ไว้ resume ได้ เหมาะกับงาน **bulk ที่เป็นอิสระต่อกัน** (เช่น
  "แก้ไฟล์เทสต์ทั้ง 800 ไฟล์ให้ใช้ fixture ใหม่") ดู
  [บทที่ 15](ch15-subagents.md), [บทที่ 17](ch17-agent-teams.md) และ
  [บทที่ 25](ch25-workflows.md)
- **Hire-able as a working agent — self-hosted sandbox ของคุณ** —
  ทิศกลับของ orchestration: thClaws เป็น *worker* ให้ orchestrator
  ตัวอื่น (เช่น scheduler, CI job หรือ control plane ของคุณเอง)
  จ้างไปทำงาน ทั้งแบบ **Employee** (`thclaws_local` — process บน
  เครื่องเดียวกัน — เทียบเท่า in-process sandbox) และ **Freelancer**
  (`thclaws_pod` — pod แยก รันบน VPS, cloud หรือ k3s ของคุณเอง —
  เทียบเท่า self-hosted sandbox ที่ agent loop อยู่ฝั่ง orchestrator
  ส่วน tool execution อยู่ใน perimeter ของ *คุณ*) orchestrator พูดผ่าน
  HTTP API เดียวกับที่ user/IDE ใช้
  (ผ่าน `POST /agent/run` และ `GET /v1/agent/info`)
- **จำสิ่งที่สำคัญในระยะยาว 3 ระดับ** —
  **`AGENTS.md` (หรือ `CLAUDE.md`)** ในโปรเจกต์ โดนฉีดเข้า prompt อัตโนมัติ
  ([บทที่ 8](ch08-memory-and-agents-md.md));
  **memory store** เก็บข้อเท็จจริงที่ agent เรียนรู้เกี่ยวกับตัวคุณและ
  โปรเจกต์ — อยู่ที่ `.thclaws/memory/` เมื่อทำงานในโปรเจกต์ (จึงติดไปกับ
  repo) ถ้าไม่มีโปรเจกต์จะถอยไปใช้ `~/.local/share/thclaws/memory/`;
  **KMS (knowledge bases)** wiki หลายหน้าที่ agent ค้น/อ่าน/เขียนเอง
  ค้นได้ทั้งแบบ grep และ **BM25 จัดอันดับความเกี่ยวข้อง** (`query:`)
  โดยไม่ใช้ embedding — ตามแนว LLM-wiki ของ Karpathy ดูแลอัตโนมัติด้วย
  side-channel agent (`/dream`, `/kms reconcile`, `/kms challenge`);
  **เปิดดูและวาดกราฟ** ใน GUI: browser ของ KMS, **graph view** แบบ
  Obsidian ของ `[[wikilink]]` และ `/kms html` export เว็บ interactive
  ไฟล์เดียวไว้แชร์ได้; และ **แลกเปลี่ยนข้ามทีมด้วย OKF** (Open Knowledge
  Format ของ Google): `/kms export-okf` / `/kms import-okf` — ทั้งหมด
  เป็น markdown ที่คุณอ่าน แก้ไข หรือ commit ได้
  ([บทที่ 9](ch09-knowledge-bases-kms.md))
- **ประกอบ agent เองจาก building block** —
  **Skill** ([บทที่ 12](ch12-skills.md)) สำหรับ workflow ที่ใช้ซ้ำได้,
  **MCP server** ([บทที่ 14](ch14-mcp.md)) สำหรับเสียบ tool ภายนอก
  (GitHub, DB, Browser, Slack ฯลฯ),
  **Plugin** ([บทที่ 16](ch16-plugins.md)) สำหรับแพ็กทุกอย่างรวมกัน,
  **Knowledge base** ([บทที่ 9](ch09-knowledge-bases-kms.md)) สำหรับ
  wiki ที่ agent ค้น/อ่าน/เขียนเอง พร้อม `/dream` ที่ consolidate KMS
  จาก session ล่าสุดให้อัตโนมัติ
- **รองรับหลาย provider อย่างเท่าเทียม** — Anthropic (native + Claude
  Agent SDK), OpenAI (Chat + Responses/Codex), Google Gemini & Gemma,
  Alibaba DashScope (Qwen), DeepSeek, Z.ai (GLM Coding Plan), NVIDIA
  NIM, NSTDA Thai LLM (OpenThaiGPT, Typhoon, Pathumma, THaLLE),
  OpenRouter, TokenRouter, Moonshot, xAI, Groq, MiniMax, Qwen Cloud,
  AtlasCloud, Unifically, Meta, 9router, OpenCode Go, Azure AI Foundry,
  Ollama (local + Anthropic-compat + Cloud), LMStudio,
  vLLM / llama.cpp / LiteLLM ที่
  self-host เอง และ slot OpenAI-compatible
  ทั่วไป (`oai/*`) — สลับกลางคันด้วย `/model` หรือ `/provider` ได้
  ([บทที่ 6](ch06-providers-models-api-keys.md))
- **API พร้อมใช้กับเครื่องมือมาตรฐาน** — `--serve` เปิดทั้ง
  `/v1/chat/completions` (OpenAI-compatible สำหรับ Cursor, Aider, n8n,
  openai-python), `/v1/messages` (Anthropic-compatible สำหรับ SDK
  `anthropic` หรืออะไรก็ตามที่ตั้ง `ANTHROPIC_BASE_URL`) และ
  `/agent/run` + `/v1/agent/info` (thClaws-native สำหรับ orchestrator)
  — agent ตัวเดียวให้บริการได้ทั้งคนและซอฟต์แวร์พร้อมกัน
- **Async webhook delivery** — งานที่รันยาว (deploy, build, multi-step
  research) ส่ง prompt + `x_callback` แล้วปิด connection ได้ thClaws
  จะ POST ผลกลับเมื่อทำเสร็จ ทนต่อ network blip และ orchestrator pod
  restart ระหว่างทาง
- **Plan mode** — สำหรับงานหลาย step ให้ agent `EnterPlanMode` แล้ว
  เสนอ step ทีละขั้น ให้ *คุณ* review และอนุมัติก่อนรัน แต่ละ step
  มี retry budget; failure จะหยุด chain เพื่อให้คุณตัดสินใจต่อ
  ([บทที่ 18](ch18-plan-mode.md))
- **Schedule + cron + watchWorkspace** — `/schedule add` รัน agent
  ตาม cron, fixed interval หรือเมื่อ directory เปลี่ยน มีทั้ง manual,
  in-process scheduler และ daemon (launchd / systemd-user) ที่อยู่
  รอดได้ข้าม reboot ([บทที่ 19](ch19-scheduling.md))
- **Long-running loops** — `/loop` สำหรับ iterate ตาม interval,
  `/goal` สำหรับ audit-driven completion (ทำต่อจนกว่า audit prompt
  จะตอบว่า "เสร็จ" หรือชน budget) ผสม `/goal --auto` ได้สำหรับ
  Ralph-style overnight builder
- **Document workflow** — read + edit + create PDF, DOCX, PPTX, XLSX
  ในตัว plus image rendering — ingest PDF 50 หน้าเข้า KMS แล้วผลิต
  PowerPoint ออกมาในการสนทนาเดียวได้
- **Hooks** — รัน shell script บน lifecycle event ของ agent (8 event
  รวม pre/post_tool_use, permission_denied, session_start,
  pre_compact ฯลฯ) audit ทุก Bash, gate Edit/Write ผ่าน linter,
  แจ้ง Slack เมื่อ session ยาว ๆ จบ ([บทที่ 13](ch13-hooks.md))
- **เหมาะกับทุกสายงาน ไม่ใช่แค่วิศวกร** — Chat tab แบบ streaming สำหรับ
  นักวิจัย PM ฝ่ายกฎหมาย/การตลาด Terminal REPL สำหรับวิศวกร — ใช้
  session และ config ชุดเดียวกัน สลับไปมาได้โดยไม่เสีย context
  ([บทที่ 4](ch04-desktop-gui-tour.md))
- **native บนเครื่อง คุมข้อมูลเอง** — Rust binary ตัวเดียว ไม่ต้องมี service
  เบื้องหลัง ไม่ต้อง cloud จะรันกับ Ollama แบบ offline ล้วน ๆ ก็ได้
- **รันได้ทุกแพลตฟอร์ม** — binary ตัวเดียวกันรันบน macOS (Apple Silicon
  + Intel), Windows, Linux ได้ ใส่ใน Docker container เพื่อ deploy ขึ้น
  VPS / cloud / Kubernetes ก็ได้ — code ตัวเดียวรองรับตั้งแต่ laptop
  ส่วนตัวจนถึง pod บน cluster
- **ยึดมาตรฐานเปิด ไม่ผูกกับ vendor** — ใช้
  [MCP](https://modelcontextprotocol.io/) สำหรับ tool,
  [`AGENTS.md`](https://agents.md) สำหรับ instruction (มาตรฐานที่
  Google, OpenAI, Cursor, Sourcegraph, Factory ใช้), `SKILL.md` สำหรับ
  workflow และ `.mcp.json` สำหรับตั้งค่า MCP server — config
  ขนย้ายไปใช้กับเครื่องมืออื่นที่พูดมาตรฐานเดียวกันได้
- **ความปลอดภัยมาก่อน** — filesystem sandbox จำกัดขอบเขตของ tool
  ไฟล์อยู่ที่ working directory tool ที่เปลี่ยนสถานะต้อง approve
  (ยกเว้นจะตั้ง auto-approve เอง) API key เก็บใน OS keychain หรือ
  `.env` ตามที่คุณเลือกตอนเปิดใช้ครั้งแรก permission request จะติดป้าย
  ว่า agent ตัวไหนกำลังขอ (main, side-channel หรือ subagent)
  ป้องกันการ approve ผิดตัว ([บทที่ 5](ch05-permissions.md))
- **ค่าใช้จ่ายโปร่งใส** — model catalogue ในตัวเก็บราคา per-token-type
  (input / output / cached read / cache write / reasoning) sync จาก
  [LiteLLM](https://github.com/BerriAI/litellm) อัตโนมัติ usage block
  ของทุก turn แสดงครบ orchestrator/UI คำนวณ cost ในเครื่องได้โดยไม่
  ต้องถาม provider
- **Host thClaws ที่ไหนก็ได้** — ใช้บนเครื่องตัวเองได้ หรือ deploy ขึ้น
  host ที่คุณเลือก เพื่อให้ thClaws รันบน cloud
  ในชื่อของคุณ — จะถูก *Company จ้าง* (เป็น employee / freelancer ผ่าน
  orchestrator) หรือยืนเดี่ยวรับงานเองก็ได้
  flow การ deploy มาในรูป plugin host จึงสลับเปลี่ยนได้ ไม่มีการล็อก
  client
- **Session resume** — `thclaws --resume last` ทำงานต่อจาก session
  ล่าสุด, `thclaws --resume <id>` กระโดดไป session ที่ระบุ session
  เก็บเป็น JSONL ที่ `.thclaws/state/sessions/` — git-friendly, grep-friendly
  ([บทที่ 7](ch07-sessions.md))
- **Settings อยู่ในไฟล์ JSON ไฟล์เดียว** — permission mode, thinking
  budget, allowed/disallowed tool, endpoint ของ provider, KMS ที่แนบไว้,
  max output tokens รวมอยู่ใน `.thclaws/settings.json` (ระดับโปรเจกต์
  commit ลง repo ได้) หรือ `~/.config/thclaws/settings.json` (ระดับ
  ผู้ใช้ทั้งระบบ)
- **Shell escape** — ใส่ `!` นำหน้าบรรทัดใน REPL เพื่อรันคำสั่ง shell
  โดยตรง ไม่เสีย token ไม่มี prompt ขออนุมัติ (เช่น `! git status`)

## สิ่งที่คุณต้องมี

- OS ที่รองรับ: macOS (arm64 หรือ x86_64), Linux (arm64 หรือ x86_64)
  หรือ Windows (arm64 หรือ x86_64)
- API key ของ LLM อย่างน้อยหนึ่งเจ้า — Anthropic, OpenAI, Gemini,
  OpenRouter, Moonshot, xAI, Groq, DashScope, DeepSeek, Z.ai, NVIDIA NIM,
  NSTDA Thai LLM หรือ Azure AI Foundry (หรือจะติดตั้ง Ollama /
  LMStudio บนเครื่องเอง ถ้าต้องการใช้แบบ offline)

[บทที่ 2](ch02-installation.md) จะพาติดตั้งและเปิดใช้ครั้งแรก
[บทที่ 6](ch06-providers-models-api-keys.md) อธิบายว่าจะวาง
key ที่ไหนและอย่างไร

## คู่มือเล่มนี้จัดเรียงอย่างไร

คู่มือเล่มนี้ 28 บท จัดเรียงเป็น reference อธิบายวิธีติดตั้งและทุก
ฟีเจอร์ที่ผู้ใช้สัมผัสได้ ทีละเรื่อง พร้อมคำสั่งและการตั้งค่าที่จำเป็น:

**ตั้งค่าและเริ่มต้น**
- [บทที่ 2](ch02-installation.md) — ติดตั้ง
- [บทที่ 3](ch03-working-directory-and-modes.md) — working directory + โหมดการรัน
- [บทที่ 4](ch04-desktop-gui-tour.md) — ทัวร์ Desktop GUI
- [บทที่ 5](ch05-permissions.md) — permissions
- [บทที่ 6](ch06-providers-models-api-keys.md) — providers, models, API keys

**ฟีเจอร์หลัก**
- [บทที่ 7](ch07-sessions.md) — sessions และ resume
- [บทที่ 8](ch08-memory-and-agents-md.md) — memory และ AGENTS.md
- [บทที่ 9](ch09-knowledge-bases-kms.md) — Knowledge bases (KMS) รวมถึง self-improving auto-learn
- [บทที่ 10](ch10-slash-commands.md) — slash commands
- [บทที่ 11](ch11-built-in-tools.md) — built-in tools
- [บทที่ 12](ch12-skills.md) — skills
- [บทที่ 13](ch13-hooks.md) — hooks
- [บทที่ 14](ch14-mcp.md) — MCP

**ประกอบ agent ขั้นสูง**
- [บทที่ 15](ch15-subagents.md) — subagents
- [บทที่ 16](ch16-plugins.md) — plugins
- [บทที่ 17](ch17-agent-teams.md) — agent teams
- [บทที่ 18](ch18-plan-mode.md) — plan mode
- [บทที่ 19](ch19-scheduling.md) — scheduling
- [บทที่ 20](ch20-research.md) — `/research` (background research)
- [บทที่ 25](ch25-workflows.md) — Workflows (orchestration ระดับที่สี่)

**เข้าถึงจากที่อื่น**
- [บทที่ 21](ch21-line-and-browser-chat.md) — LINE chat + browser bridge
- [บทที่ 23](ch23-telegram.md) — Telegram bot
- [บทที่ 24](ch24-messenger.md) — Facebook Page Messenger bot
- [บทที่ 27](ch27-thclaws-cloud.md) — thClaws.cloud (catalog + hosted runtime)

**Surface ขั้นสูงและงานอัตโนมัติ**
- [บทที่ 26](ch26-gui-shells.md) — GUI Shells (frontend เฉพาะทาง)
- [บทที่ 28](ch28-browser-automation.md) — Browser automation

ถ้าเพิ่งเริ่ม อ่านบทที่ 2 ต่อได้เลย ถ้าย้ายมาจาก Claude Code แนะนำให้
ข้ามไปบทที่ 6, 7, 11 และ 13 ถ้าคุ้นเคยพื้นฐานแล้วและสนใจของใหม่ ฟีเจอร์
ที่เพิ่งเพิ่มเข้ามาอยู่ในบทที่ 9 (auto-learn และ `/dream`), บทที่ 15
(`/agent` side-channels), บทที่ 21 (LINE), บทที่ 23 (Telegram), บทที่ 24
(Messenger) และ — ไฮไลต์ของรุ่นนี้ — บทที่ 27 (thClaws.cloud)
