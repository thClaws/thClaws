# บทที่ 6 — provider, model และ API key

thClaws คุยกับ **provider ได้ทั้งหมดสามสิบเอ็ดราย** โดยตรวจจับให้อัตโนมัติ
จากชื่อ model และสลับได้ตลอดเวลาด้วย `/model`, `/provider` หรือ
คลิกที่แถบ provider/model ใน sidebar (Desktop GUI, v0.7.2+)

## ภาพรวม provider

| Provider | Model prefix | Auth env var | หมายเหตุ |
|---|---|---|---|
| Moonshot | `moonshot/*` | `MOONSHOT_API_KEY` | Moonshot AI (Kimi); ค่า default `moonshot/kimi-k2.6` |
| xAI | `xai/*` | `XAI_API_KEY` | xAI Grok; ค่า default `xai/grok-4.3` |
| Groq | `groq/*` | `GROQ_API_KEY` | Groq LPU (เร็วมาก); `groq/llama-3.3-70b-versatile` |
| TokenRouter | `tokenrouter/*` | `TOKENROUTER_API_KEY` | Router รวมเข้าถึง 300+ model (`tokenrouter/<vendor>/<model>`) |
| AtlasCloud | `atlascloud/*` | `ATLASCLOUD_API_KEY` | router แบบ OpenAI-compatible ที่โฮสต์ให้; ค่าเริ่มต้น `atlascloud/qwen/qwen3.5-flash` |
| Unifically | `unifically/*` | `UNIFICALLY_API_KEY` | router แบบ OpenAI-compatible ที่โฮสต์ให้; ค่าเริ่มต้น `unifically/google/gemini-3.5-flash` |
| Meta | `meta/*` | `META_API_KEY` | API ที่ Meta โฮสต์เอง; ค่าเริ่มต้น `meta/muse-spark-1.2` |
| 9router | `9router/*` | `NINEROUTER_API_KEY` | router แบบ **self-host** — ตั้ง `NINEROUTER_BASE_URL` (ค่าเริ่มต้น `http://localhost:20128/v1`) แก้ base URL ได้ใน Settings |
| Qwen Cloud | `qwen-cloud/*` | `QWENCLOUD_API_KEY` | บริการ Qwen ที่โฮสต์โดย Alibaba |
| Anthropic | `claude-*` | `ANTHROPIC_API_KEY` | extended thinking, prompt caching (system + tools) |
| Anthropic Agent SDK | `agent/*` | — (ใช้ auth ของ Claude Code เอง) | ขับ `claude` CLI ผ่าน subscription Claude Pro / Max แทนการคิดเงินแบบ API ⚠ tool registry ของ thClaws ไม่ข้าม subprocess boundary — model เห็นเฉพาะ toolset ของ Claude Code เท่านั้น tool ของ KMS / MCP / Agent Teams เข้าถึงไม่ได้จาก provider นี้ ต้องสลับไป `claude-*` หากต้องการใช้ |
| OpenAI | `gpt-*`, `o1-*`, `o3*`, `o4-*` | `OPENAI_API_KEY` | Chat Completions; prompt caching อัตโนมัติ |
| OpenAI Responses | `codex/*` | `OPENAI_API_KEY` | Responses API — รูปแบบ agentic-native ที่ใหม่กว่า |
| OpenAI-Compatible | `oai/*` | `OPENAI_COMPAT_API_KEY` (+ `OPENAI_COMPAT_BASE_URL`) | endpoint OAI-compat แบบ generic — ชี้ไป LiteLLM/Portkey/Helicone/vLLM/proxy ภายในองค์กร ที่พูด `/v1/chat/completions` ได้; prefix `oai/` ถูก strip ก่อน forward |
| OpenRouter | `openrouter/*` | `OPENROUTER_API_KEY` | gateway รวม เข้าถึง model 300+ ตัวจากผู้ให้บริการ LLM รายใหญ่ทุกเจ้า |
| OpenCodeGo | `opencode-go/*` | `OPENCODE_GO_API_KEY` | gateway subscription ของ opencode.ai มี base URL เดียวให้บริการสาม wire shape (OpenAI-compatible สำหรับ GLM/Kimi/DeepSeek/MiMo, Anthropic-compatible สำหรับ MiniMax M2.x, Alibaba-compatible สำหรับ Qwen3.x Plus); provider auto-route จาก model id เพิ่มใน v0.9.6 |
| Gemini | `gemini-*`, `gemma-*` | `GEMINI_API_KEY` | Gemma ให้บริการผ่าน Google AI Studio |
| Ollama | `ollama/*` | — (local) | NDJSON streaming; ไม่ต้อง auth |
| Ollama Anthropic | `oa/*` | — (local, v0.14+) | endpoint `/v1/messages` ของ Ollama ที่เข้ากันกับ Anthropic |
| DashScope | `qwen-*`, `qwq-*` | `DASHSCOPE_API_KEY` | Qwen ของ Alibaba; caching อัตโนมัติ |
| DeepSeek | `deepseek-*` | `DEEPSEEK_API_KEY` (+ `DEEPSEEK_BASE_URL`) | สาย V4: `deepseek-v4-flash`, `deepseek-v4-pro` ส่วน alias เดิม `deepseek-chat` / `deepseek-reasoner` ยังใช้ได้ในระดับ wire |
| ThaiLLM (สวทช.) | `thaillm/*` | `THAILLM_API_KEY` | aggregator ที่ `thaillm.or.th` รวม model 8B ภาษาไทยสี่ตัว (OpenThaiGPT, Typhoon-S, Pathumma, THaLLE) มี alias (case-insensitive): `openthaigpt`, `typhoon`, `pathumma`, `thalle` |
| Z.ai | `zai/*` | `ZAI_API_KEY` (+ `ZAI_BASE_URL`) | Endpoint GLM Coding Plan ที่ `api.z.ai` ค่า default `zai/glm-5.2` ใช้ `ZAI_BASE_URL` override สำหรับ BigModel SKU ที่ `open.bigmodel.cn` |
| MiniMax | `minimax/*` | `MINIMAX_API_KEY` (+ `MINIMAX_BASE_URL`) | Endpoint สากลที่ `api.minimax.io` รุ่น: `minimax/MiniMax-M3` (flagship, default), `minimax/MiniMax-M1` (context 1M), `minimax/abab7-chat-preview` ผู้ใช้แพลตฟอร์มจีน (`api.minimax.chat`) ต้อง override `MINIMAX_BASE_URL` (auth scheme ต่างกัน — YMMV) เพิ่มใน v0.8.5 |
| Ollama Cloud | `ollama-cloud/*` | `OLLAMA_CLOUD_API_KEY` | Catalog Ollama แบบ hosted (Kimi, GPT-OSS, DeepSeek, Llama ฯลฯ) เป็น OpenAI-compatible ที่ `ollama.com/v1` |
| NVIDIA NIM | `nvidia/*` | `NVIDIA_API_KEY` (+ `NVIDIA_BASE_URL`) | NVIDIA hosted inference ที่ `integrate.api.nvidia.com/v1` ครอบคลุม Nemotron, Llama, DeepSeek, GLM และอื่น ๆ — prefix `nvidia/` route ทุกตัว ระบบจะตัด prefix ก่อนยิง wire ใช้ env var override สำหรับ on-prem NIM |
| LMStudio | `lmstudio/*` | — (local) | LMStudio server บน local ที่ `localhost:1234/v1` (OpenAI-compatible) ไม่ต้อง auth model ตามที่โหลดไว้ในแอป LMStudio |
| vLLM | `vllm/*` | — (self-hosted, + `VLLM_BASE_URL`) | `vllm serve` ที่ `localhost:8000/v1` ไม่ต้อง auth เว้นแต่สั่งด้วย `--api-key` ส่วนหลัง prefix คือ served id (มักเป็น path ของ HF เช่น `vllm/Qwen/Qwen3-8B`) `/models` จะดึงรายการที่เสิร์ฟอยู่จริงมาแสดง |
| llama.cpp | `llamacpp/*` | — (self-hosted, + `LLAMACPP_BASE_URL`) | `llama-server` ที่ `localhost:8080/v1` ไม่ต้อง auth เสิร์ฟ GGUF ได้ทีละไฟล์และไม่สนใจฟิลด์ `model` ในคำขอ ดังนั้นใส่ id อะไรหลัง prefix ก็ทำงาน |
| LiteLLM | `litellm/*` | `LITELLM_API_KEY` (ไม่บังคับ) (+ `LITELLM_BASE_URL`) | LiteLLM proxy ที่ self-host เองที่ `localhost:4000/v1` id คือ alias `model_name` ที่ตั้งไว้ใน `config.yaml` ของคุณเอง (`litellm/gpt-4o-mini`, `litellm/azure/prod-gpt4`) `/models` ดึงรายการสดมาแสดง และ `/model/info` ให้ context window จริง ส่วน key ต้องใส่เฉพาะตอนที่ proxy เปิด `master_key` หรือใช้ virtual key |
| Azure AI Foundry | `azure/<deployment>` | `AZURE_AI_FOUNDRY_API_KEY` (+ `AZURE_AI_FOUNDRY_ENDPOINT`) | Deployment Azure แบบ Anthropic-Messages-shaped ใส่ `<deployment>` ตามชื่อใน Azure ไม่มี default (ตั้งต่อ subscription) |

ค่าเริ่มต้นครั้งแรกคือ `claude-sonnet-4-6` เปลี่ยนได้ด้วย
`--model` ที่ command line หรือบันทึกลง `settings.json`

## การสลับ provider

```
❯ /providers
  * anthropic     → claude-sonnet-4-6
    anthropic-agent → agent/claude-sonnet-4-6
    openrouter    → openrouter/anthropic/claude-sonnet-4-6
    ...

❯ /provider openai
model → gpt-4o (provider: openai; saved to .thclaws/settings.json; new session sess-…)

❯ /provider
current provider: openai (model: gpt-4o)
```

### เมื่อไรจะ fork session และเมื่อไรจะคุย session เดิมต่อ

การสลับ model/provider จะตัดสินใจให้คุณอัตโนมัติว่าบทสนทนาจะถูกต่อ
หรือถูก fork เป็น session ใหม่ โดยดูจากว่า **provider family**
เปลี่ยนหรือไม่ (Anthropic, OpenAI, Gemini, Ollama, DashScope,
OpenRouter, Moonshot ฯลฯ):

| สลับจาก → ไป | พฤติกรรม | เหตุผล |
|---|---|---|
| `sonnet` → `opus` (Anthropic → Anthropic) | **ต่อบทสนทนาเดิม** session id เดิม ประวัติเดิมทั้งหมด | wire schema เหมือนกัน ข้อความ + tool call ส่งเข้าโมเดลใหม่ได้ตรง ๆ |
| `gemini-2.0-flash` → `gemini-2.5-flash` (Gemini → Gemini) | **ต่อบทสนทนาเดิม** | เช่นกัน — intra-family |
| `sonnet` → `gpt-4o` (Anthropic → OpenAI) | **fork session ใหม่** บันทึก session เก่าลงดิสก์ก่อน แล้วเริ่มใหม่ | provider แต่ละค่ายใช้ shape ของ message/tool-call ต่างกัน ถ้าส่งประวัติเดิมข้ามค่ายจะ error หรือได้ผลลัพธ์เพี้ยน |
| `/provider <name>` | **fork session ใหม่เสมอ** | การสลับ provider ถือเป็นการเปลี่ยน family โดยนิยาม |

ข้อความที่ thClaws แจ้งหลังสลับจะบอกชัดเจนว่าอยู่โหมดไหน:

```
# intra-family — ต่อบทสนทนาเดิม
model → claude-opus-4-6 (provider: anthropic; saved to .thclaws/settings.json; conversation preserved)

# cross-family — fork session
model → gpt-4o (provider: openai; saved to .thclaws/settings.json; new session sess-…)
```

session เก่าจะถูก save ลงดิสก์ก่อนเสมอ จึงสามารถ `/load <id>` หรือ
คลิกใน sidebar เพื่อกลับไปคุยต่อได้เมื่อสลับ provider กลับ

**การ load session ย้อนกลับ:** ถ้าคุณคลิก session เก่าใน sidebar ขณะ
อยู่คนละ provider กับของ session นั้น thClaws จะ **auto-switch
provider/model ให้ตรงกับ session** ก่อน replay แต่ถ้า provider ของ
session นั้นยังไม่ได้ตั้ง API key ไว้ (เช่น load Gemini session แต่ยังไม่มี
`GEMINI_API_KEY`) ระบบจะ **ปฏิเสธการ load** และขึ้น error แทน ไม่ใช่
โหลดแบบเพี้ยน ๆ แล้วรอ error ตอนส่ง prompt รอบต่อไป

## การสลับ model

`/model` รับได้ทั้ง model id เต็ม หรือ alias สั้น:

| Alias | resolve เป็น |
|---|---|
| `sonnet` | `claude-sonnet-4-6` |
| `opus`   | `claude-opus-4-6` |
| `haiku`  | `claude-haiku-4-5` |
| `flash`  | `gemini-2.5-flash` |
| `openthaigpt` | `thaillm/OpenThaiGPT-ThaiLLM-8B-Instruct-v7.2` |
| `typhoon` | `thaillm/Typhoon-S-ThaiLLM-8B-Instruct` |
| `pathumma` | `thaillm/Pathumma-ThaiLLM-qwen3-8b-think-3.0.0` |
| `thalle` | `thaillm/THaLLE-0.2-ThaiLLM-8B-fa` |

สี่ตัวท้ายคือโมเดล Thai LLM ของ NSTDA — พิมพ์ `/model typhoon` ง่ายกว่า
พิมพ์ id เต็มเยอะ

```
❯ /model sonnet
(alias 'sonnet' → 'claude-sonnet-4-6')
model → claude-sonnet-4-6 (provider: anthropic; saved to .thclaws/settings.json; conversation preserved)

❯ /models
  claude-haiku-4-5
  claude-opus-4-6
  claude-sonnet-4-6
  ...
```

`/model` จะ **ตรวจสอบ** ชื่อเทียบกับ `list_models` ก่อน commit
ถ้าพิมพ์ผิดอย่าง `/model gemma4-9999` model ปัจจุบันจะยังอยู่เหมือนเดิม
และระบบจะพิมพ์ว่า `unknown model '…' — try /models`

`/models` จะแสดง catalogue ที่ server รายงานมาสำหรับ provider
ปัจจุบัน สำหรับ provider ที่มี prefix (Ollama, Moonshot, Groq ฯลฯ) ID จะถูกใส่ prefix กลับมาให้ด้วย
(เช่น `ollama/llama3.2`, `moonshot/kimi-k2.6`) เพื่อให้คุณ paste เข้า
`/model` ได้ทันที

## Model catalogue — ขนาด context ของแต่ละโมเดล

thClaws เก็บตาราง **context window** ของแต่ละโมเดลไว้ในตัว เพื่อให้
compaction / fork / threshold อื่น ๆ อ้างอิงขนาดจริงของโมเดลที่ใช้อยู่
(เช่น Claude Sonnet 4.6 = 200k tokens, Gemini 2.5 Pro = 2M, GPT-4o =
128k, Qwen Max = 32k) ไม่ได้ใช้เลขสมมติตายตัวเดียวกันทุก provider

### สามชั้นของ lookup (ตามลำดับความสำคัญ)

1. **User cache** — `~/.config/thclaws/model_catalogue.json` เขียน
   โดย `/models refresh` และ auto-refresh รายวัน
2. **Embedded baseline** — ตารางฝังมากับ binary ชั้นสำรองเวลา cache ไม่มี
   หรือออฟไลน์
3. **Provider default + global fallback** — ถ้าไม่เจอ model เลย จะใช้
   ขนาดกลาง ๆ ของ provider นั้น (เช่น 200k สำหรับ Anthropic) และ
   fallback รวม 128k เป็นพื้น

### `/models refresh` — อัปเดตตารางเอง

```
❯ /models refresh
refreshing model catalogue…
catalogue refreshed: 352 models (source: openrouter + thclaws 2026-04-24)
```

ถ้ายังไม่มีอินเทอร์เน็ต หรือ endpoint ล่ม ข้อความจะบอกตรง ๆ ว่าล้มเหลว
และ cache เดิมจะไม่ถูกแตะ

### สำหรับ contributor — `make catalogue`

ถ้าคุณ build จาก source แล้วอยากอัปเดต `model_catalogue.json` (ไฟล์ที่
compile-in baseline ไปกับ binary) ใช้ Makefile target ที่ root ของ workspace:

```sh
make catalogue
```

จะดึง model list จาก OpenRouter (เสมอ ไม่ต้องมี key) + ของ
Anthropic / OpenAI / Gemini ถ้า env var key ที่ตรงกันถูกตั้งไว้ + Ollama
ถ้าเข้าถึงได้ที่ `localhost:11434` จากนั้น merge เข้า catalogue โดย
**ไม่ทับ** rows ที่ hand-curated (insert-only) แล้ว print `git diff
--stat` ให้ตรวจสอบก่อน commit รายการ id ใหม่จะถูกแสดงในรายงาน
พร้อมจำนวน unchanged + skipped (no context) per provider เพื่อให้
ตอบคำถามแบบ "ทำไมโมเดล X ไม่มาในรายการใหม่" ได้จากรายงานเอง

### Auto-refresh รายวัน

เมื่อเปิด thClaws ใหม่ ถ้า cache มีอายุเกิน 24 ชั่วโมง (หรือยังไม่เคยมี) จะมี
task เบื้องหลังไปโหลด catalogue ใหม่ครั้งเดียวโดยไม่บอกอะไร (เงียบโดย
ออกแบบ) ถ้าสำเร็จ cache จะถูกเขียน; ถ้าไม่สำเร็จจะข้ามไปเฉย ๆ และ
session ยังทำงานต่อได้ปกติด้วย cache / baseline เดิม

### ถ้าเลือกโมเดลที่ catalogue ไม่รู้จัก

```
model → claude-future-x99 (provider: anthropic; …)
⚠ no catalogue entry for 'claude-future-x99' — using anthropic (200000 tokens). Run /models refresh to pick up newer entries.
```

threshold ของ auto-compact ยังใช้ได้ (fall back ไป provider default)
แต่จะแม่นยำขึ้นเมื่อ refresh แล้วมี entry จริงของโมเดลใหม่

## ลำดับชั้นของ API key

**Key จะไม่ถูกเก็บใน `settings.json` เด็ดขาด** thClaws จะมองหาจาก
สี่ที่ โดยที่มีลำดับความสำคัญสูงสุดจะชนะ

| ระดับ | ที่อยู่ | ขอบเขต |
|---|---|---|
| Shell export | `~/.zshrc`, env ของ CI ฯลฯ | ทุก process |
| OS keychain | macOS Keychain / Windows Credential Manager / Linux Secret Service | ทุก session ของ thClaws บนเครื่องนี้ |
| user `.env` | `~/.config/thclaws/.env` | ทุก session ของ thClaws |
| project `.env` | `./.env` ใน working directory | เฉพาะโปรเจกต์นี้ |

**แนะนำ**: ใช้ **Settings modal (GUI)** ซึ่งจะบันทึก key ลงใน
OS keychain ให้ ปลอดภัยกว่าเส้นทาง `.env` แบบไหน ๆ ชัดเจน

| | OS keychain (ผ่าน Settings modal) | ไฟล์ `.env` |
|---|---|---|
| เข้ารหัสขณะพัก (at-rest) | ✓ ได้มาจากรหัสผ่าน login ของคุณ (Secure Enclave บน Mac รุ่นใหม่) | ✗ plaintext |
| การควบคุมการเข้าถึง | ✓ ผูกกับบัญชีผู้ใช้ของคุณ | ✗ process ใดก็ตามที่อ่าน filesystem ได้ |
| commit เข้า git โดยไม่ตั้งใจ | ✓ เป็นไปไม่ได้ (ไม่ใช่ไฟล์ใน repo) | ⚠ เกิดง่าย (คนลืม `.gitignore`) |
| รั่วผ่าน Time Machine / cloud sync / rsync | ✓ ไม่ | ⚠ รั่ว — ไฟล์ไปที่ไหน backup ก็ไปที่นั่น |
| ใช้ได้ใน headless / CI | ✗ Linux แบบ headless ส่วนใหญ่ไม่มี Secret Service | ✓ ใช้ได้ |

สรุปคือ: ใช้ Settings modal บน laptop หรือ workstation ของคุณ แล้ว
ค่อย fallback ไปใช้ `.env` เฉพาะเมื่ออยู่ในสภาพแวดล้อมที่ไม่มี keychain
(เช่น CI runner, Docker image ขั้นต่ำ หรือ server แบบ headless)

## ตัวเลือก backend สำหรับ secret {#secrets-backend-chooser}

**ครั้งแรกที่เปิด thClaws** ทันทีหลังจากเลือก working directory
(บทที่ 3) จะมี dialog เด้งถามว่าอยากเก็บ secret แบบไหน
dialog นี้จะขึ้นก่อนที่ thClaws จะแตะ OS keychain ด้วยซ้ำ ถ้า
เลือก `.env` จะไม่มี prompt จาก keychain เด้งขึ้นมาเลยสักครั้ง

![thClaws ควรเก็บ API key ที่ไหน? — OS keychain (แนะนำ) vs ไฟล์ .env](../user-manual-img/ch-03/secrets-backend-chooser.png)

มีสองทางเลือก:

- **OS keychain (แนะนำ)** — macOS Keychain / Windows Credential
  Manager / Linux Secret Service เข้ารหัสขณะพัก และผูกไว้กับบัญชี
  ผู้ใช้ของคุณ ครั้งแรกที่ thClaws อ่าน key คุณจะเจอ prompt ขอสิทธิ์
  จาก OS ครั้งเดียว คลิก "Always Allow" หลังจากนั้นจะเงียบไปเลย
- **ไฟล์ `.env`** — เก็บเป็น plain-text อยู่ที่ `~/.config/thclaws/.env`
  ไม่มี prompt จาก keychain มารบกวน เหมาะกับเครื่อง Linux แบบ
  headless ที่ไม่มี Secret Service แต่แลกมาด้วยความเสี่ยงที่ใครก็ตาม
  ซึ่งเข้าถึง home directory ของคุณได้ จะอ่านไฟล์นี้ได้ด้วย จึงควรปฏิบัติ
  กับมันเหมือนไฟล์ลับอื่น ๆ

ตัวเลือกของคุณจะถูกบันทึกลง `~/.config/thclaws/secrets.json` และ
ใช้ต่อไปตลอด หากภายหลังเปลี่ยนใจก็ทำได้: Settings → Provider API
keys → ลิงก์ "Change…" ที่หัวของ modal จะเปิดตัวเลือกขึ้นมาอีกครั้ง

### bundle keychain แบบ entry เดียว (prompt เดียวต่อการเปิดใช้งาน)

เมื่อเลือก backend แบบ keychain key ของทุก provider จะถูกเก็บไว้ใน
keychain item **เดียว** (service `thclaws`, account `api-keys` เก็บเป็น JSON map
`{"anthropic": "sk-ant-…", "openai": "sk-…", …}`) เรื่องนี้สำคัญเพราะ macOS
Keychain ACL เป็นแบบต่อ item ถ้ามี N item แยกกัน คุณจะโดน N prompt ทุกครั้งที่
เปิด binary ที่ rebuild ใหม่ แต่พอรวมเป็น bundle เดียว ก็จะเห็น prompt
**ครั้งเดียว** คลิก "Always Allow" แล้วการเปิดครั้งต่อ ๆ ไปของ
binary ที่เซ็นแล้วจะเงียบไปเลย

การ migrate ทำให้อัตโนมัติ — ครั้งแรกที่ thClaws อ่าน bundle entry เก่าที่เคยแยก
ตาม provider จะถูกดึงเข้ามาใน bundle แล้วเขียน bundle กลับลงไปให้

### การมองเห็น key ข้าม process

Desktop GUI และ PTY-child REPL เป็น OS process คนละตัวกัน เมื่อคุณบันทึก key
ใน Settings ตัว GUI จะตั้ง env var ให้ *ตัวมันเอง* เท่านั้น แต่ REPL ลูกที่รันอยู่แล้ว
จะมองไม่เห็นการเปลี่ยน env ของ GUI เพื่อให้ทั้งคู่ sync กัน ทุกคำขอจึงอ่าน
keychain แบบ live ถ้าไม่มี env var อยู่ — ดังนั้น key ที่บันทึกใน Settings
จะใช้งานได้ทันทีใน REPL ของแท็บ Terminal

### auto-switch ตอนบันทึก key {#auto-switch-on-key-save}

เคสที่น่าสนใจ: สมมติคุณบันทึก key ของ Anthropic แต่ `config.model` ยังเป็น
`gpt-4o` (OpenAI) อยู่ ถ้าไม่มี auto-switch คุณก็จะยังเห็นตัวบอก "no API key"
สีแดงค้างอยู่

thClaws จัดการเรื่องนี้ให้: ทันทีหลังจากบันทึก key สำเร็จ หาก provider ของ
model ที่ตั้งไว้ตอนนี้ยังไม่มี credential ตัว model ที่ใช้งานอยู่จะถูกเขียนทับ
ให้เป็นค่าเริ่มต้นของ provider ที่เพิ่งใช้ได้ (Anthropic → `claude-sonnet-4-6`,
OpenAI → `gpt-4o` ฯลฯ) sidebar จะเปลี่ยนเป็นสีเขียวภายในหนึ่งวินาที และ
chat รอบถัดไปก็ใช้งานได้เลย

### env var สำหรับวินิจฉัย

| Env var | ผล |
|---|---|
| `THCLAWS_DISABLE_KEYCHAIN=1` | ข้าม keychain ไปเลย เหมาะสำหรับทดสอบหรือวินิจฉัยอาการ flaky |
| `THCLAWS_KEYCHAIN_TRACE=1` | พิมพ์บรรทัด diagnostic สีม่วงทุกครั้งที่มีการเรียก keychain พร้อมแสดง process ID และ flag "already loaded" |
| `THCLAWS_KEYCHAIN_LOADED=1` | GUI ตั้งให้อัตโนมัติหลังอ่าน keychain ครั้งแรก เพื่อให้ PTY ลูกที่ถูก spawn ขึ้นมาข้ามการ walk ของตัวเองไป โดยทั่วไปคุณไม่ต้องแตะตัวนี้ |

## Settings modal (GUI)

คลิกไอคอนเฟืองที่แถบสถานะด้านล่าง การ์ดของ provider แต่ละตัวจะแสดง

- ช่อง **API Key** — กรอกไว้ล่วงหน้าด้วย `*****` (ดอกจันจำนวนเท่า
  ความยาวของ key ที่เก็บไว้ สูงสุด 64 ตัว) เมื่อพิมพ์อะไรลงไป sentinel
  จะถูกแทนที่ และช่องจะเปลี่ยนจาก plain text เป็น masked ส่วนปุ่ม
  Save จะถูก disable ไว้จนกว่าคุณจะพิมพ์ค่าใหม่จริง ๆ
- ช่อง **Base URL** (เฉพาะ Ollama) — กรอกไว้ล่วงหน้าด้วยค่าปัจจุบัน
  หรือ placeholder ค่าเริ่มต้น เก็บไว้ใน `~/.config/thclaws/endpoints.json`

DashScope ถูกล็อกไว้ที่ค่าเริ่มต้นใน Settings UI แต่หากจำเป็นก็สามารถ
ชี้ไป endpoint ระดับภูมิภาคได้ด้วย env var `DASHSCOPE_BASE_URL`
(เช่น URL ของ Alibaba Cloud International)

ล้าง key ได้ด้วยไอคอนถังขยะ entry ใน keychain จะถูกลบ และ
env var จะถูก unset สำหรับ session ที่กำลังรันอยู่

![thClaws setting LLM Keys](../user-manual-img/ch-05/thClaws-setting-llm-keys.png)

## ไฟล์ `.env` (CI, headless, quick-start)

เมื่อ keychain ใช้ไม่ได้ — เช่น CI runner, Linux แบบ headless
ที่ไม่มี Secret Service หรือเวลาต้องการ key ที่ tool แบบ CLI อย่างเดียว
(script หรือ `thclaws -p` ใน pipeline) อ่านได้ — เส้นทาง `.env`
แบบคลาสสิกก็ยังใช้ได้เช่นกัน

```bash
# ~/.config/thclaws/.env
ANTHROPIC_API_KEY=sk-ant-...
OPENAI_API_KEY=sk-...
OPENROUTER_API_KEY=sk-or-v1-...
GEMINI_API_KEY=AI...
DASHSCOPE_API_KEY=sk-...
OLLAMA_BASE_URL=http://localhost:11434   # defaults to this anyway
OPENAI_COMPAT_BASE_URL=http://localhost:8000/v1   # gateway OAI-compat ใดๆ
OPENAI_COMPAT_API_KEY=...
```

> ⚠️ **ถ้าคุณใช้ git ให้ใส่ `.env` ลงใน `.gitignore` ทันที** — ก่อน
> จะ paste key ใด ๆ เข้าไป ไฟล์ `.env` ที่ถูก commit ขึ้น repo สาธารณะ
> (หรือแม้แต่ repo ส่วนตัวที่แชร์กัน) คือสาเหตุที่พบบ่อยที่สุดของการที่
> API key รั่ว โดย `./.env` แบบ project-scope นั้นเสี่ยงเป็นพิเศษ
> เพราะอยู่ใน root ของ repo ส่วน `~/.config/thclaws/.env` แบบ
> user-scope นั้นอยู่นอก repo จึงปลอดภัยในแง่นี้ แต่ก็ยังควรปฏิบัติ
> กับมันเหมือนเป็นไฟล์ลับอยู่ดี
>
> วิธีแก้บรรทัดเดียว:
>
> ```bash
> $ echo ".env" >> .gitignore && git add .gitignore
> ```
>
> และถ้าเผลอ commit ไปแล้ว ให้ rotate key ทันทีที่ dashboard ของ
> provider เพราะประวัติ git จะเก็บไฟล์ที่ลบไปไว้ตลอดกาล การ rewrite
> ประวัติก็ยุ่งยาก แถมใครก็ตามที่ clone ไปก่อนที่คุณจะรู้ตัวก็มี key
> ของคุณติดมือไปแล้ว

## ระดับ thinking — ปุ่มเดียวใช้ได้ทุก provider

แต่ละ provider เรียก "ให้โมเดลคิดหนักแค่ไหน" คนละแบบ: Anthropic
รับเป็น token budget, OpenAI o-series/gpt-5 รับ `reasoning_effort`,
DeepSeek กับ GLM เป็นสวิตช์เปิด/ปิด, Qwen มี `enable_thinking` + budget,
Gemini 2.5 รับ budget ส่วน Gemini 3 รับ `thinkingLevel`, Ollama รับ
`think: true|false` thClaws รวมทั้งหมดเป็นระดับเดียวที่ตั้งครั้งเดียว:

| ระดับ | ความหมาย | Anthropic | OpenAI o*/gpt-5 | DeepSeek / GLM | Qwen (hybrid) | Gemini 2.5 flash / pro | Gemini 3 | Ollama |
|---|---|---|---|---|---|---|---|---|
| `0` off | เร็วสุด ไม่คิด | ไม่ส่ง thinking block | `minimal` / `low` | `disabled` | ปิด | budget 0 / 128 | `low` | `think: false` |
| `1` low | คิดสั้น ๆ | 2,048 tokens | `low` | enabled | เปิด, 2,048 | 2,048 | `low` | `think: true` |
| `2` medium | สมดุล | 10,000 | `medium` | enabled | เปิด, 10,000 | 10,000 | `high` | `think: true` |
| `3` high | คิดลึกสุด | 32,000 | `high` | enabled | เปิด, 32,000 | 24,576 / 32,000 | `high` | `think: true` |
| `auto` | ค่า default ของ provider — ไม่ส่ง field ใดเลย | | | | | | | |

ตั้งได้สองทาง:

- **Sidebar** — pill `think auto 0 1 2 3` ใต้ชื่อโมเดล
- **`/thinking 0`** … `/thinking 3`, `/thinking auto` หรือระบุ budget ตรง ๆ
  เช่น `/thinking 12000` สำหรับ provider แบบ Anthropic

ทั้งสองทางบันทึกลง `thinkingBudget` ใน `.thclaws/settings.json`
ระดับจึงติดกับ workspace ไม่ใช่ session โมเดลที่ไม่มีปุ่มให้ปรับ
(gpt-4o, Qwen-Max, โมเดล local ส่วนใหญ่) จะไม่ถูกส่ง field เพิ่ม
และทำงานเหมือนเดิม — thClaws ไม่ส่งระดับไปให้โมเดลที่จะ reject

โมเดล frontier ยังเลือกเองว่าจะใช้ budget เท่าไร: บน Anthropic และ
Gemini ค่า budget เป็นเพดาน โมเดลจะใช้น้อยลงเองเมื่อคำถามง่าย ค่าที่
เปลี่ยนพฤติกรรมชัดที่สุดคือ `off` — บน DeepSeek v4 คือความต่างระหว่าง
~90 วินาที กับ ~4 วินาที สำหรับ prompt สกัดข้อมูลยาว ๆ นี่คือเหตุผลที่
`/research` รัน worker call (digest, แผน, note) ที่ระดับ `off` เสมอ
ไม่ว่าคุณตั้งค่าแชทไว้อย่างไร

## โมเดลแบบ reasoning / thinking

โมเดลในกลุ่มต่อไปนี้ส่ง field `reasoning_content` (chain-of-thought) ออก
มาคู่กับ `content` ปกติ และ provider บังคับให้เราต้อง **ส่งกลับ
`reasoning_content` เก่า** ไปด้วยทุกครั้งที่ต่อบทสนทนา (turn ถัด ๆ
ไป) — ไม่อย่างนั้น API จะตอบ HTTP 400 ว่า `"The reasoning_content in the
thinking mode must be passed back to the API"`

thClaws จัดการให้อัตโนมัติ — เก็บ reasoning ไว้บน assistant message
แล้วส่งกลับเฉพาะกับ provider ที่ต้องการ ไม่บอกใครเสริม:

| ตระกูล | model id (ตัวอย่าง) | provider |
|---|---|---|
| DeepSeek v4 | `deepseek-v4-flash`, `deepseek-v4-pro` (native) · `deepseek/deepseek-v4-*` (OpenRouter) | DeepSeek, OpenRouter |
| DeepSeek r1 | `deepseek-reasoner` (native) · `deepseek/deepseek-r1`, `deepseek-r1` | DeepSeek, OpenRouter |
| OpenAI o-series | `openai/o1-mini`, `openai/o3`, `openai/o4-*` | OpenRouter |

โมเดลอื่น ๆ ที่ไม่อยู่ในกลุ่มนี้ (เช่น `gpt-4o`, `claude-sonnet-4-6`,
`qwen3.6-plus`) — `reasoning_content` block จะถูก **ตัดออก**
ระหว่าง serialize เพื่อไม่ให้กิน input token เพิ่มและไม่เสี่ยงโดน
provider reject เพราะ field แปลก ๆ

ถ้าคุณสลับจากโมเดล thinking ไปโมเดลปกติกลาง session, reasoning
block ของ turn ก่อนจะคงอยู่ในไฟล์ session แต่ไม่ถูกส่งบนสาย ไม่มี
token leak

## ใช้ Ollama ในเครื่อง

1. ติดตั้ง Ollama: `brew install ollama` (macOS) หรือดูที่ ollama.com
2. pull model: `ollama pull gemma4:26b`
3. บอก thClaws: `/model ollama/gemma4:26b`

ไม่ต้องใช้ API key หากใช้ Ollama server ระยะไกล ให้ตั้ง `OLLAMA_BASE_URL`
(ผ่าน Settings modal หรือ env var)


## ใช้ OpenRouter (model 300+ ผ่าน key เดียว)

OpenRouter คือ gateway รวมที่เข้าถึงผู้ให้บริการ LLM รายใหญ่ได้ทุกเจ้า
(ทั้ง Anthropic, OpenAI, Google, Meta, Mistral, xAI, DeepSeek, Alibaba
และอื่น ๆ อีกมาก) ใช้ API key เดียวแต่เข้าถึง model ได้กว่า 300 ตัว

1. ขอ key จาก [openrouter.ai/keys](https://openrouter.ai/keys)
2. Paste ลงใน Settings → API Keys (OpenRouter) — หรือตั้ง
   `OPENROUTER_API_KEY`
3. เลือก model: `/model openrouter/anthropic/claude-sonnet-4-6` (หรือ
   ตัวใดก็ได้จากหลายร้อยที่ `/models` แสดง)

Model ID มีรูปแบบ `openrouter/<vendor>/<model>` — copy ได้จาก
[openrouter.ai/models](https://openrouter.ai/models) หรือ paste string
ตรง ๆ ตามที่เห็นใน output ของ `/models`

เหมาะสำหรับ

- เปรียบเทียบคำตอบข้าม vendor โดยไม่ต้องไปสมัครทีละเจ้า
- ทดลอง model ใหม่โดยไม่ต้องเปิดบัญชีแยก
- มีรายการจ่ายเงินเพียงรายการเดียว สำหรับการใช้งานแบบ hobby หรือทีมเล็ก

หมายเหตุ: OpenRouter จะบวก markup เล็กน้อยทับต้นทุนของ vendor แต่ละเจ้า
สำหรับงาน production ที่ volume สูง ใช้ตรงกับ provider ต้นทางจะคุ้มกว่า

### Fusion Router — ปรึกษาหลายโมเดล (v0.61.0+)

Fusion router ของ OpenRouter คือ model id เดียวที่เบื้องหลังจะกระจาย
คำถามของคุณไปยัง **คณะโมเดล (panel)** สูงสุด 8 ตัวพร้อมกันแบบขนาน
(แต่ละตัวค้น/ดึงเว็บได้) จากนั้น **โมเดลกรรมการ (judge)** จะเอาคำตอบ
ทั้งหมดมาเปรียบเทียบ — ตรงกันตรงไหน (consensus), ขัดแย้งตรงไหน, ตกหล่น
อะไร, มุมที่ไม่เหมือนใคร, จุดบอด — แล้วสังเคราะห์เป็นคำตอบสุดท้าย พูด
ง่าย ๆ คือ "ระดมความเห็นหลายหัว + มีกรรมการตัดสิน" ยัดมาในรูป model เดียว

ใช้ใน thClaws ได้สองแบบ:

- **`openrouter/fusion`** — panel เริ่มต้น (Claude Opus + GPT + Gemini)
  ไม่ต้องตั้งค่าอะไร แค่ `/model openrouter/fusion`
- **`openrouter/fusion+`** — แบบ **ปรับแต่งได้** เมื่อเลือกใน model
  picker จะเปิดหน้าตั้งค่าให้ปรับการปรึกษา:

  | ฟิลด์ | ความหมาย |
  |---|---|
  | **Analysis models** | คณะโมเดล (panel) — model id ของ OpenRouter 1–8 ตัว (เช่น `anthropic/claude-opus-4.8`) เว้นว่างไว้เพื่อใช้ panel เริ่มต้นของ Fusion |
  | **Judge model** | โมเดลกรรมการที่สังเคราะห์คำตอบสุดท้าย เว้นว่าง = ใช้ outer model |
  | **Outer model** | โมเดลที่ thClaws เรียกจริง (id แบบ thClaws เช่น `openrouter/openai/gpt-4.1`) และเป็นกรรมการเริ่มต้นด้วย |
  | **Max tool calls** | จำนวนรอบ web-search/fetch ต่อโมเดล (1–16, ค่าเริ่มต้น 8) |
  | **Max output tokens** | จำกัด token ต่อการเรียกย่อยแต่ละครั้ง |
  | **Temperature** | ส่งต่อให้ panel + judge (0–2) |
  | **Reasoning effort** | ส่งต่อให้ panel + judge |
  | **Tool choice** | `auto` (Fusion ถูกเรียกเมื่อจำเป็น และอยู่ร่วมกับ tool ของ agent ได้) หรือ `required` (บังคับให้ปรึกษาทุกครั้ง) |

  ค่าที่ตั้งจะถูกบันทึกใน `.thclaws/settings.json` ใต้คีย์
  `openrouterFusion` จึงมีผลตอนรันแบบ headless หรือ `--serve` ด้วย
  ไม่เฉพาะใน GUI

Fusion คิดเงินตาม panel + judge ที่รันจริง หนึ่งเทิร์นจึงแพงกว่าการเรียก
โมเดลเดียว เหมาะเป็นตัวแทนเมื่อโมเดลที่อยากใช้ไม่มีให้บริการบนแพลตฟอร์ม

## ใช้ endpoint OpenAI-compatible แบบ generic (`oai/*`)

provider `OpenAICompat` คือ slot configurable เดียวสำหรับ
**บริการใดก็ได้ที่พูด `/v1/chat/completions` ของ OpenAI ด้วย Bearer
token** เป้าหมายที่พบบ่อย:

- LLM gateway: LiteLLM, Portkey, Helicone, proxy ภายในองค์กรที่
  รวบบิลของหลาย vendor และบังคับ policy ระดับองค์กร
- self-hosted inference: vLLM, text-generation-inference, lm-deploy,
  binary `server` ของ llama.cpp ในโหมด OpenAI-compat, MLX-LM ฯลฯ
- aggregator service อื่น ๆ นอกจาก OpenRouter ที่ใช้ shape เดียวกัน
  แต่อยู่บน URL ส่วนตัว

ตั้ง env var สองตัว (หรือ Settings modal card ที่ตรงกัน):

```sh
OPENAI_COMPAT_BASE_URL=http://localhost:8000/v1
OPENAI_COMPAT_API_KEY=...
```

แล้วเลือก model:

```
/model oai/<upstream-model-id>
```

prefix `oai/` ถูก **strip** ก่อนส่ง request ไปยัง upstream — ส่ง
model id อะไรก็ได้ที่ gateway รับ ตัวอย่าง:

- `/model oai/gpt-4o-mini` → wire payload `model: "gpt-4o-mini"`
- `/model oai/meta-llama/Llama-3.1-70B-Instruct` → wire payload
  `model: "meta-llama/Llama-3.1-70B-Instruct"`
- `/model oai/anthropic/claude-sonnet-4-6` → wire payload
  `model: "anthropic/claude-sonnet-4-6"`

แยกจาก `ProviderKind::OpenAI` โดยตั้งใจ จะได้ใช้ OpenAI ตรง ๆ
(`OPENAI_API_KEY` + model `gpt-*` / `o*`) ได้ตามปกติ ไม่กระทบกัน
ทั้งสองตัวอยู่ร่วมกันได้ — ตั้ง env var ทั้งสองชุด แล้วสลับด้วย
`/model gpt-4o` (OpenAI ตรง) หรือ `/model oai/<id>` (gateway ของคุณ)

Base URL รับได้สองรูปแบบ:

- ลงท้าย `/v1` — ระบบจะต่อ `/chat/completions` ให้เอง
- ลงท้าย `/v1/chat/completions` — ใช้ตรง ๆ

Auth เป็น header `Authorization: Bearer $OPENAI_COMPAT_API_KEY`
มาตรฐาน gateway ที่ใช้ auth แบบอื่น (custom header, mTLS ฯลฯ)
อยู่นอกขอบเขต — เปิด issue หรือใช้ org-policy `gateway` route
ของ EE Phase 3 แทน

ถ้า endpoint ของคุณ implement `/v1/models` ด้วย คำสั่ง
`/models refresh` จะดึง catalogue มาให้อัตโนมัติ ถ้าไม่มี
endpoint นั้น refresh จะ fail เงียบ ๆ และ chat ยังทำงานต่อได้ปกติ

## ใช้ LiteLLM proxy ที่ self-host เอง (`litellm/*`)

LiteLLM มีช่องของตัวเองแทนที่จะไปเบียด `oai/` จึงเก็บ base URL และ key
แยกจาก endpoint อื่นที่คุณชี้ `oai/` ไว้ได้:

```sh
LITELLM_BASE_URL=http://localhost:4000/v1   # ค่า default แก้ใน Settings ได้เช่นกัน
LITELLM_API_KEY=sk-...                      # ใส่เฉพาะตอน proxy ตั้ง master_key
```

id ของ model คือ alias `model_name` ใน `config.yaml` ของคุณเอง โดย prefix
`litellm/` จะถูกตัดออกก่อนส่งขึ้น upstream:

```yaml
model_list:
  - model_name: gpt-4o-mini            # → /model litellm/gpt-4o-mini
    litellm_params: { model: openai/gpt-4o-mini }
  - model_name: azure/prod-gpt4        # → /model litellm/azure/prod-gpt4
    litellm_params: { model: azure/my-deployment }
```

เพราะ alias เหล่านี้เป็นของคุณเอง catalogue ที่ ship มากับตัวโปรแกรมจึง
ไม่มีทางรู้จัก — `/models` จะยิงถาม `/models` ของ proxy สด ๆ แทน และตอน
สลับ model จะอ่าน context window จริงจาก `/model/info` ของ LiteLLM
(ลองทั้งที่ root และใต้ `/v1`) แล้ว cache ไว้

Auth ไม่บังคับ: proxy ที่รันโดยไม่ตั้ง `master_key` รับ bearer อะไรก็ได้
thClaws จึงส่งค่า placeholder ไปแทนการปฏิเสธที่จะเริ่มทำงาน

ข้อควรระวัง — LiteLLM เป็น **router** ไม่ใช่ local inference ถึงคุณจะ
host เอง แต่ model ปลายทางมักอยู่ที่ OpenAI / Anthropic / Azure thClaws
จึงถือว่า traffic ออกนอกเครื่อง: การ mask PII ภาษาไทยยังทำงานอยู่ (ต่างจาก
`ollama/*`, `vllm/*` หรือ `llamacpp/*`) และ org-policy gateway จะไม่ยกเว้น
ให้แบบ local model

## ใช้ Codex ผ่าน subscription ChatGPT (`chatgpt-codex/*`)

รัน Codex model กับ `chatgpt.com/backend-api/codex/responses` โดย
**คิดเงินจาก subscription ChatGPT Plus / Pro / Team** แทนการใช้
OpenAI API key แบบเสียเงิน เป็น wire path เดียวกับที่ Codex CLI
ตัวทางการใช้

การตั้งค่าทำครั้งเดียว และ thClaws จะอาศัย auth ของ Codex CLI
ตัวทางการ:

1. ติดตั้ง Codex CLI (`npm i -g @openai/codex-cli` หรือทำตามเอกสาร
   ของเขา)
2. รัน `codex login` หนึ่งครั้ง — จะเปิด browser ให้คุณ sign in เข้า
   บัญชี ChatGPT แล้ว CLI จะเก็บ token ไว้ที่
   `~/.codex/auth.json`
3. ใน thClaws: `/model chatgpt-codex/gpt-5.4` (หรือ Codex model ตัว
   อื่นตาม tier ของ subscription คุณ)

thClaws จะ auto-import ไฟล์ auth ให้ตอนใช้งานครั้งแรก — ไม่ต้อง
login ฝั่ง thClaws แยกอีก เมื่อ access token หมดอายุ ให้รัน `codex
login` ใหม่ thClaws จะหยิบไฟล์ที่ refresh แล้วมาใช้ในการเรียกครั้งถัดไป

ข้อควรระวัง:

- endpoint นี้ไม่มีเอกสาร OpenAI อาจเปลี่ยน wire shape ได้โดยไม่แจ้ง
  ล่วงหน้า ถ้าเจอ 400 พร้อมชื่อ field ที่ไม่คาดคิด ลองเช็ค
  [thclaws issues](https://github.com/thClaws/thClaws/issues) ก่อน
  ลงมือ debug
- rate limit ของ subscription มีผล (โดยทั่วไปใจกว้างกว่า free API
  tier มาก แต่ก็ยังมีขีดจำกัด — automation หนัก ๆ อาจชนเพดานได้)
- การ refresh token ยังไม่ถูกทำให้อัตโนมัติใน thClaws (ให้รัน `codex
  login` ใหม่เมื่อเจอ error เรื่อง auth)

เพิ่มใน v0.9.5 ผ่าน PR #88 เครดิต: port มาจาก `client_codex.rs`
ของ themion

## Sign in เข้า thClaws Cloud — ตัวเลือกเสริม

มุมขวาบนของ navbar มีปุ่ม **Sign in** ตั้งแต่ v0.9.6 เป็นต้นไป
dropdown มี IdP ให้เลือกสองตัว:

- **Sign in with Google** — สำหรับบัญชี Google / Workspace ส่วนตัว
- **Sign in with Microsoft** — สำหรับบัญชี Microsoft 365 / Azure Entra
  (multi-tenant — Entra org ไหนก็ใช้ได้โดยไม่ต้องลงทะเบียนต่อ tenant)
  บัญชี Microsoft ส่วนตัว (Outlook, Hotmail) ก็ใช้ได้ผ่าน endpoint
  `/common` เดียวกัน

ทั้งสองทางจะ authenticate คุณกับ `gateway.thclaws.ai` และปลดล็อก
ฟีเจอร์ cloud-gateway (proxy ต่อ provider, shared credit pool — ดูที่
หัวข้อ **thClaws Gateway** ด้านล่าง)

**สำคัญ:** thClaws ใช้งานได้เต็มที่โดยไม่ต้อง sign in ปุ่มนี้เป็น
opt-in ถ้าไม่สนใจก็ไม่มีอะไรพัง

### จาก source — ชี้ไป OAuth project ของคุณเอง

ใส่คู่ `*_CLIENT_ID` / `*_CLIENT_SECRET` ที่ตรงกันลงใน `.env`
(หรือ environment ของ workspace) ก่อนเปิดใช้งาน:

```sh
# Google (web/native app — ต้องมีทั้ง ID และ secret)
GOOGLE_CLIENT_ID=...apps.googleusercontent.com
GOOGLE_CLIENT_SECRET=GOCSPX-...

# Microsoft Entra (public/native client — PKCE-only, ไม่มี secret)
AZURE_CLIENT_ID=00000000-0000-0000-0000-000000000000
```

Azure เป็น client แบบ **public** และรัน PKCE โดยไม่ต้องมี secret
การ register Entra app ต้องตั้ง "Allow public client flows = Yes"
และใส่ `http://localhost` เป็น redirect URI (ไม่ระบุ port — Entra
จับ ephemeral port ตัวไหนก็ได้) คู่มือฉบับ operator ครบ ๆ อยู่ที่
[`docs/azure-setting.md`](../docs/azure-setting.md)

คลิกปุ่ม → browser เปิด → หน้า consent → desktop รับ callback →
ปุ่มจะเปลี่ยนเป็น email ของคุณพร้อมเครื่องหมายถูก token จะถูกเก็บลง
OS keychain (macOS Keychain / Windows Credential Manager / Linux
Secret Service) **ไม่ใช่ใน `.env`** — แม้ว่าคุณจะเลือก backend แบบ
dotenv สำหรับเก็บ API key ก็ตาม

### จาก dmg / msi ตัวทางการ

OAuth credential ที่ bundle มาถูกฝังเข้าไปใน build ตัวทางการผ่านการ
inject secret ตอน CI (`BUNDLED_GOOGLE_CLIENT_ID`,
`BUNDLED_GOOGLE_CLIENT_SECRET`, `BUNDLED_AZURE_CLIENT_ID` ถูกอ่าน
ตอน compile) ปุ่ม Sign-in ทำงานได้ทันที — ไม่ต้องตั้งค่า `.env` เอง

### ไม่มี prompt จาก keychain ตอนเปิดครั้งแรก

การอ่าน OS keychain จะ trigger prompt ขอสิทธิ์เข้าถึงครั้งแรกที่
binary ที่เพิ่งเซ็นใหม่ไปแตะ entry แม้ entry นั้นจะยังไม่มีอยู่ก็ตาม
v0.9.6 เพิ่มไฟล์ marker เล็ก ๆ ที่
`~/.config/thclaws/sso-known.json` ซึ่งระบุ issuer ที่คุณ sign in
ไปจริง ๆ ตอน startup จะปรึกษา marker นี้ก่อนจะ probe keychain
ดังนั้นผู้ใช้ที่ไม่เคย sign in (และผู้ใช้ backend แบบ dotenv ที่ SSO
ไม่เกี่ยวข้องด้วย) จะไม่เจอ prompt เลย marker นี้ไม่มี secret ใด ๆ —
เป็นแค่ hint แบบ denormalised ว่า "ใช่ มี session ของ X อยู่"

### override สำหรับองค์กร

องค์กรที่ ship ไฟล์ policy ที่เซ็นแล้วพร้อม `policies.sso` จะ
override ปุ่ม Google/Microsoft มาตรฐาน — navbar จะแสดง IdP ของเขา
แทน ดูโมเดล verification ฝั่ง gateway ได้ในเอกสาร SSO ของ technical
manual
