# thClaws Enterprise Edition — คู่มือผู้ดูแลระบบ

> ฉบับแปลภาษาไทยของ [`ENTERPRISE.md`](ENTERPRISE.md) ถ้าสองฉบับไม่ตรงกัน
> ให้ยึดฉบับภาษาอังกฤษเป็นหลัก

> **สถานะ:** Phase 0–4 (โครงสร้าง policy, branding, allow-list ของ
> plugin/skill/MCP, การบังคับใช้ gateway, OIDC SSO) ship ใน **v0.6.0**;
> `audit` (บันทึก tool call ฝั่ง client) ใน **v0.120.0**; `runtime`
> (บังคับ permission mode, รายการ tool ที่ห้าม, สวิตช์ Remote และ
> `--serve`) ใน release ถัดไป ดูหัวข้อ "สถานะรายเฟส" ว่าแต่ละเฟส
> ครอบคลุมอะไรและมีข้อจำกัดที่รู้อยู่ตรงไหน

เอกสารนี้สำหรับผู้ดูแลระบบฝ่าย IT/Security ที่กำลังประเมินหรือ deploy
thClaws ภายในองค์กร อ่านฉบับนี้ถ้าคุณต้องการ:

- บังคับให้ LLM call ทุกครั้งวิ่งผ่าน gateway ส่วนตัวของคุณ (LiteLLM,
  Portkey, Azure OpenAI proxy) เพื่อคุมค่าใช้จ่าย เก็บ audit log หรือ
  ทำ rate limiting
- จำกัดว่าผู้ใช้ติดตั้ง MCP server, skill และ plugin จากที่ไหนได้บ้าง
- ตรึงรายชื่อ provider ของโมเดลที่อนุญาต
- ทำ branding ให้ binary (โลโก้ ชื่อ อีเมลติดต่อ) สำหรับใช้ภายในองค์กร
- ล็อกพฤติกรรมของ client ไม่ให้ผู้ใช้ปลายทางแก้นโยบายความปลอดภัยได้

ถ้าคุณเป็นผู้ใช้ทั่วไปที่ต้องการใช้ thClaws ให้ดู
[`README.md`](README.md) แทน

---

## ก่อนเริ่ม: เอกสารนี้สมมติว่าคุณรู้อะไรมาแล้ว

ผู้ดูแลระบบส่วนใหญ่ที่อ่านเอกสารนี้เคย deploy ซอฟต์แวร์ที่บริหารจัดการ
มาแล้ว แต่ยังไม่เคย deploy *agent* ความต่างตรงนี้สำคัญ หัวข้อนี้จึงพูด
ให้ชัด ถ้าคุ้นศัพท์อยู่แล้วข้ามได้เลย

### agent ไม่ใช่ chatbot

chatbot รับข้อความแล้วคืนข้อความ ความล้มเหลวที่แย่ที่สุดของมันคือตอบผิด
แต่ thClaws เป็น **agent**: คำตอบของโมเดลอาจเป็นคำขอให้รัน tool แล้ว
engine ก็รัน tool นั้นบนเครื่องของผู้ใช้จริงๆ ผลลัพธ์ถูกป้อนกลับเข้าไป
แล้ววนซ้ำจนกว่าโมเดลจะเลิกขอ การอ่านไฟล์ แก้ไฟล์ และรันคำสั่ง shell
คือขั้นตอนปกติในลูปนั้น

นั่นคือเหตุผลที่องค์กรต้องมีกลไกควบคุมตรงนี้ ซึ่งไม่จำเป็นสำหรับ
แอปพลิเคชันเดสก์ท็อปธรรมดา สี่คำถามที่เอกสารนี้ตอบคือคำถามมาตรฐาน:

| คำถาม | ตอบด้วย |
|---|---|
| **ใครใช้ได้** | block `sso` |
| **มันเอื้อมถึงอะไรได้** | block `gateway`, `plugins` และ `runtime` |
| **ใครจ่าย เท่าไร** | block `gateway` |
| **เกิดอะไรขึ้น** | block `audit` |

### ศัพท์ที่ใช้ตลอดทั้งเอกสาร

- **Tool / tool call** — ความสามารถที่มีชื่อ ซึ่งโมเดลขอให้ engine
  รันได้ (`Read`, `Write`, `Edit`, `Bash`, `WebFetch`…) หนึ่งคำขอให้รัน
  หนึ่งครั้งคือ *tool call*: หน่วยที่ถูกอนุมัติ ถูกปฏิเสธ และถูก audit
- **Permission mode** — tool call ต้องมีมนุษย์กดตกลงหรือไม่: `ask`
  (ถามก่อนทุก tool ที่แก้ของ), `auto` (ไม่ถามเลย), `plan` (สำรวจแบบ
  อ่านอย่างเดียว tool ที่แก้ของถูกบล็อกตอน dispatch)
- **Confinement** — ข้อจำกัดระดับ OS ว่าคำสั่ง shell แตะอะไรได้บ้าง
  โดยไม่สนใจว่าคำสั่งเขียนว่าอะไร ใช้ Seatbelt บน macOS, Landlock บน
  Linux และมี bubblewrap เป็นตัวสำรอง ถ้าเครื่องไหนไม่มีตัว confine
  คำสั่งยังรันแต่ไม่ถูกจำกัด และ audit record จะระบุข้อเท็จจริงนั้นไว้
- **Provider** — บริษัทหรือเซิร์ฟเวอร์ที่โฮสต์โมเดล (Anthropic, OpenAI,
  Google หรือ runtime ในเครื่องอย่าง Ollama)
- **Gateway** — เซิร์ฟเวอร์ที่ *คุณ* รันเอง พูด HTTP API แบบเดียวกับ
  provider โน้ตบุ๊กทุกเครื่องคุยกับมันแทนที่จะคุยกับ provider และมันคือ
  ตัวที่ถือ credential ใช้ LiteLLM, Portkey, Azure OpenAI deployment
  หรือของที่ทำเองก็ได้
- **MCP / plugin / skill** — สามวิธีที่ความสามารถถูกเพิ่มให้ agent
  **MCP server** เปิด tool เพิ่ม ผ่าน **stdio** (โปรแกรมในเครื่องที่ถูก
  เปิดเป็น subprocess) หรือ **HTTP** (URL ปลายทาง); **plugin** คือชุด
  ที่แพ็กมา ติดตั้งจาก URL หรือ repo; **skill** คือไฟล์คำสั่ง markdown
  ที่อาจพก script มาด้วย ทั้งสามคือช่องที่โค้ดบุคคลที่สามเข้าสู่เครื่อง
  จึงถูกครอบด้วย policy block เดียวกัน
- **Session log** — ไฟล์ JSONL บนเครื่องผู้ใช้ที่เก็บเนื้อหาบทสนทนาจริง
  ต่างจาก audit record ซึ่งเก็บข้อเท็จจริงเกี่ยวกับ tool call และจงใจ
  ไม่เก็บ payload
- **IdP** — identity provider: Okta, Microsoft Entra ID, Google
  Workspace, Auth0, Keycloak, Ping
- **SIEM** — ที่ที่ทีมความปลอดภัยของคุณรวบรวม log (Splunk, Sentinel,
  Elastic, QRadar) sink แบบ `http` ของ block `audit` ส่งไปที่นั่น
- **Fail-closed / fail-open** — เมื่อกลไกทำงานไม่ได้ ระบบหยุด หรือทำงาน
  ต่อโดยไม่มีกลไกนั้น? ตัวโหลด policy ของ thClaws เป็น **fail-closed**
  (policy ที่ตรวจไม่ผ่านทำให้โปรแกรมหยุด) ส่วน audit sink เป็น
  **fail-open** (SIEM ที่ติดต่อไม่ได้ไม่เคยขวางงานของผู้ใช้ และของที่
  หายถูกนับไว้) ทั้งสองอย่างตั้งใจ

### งานนี้คืองานอะไรกันแน่

การ deploy สิ่งนี้คือกิจกรรมสี่อย่างที่มีจังหวะต่างกันมาก และการเอามา
ปนกันคือต้นเหตุของปัญหาตามปกติ:

| กิจกรรม | ความถี่ | ถ้าพลาดจะเป็นอย่างไร |
|---|---|---|
| **ถือ signing key** | ครั้งเดียว แล้วตลอดไป | ถูกยึดทั้งหมด นี่คือ root of trust |
| **Build binary ที่เชื่อ key นั้น** | ครั้งเดียวต่อ release ของ thClaws | ไม่มีใครติดตั้งเวอร์ชันใหม่ได้ เครื่องเดิมยังทำงานปกติ |
| **เขียน ลงนาม deploy policy** | ทุกครั้งที่กฎเปลี่ยน | กฎผิดมีผลบังคับ หรือ binary ปฏิเสธการเริ่มทำงาน |
| **เฝ้าดูบน production** | ตลอดเวลา | วันหมดอายุมาถึงโดยไม่มีใครสังเกต แล้วทุกโต๊ะหยุดพร้อมกัน |

ความไม่สมมาตรที่ต้องออกแบบรอบมัน: **เปลี่ยนกฎถูก** (แก้ JSON ลงนามใหม่
push ไฟล์เดียว ไม่ต้องติดตั้งใหม่) **เปลี่ยน key แพง** (binary ใหม่ทุก
เครื่อง) ให้สิ่งที่คาดว่าจะเปลี่ยนบ่อยอยู่ใน policy และแตะ key ปีละครั้ง

---

## หลักการทำงาน

thClaws เป็นโอเพนซอร์สและใช้ฟรี ไม่มี codebase "Enterprise" แยกต่างหาก
— binary ตัวเดียวกันรันได้ทั้งสองโหมด สิ่งที่ทำให้มันกลายเป็น
"enterprise client" คือ **ไฟล์ policy ขององค์กรที่ลงนามแล้ว**

```
┌─────────────────────────────────────────────────────────┐
│         thClaws binary (open source, MIT/Apache)        │
│                                                         │
│  ┌─────────────────────────────────────────────────┐    │
│  │ Org Policy Loader (Ed25519 signature verifier)  │    │
│  └────────────────┬────────────────────────────────┘    │
│                   │                                     │
│       ┌───────────┴────────────┐                        │
│       │                        │                        │
│       ▼                        ▼                        │
│  ไม่มี policy บนดิสก์    policy ที่ตรวจผ่าน               │
│  → ใช้ตัวที่ฝัง          → กฎขององค์กรมีผล                │
│    ถ้าไม่มีก็ open-core    (branding, allow-list,        │
│                            gateway, SSO ฯลฯ)            │
└─────────────────────────────────────────────────────────┘
```

**คุณสมบัติหลัก:**

- **เมื่อไม่มีไฟล์ policy** thClaws ทำงานเหมือนที่ทำให้ชุมชนโอเพนซอร์ส
  ทุกประการ ไม่มี overhead ไม่มีพฤติกรรมเปลี่ยน (build ที่ฝัง policy ไว้
  จะใช้ตัวที่ฝังแทน ส่วน build ที่บังคับต้องมี policy จะปฏิเสธการเริ่ม
  ทำงาน ดู [การแจกจ่ายเมื่อไม่มีระบบจัดการเครื่องปลายทาง](#การแจกจ่ายเมื่อไม่มีระบบจัดการเครื่องปลายทาง))
- **เมื่อมีไฟล์ policy ที่ตรวจผ่าน** binary บังคับใช้กฎในไฟล์นั้นและทับ
  ค่า settings ระดับผู้ใช้ที่ขัดกัน
- **เมื่อมีไฟล์ policy ที่ตรวจไม่ผ่านหรือหมดอายุ** binary ปฏิเสธการเริ่ม
  ทำงาน — ไม่มีการถอยกลับไป "โหมดเปิด" เงียบๆ เมื่อมี policy อยู่แล้ว
- key ที่ใช้ตรวจสอบถูก **ฝังตอน build** ใน binary ขององค์กรคุณ ผู้ใช้
  ไม่สามารถเอา key ของตัวเองมาใส่เพื่อหลบได้

นี่คือแพตเทิร์นเดียวกับที่ GitLab, Mattermost และ Sentry ใช้: open core
พร้อมห่อเชิงพาณิชย์

---

## โมเดลการ deploy

มีของสองชิ้นที่องค์กรต้อง deploy — หรือชิ้นเดียวถ้าคุณฝัง policy ไว้ใน
binary (ดู [การแจกจ่ายเมื่อไม่มีระบบจัดการเครื่องปลายทาง](#การแจกจ่ายเมื่อไม่มีระบบจัดการเครื่องปลายทาง))

### 1. binary ของ thClaws (ครั้งเดียวต่อ release)

คอมไพล์โดยฝัง **public key** แบบ Ed25519 ขององค์กรคุณไว้ แจกจ่ายผ่าน
ช่องทางแจกซอฟต์แวร์ปกติของคุณ (MDM, Intune, JAMF, system package,
พอร์ทัลภายใน)

นอกนั้น binary เหมือน release โอเพนซอร์สทุกประการ — ฟีเจอร์เดียวกัน
โค้ดเดียวกัน ไลเซนส์ MIT/Apache เดียวกัน ความต่างเดียวคือ build นี้
เชื่อถือ policy ที่ลงนามด้วย private key ของคุณ

### 2. ไฟล์ policy ที่ลงนามแล้ว (หมุนเวียนตามต้องการ)

เอกสาร JSON ที่ลงนามด้วย **private key** แบบ Ed25519 ขององค์กรคุณ
(ซึ่งไม่เคยออกจากโครงสร้างพื้นฐานด้านความปลอดภัยของคุณ) แจกจ่ายไปยัง
เครื่องผู้ใช้ผ่าน:

- `/etc/thclaws/policy.json` (ทั้งระบบ deploy ด้วย MDM/configmap)
- `~/.config/thclaws/policy.json` (สำรองรายผู้ใช้ ตั้งด้วย login script)

ใช้ได้ทั้งสองที่ ถ้ามีทั้งคู่ path ระดับระบบมาก่อน

ไฟล์ public key deploy ไปคู่กันที่ `/etc/thclaws/policy.pub` (หรือ
`~/.config/thclaws/policy.pub`) — มีประโยชน์กับ build โอเพนซอร์สที่
ต้องการตรวจสอบตอน runtime โดยไม่ต้องคอมไพล์ใหม่ ส่วน build แบบ
enterprise ฝัง key ไว้ตอนคอมไพล์อยู่แล้ว จึงไม่ต้องใช้ไฟล์นี้ตอน runtime

---

## เริ่มต้นอย่างเร็ว (เดินทีละขั้น 10 นาที)

ขั้นตอนนี้ให้ deployment แบบ enterprise ที่ใช้งานได้จริงบนเครื่องเดียว
สำหรับการประเมิน การ rollout จริงมีขั้นตอนเพิ่ม (keygen แบบออฟไลน์,
deploy ผ่าน MDM, ตั้งค่า IdP) ซึ่งอยู่ในหัวข้อถัดไป

### สิ่งที่ต้องมี

- เครื่องที่มี Rust toolchain (`cargo`) — ใช้ครั้งเดียวเพื่อ build
  binary เฉพาะขององค์กร
- ซอร์สของ thClaws: `git clone https://github.com/thClaws/thClaws`

### 1. สร้าง keypair ขององค์กร

```bash
cd thClaws
cargo build --release --bin thclaws-policy-tool
./target/release/thclaws-policy-tool keygen \
    --public  ~/.config/thclaws/policy.pub \
    --private ~/secure/acme-org.key
```

> **สำคัญ:** **private** key คือ root of trust ของ policy ทุกฉบับที่คุณ
> จะลงนามตลอดไป เก็บไว้ออฟไลน์ ใน hardware security module หรือใน
> secrets manager ที่คุณมีอยู่แล้ว private key ที่รั่วแปลว่าผู้โจมตี
> ออก policy ที่ binary ของคุณจะเชื่อถือได้
>
> **public** key เผยแพร่ได้อย่างปลอดภัย — มันไปอยู่ใน binary และในไฟล์
> config ที่ deploy ออกไป

### 2. Build binary ของ thClaws ที่เชื่อถือ key ของคุณ

```bash
# build script หยิบ ~/.config/thclaws/policy.pub เป็นค่าเริ่มต้น
# ถ้าต้องการเปลี่ยน ให้ตั้ง THCLAWS_POLICY_PUBKEY_PATH เป็น path อื่น
cd thclaws/crates/core    # หรือ root ของ repo ถ้าใช้ layout แบบ workspace
cargo build --release --bin thclaws --features gui
```

ตรวจว่าฝังสำเร็จ:

```bash
strings ./target/release/thclaws | grep -A1 "POLICY_PUBKEY" || true
# หรือรัน binary โดยไม่มีไฟล์ policy — build แบบ open-core จะเริ่มทำงาน
# ตามปกติ (UX เดิมเมื่อไม่มี policy); build ที่ฝัง policy ไว้ด้วยจะใช้
# ตัวที่ฝัง; ส่วน build ที่ตั้ง THCLAWS_REQUIRE_POLICY=1 แล้วไม่มี policy
# ที่ไหนเลย จะปฏิเสธด้วย exit 2
```

สำหรับ deployment จริง ให้ build ให้ครบทุกสถาปัตยกรรมเป้าหมาย (Linux
x86_64, Linux ARM64, macOS Apple Silicon, macOS Intel, Windows x86_64,
Windows ARM64) แล้วแจกจ่ายผ่านกระบวนการ signing/notarization ปกติของคุณ

### 3. ร่างไฟล์ policy

สร้าง `policy.json`:

```json
{
  "version": 1,
  "issuer": "ACME Corp Security",
  "issued_at": "2026-04-27T00:00:00Z",
  "expires_at": "2027-04-27T00:00:00Z",
  "binding": {
    "org_id": "acme-corp"
  },
  "policies": {
    "branding": {
      "enabled": true,
      "name": "ACME Agent",
      "support_email": "security@acme.example",
      "banner_text": "ACME internal AI assistant — confidential."
    },
    "plugins": {
      "enabled": true,
      "allowed_hosts": [
        "github.com/acmecorp/*",
        "internal.acme.example/*"
      ],
      "allow_external_scripts": false,
      "allow_external_mcp": false
    },
    "gateway": {
      "enabled": true,
      "url": "https://gateway.acme.internal/v1",
      "auth_header_template": "Bearer {{sso_token}}",
      "fail_closed": true,
      "read_only_local_models_allowed": false
    },
    "sso": {
      "enabled": true,
      "provider": "oidc",
      "issuer_url": "https://acme.okta.com",
      "client_id": "thclaws-internal",
      "audience": "thclaws"
    },
    "runtime": {
      "enabled": true,
      "permission_mode": "ask",
      "deny_tools": ["Bash", "WebFetch"],
      "allow_remote": false,
      "allow_serve": false
    }
  }
}
```

block `runtime` คือบล็อกที่พูดว่า *ไม่*:

| Field | ผลที่เกิด |
|---|---|
| `permission_mode` | บังคับ `ask`, `auto` หรือ `plan` ถูกใช้หลัง `settings.json` **และ** หลัง CLI flag ดังนั้น `--permission-mode auto` กับ `--accept-all` ปีนข้ามไม่ได้ ถ้าไม่ระบุจะปล่อยให้ผู้ใช้เลือกเอง |
| `deny_tools` | ชื่อ tool ที่ agent ใช้ไม่ได้ ถูกถอดออกจากทุก registry โมเดลจึงไม่เห็นเลย และถูกปฏิเสธซ้ำตอน dispatch — subagent ที่สร้าง registry ของตัวเองก็ยังเรียกไม่ได้ ไม่สนตัวพิมพ์ |
| `allow_remote` | `false` บล็อก thClaws Remote ซึ่งเป็น tunnel ที่ทำให้ agent ของเครื่องนี้เข้าถึงได้จาก cloud ทุกเส้นทางที่เริ่ม session จะปฏิเสธ ค่าเริ่มต้น `true` |
| `allow_serve` | `false` ทำให้ `--serve` ไม่ยอม bind ปิดพื้นผิว HTTP ที่พา web UI และ API แบบ OpenAI-compatible มาด้วย ค่าเริ่มต้น `true` |

boolean สองตัวมีค่าเริ่มต้นเป็น **true** โดยตั้งใจ: policy ปิด Remote
ด้วยการลืม field ไม่ได้ ปิดได้ด้วยการระบุเท่านั้น `audit` บันทึกว่าเกิด
อะไรขึ้น ส่วน `runtime` ตัดสินว่าอะไรเกิดขึ้นได้ — deploy ทั้งคู่ และ
tool call ที่ถูกปฏิเสธก็ยังถูก audit ด้วย `decided_by: "policy"`

flag `policies.<feature>.enabled` แต่ละตัวคุมว่าฟีเจอร์นั้นมีผลหรือไม่
block ที่ปิดหรือไม่ได้ระบุจะถอยไปใช้พฤติกรรมค่าเริ่มต้นแบบโอเพนซอร์ส —
มีประโยชน์กับการ rollout เป็นระยะ (เช่น เริ่มด้วย branding + allow-list
ของ plugin แล้วค่อยเพิ่ม gateway ทีหลัง)

### 4. ลงนาม policy

```bash
./target/release/thclaws-policy-tool sign policy.json \
    --private-key ~/secure/acme-org.key
```

การรัน `sign` ซ้ำกับไฟล์ที่ลงนามแล้วปลอดภัย — ลายเซ็นเดิมถูกถอดออกและ
แทนที่ด้วยอันใหม่

### 5. Deploy ไปเครื่องทดสอบ

```bash
# รายผู้ใช้ (เหมาะกับการทดสอบ)
mkdir -p ~/.config/thclaws
cp policy.json ~/.config/thclaws/policy.json

# ทั้งระบบ (production)
sudo mkdir -p /etc/thclaws
sudo cp policy.json /etc/thclaws/policy.json
sudo chown root:root /etc/thclaws/policy.json
sudo chmod 644 /etc/thclaws/policy.json
```

#### การแจกจ่ายเมื่อไม่มีระบบจัดการเครื่องปลายทาง

ของสองชิ้นคือ binary (มี public key ของคุณคอมไพล์อยู่ข้างใน) กับ
`policy.json` ส่วน private key ไม่เคยออกจากการควบคุมของคุณ

MDM คือสิ่งที่รับประกันว่าไฟล์ policy ไปถึงจริง ถ้าไม่มี ผู้ใช้ที่ไม่เคย
วางไฟล์ — หรือลบมันทิ้ง — จะรัน binary ที่ไม่ถูกจำกัด เพราะ build ที่ไม่มี
policy ทำตัวเหมือน open-core ทุกประการ

ให้ build โดยฝัง policy ที่ลงนามแล้วเข้าไป แล้วคุณจะแจกของ **ชิ้นเดียว**:

```bash
THCLAWS_POLICY_PUBKEY_PATH=policy.pub \
THCLAWS_POLICY_FILE_EMBED=policy.json \
  cargo build --release --features gui
```

| บนเครื่อง | ผลลัพธ์ |
|---|---|
| มีไฟล์ policy อยู่ | ใช้ไฟล์นั้น |
| ไม่มีไฟล์ แต่มี policy ฝังไว้ | ใช้ตัวที่ฝัง |
| ไม่มีทั้งคู่ และ build นี้บังคับต้องมี | ปฏิเสธการเริ่มทำงาน exit 2 |
| ไม่มีทั้งคู่ และเป็น build แบบ open-core | รันโดยไม่ถูกจำกัด |

ไฟล์ยังชนะเสมอ ซึ่งเป็นสิ่งที่รักษาการหมุนเวียนไว้: ลงนามใหม่ แจกไฟล์
เดียว ไม่ต้อง build ใหม่ ทุกแหล่งถูกตรวจเหมือนกันหมด — ลายเซ็น
`expires_at` และ `binding` ถูกตรวจไม่ว่า policy จะมาทางไหน

build จะบังคับต้องมี policy เมื่อมันพกทั้ง key และ policy ที่ฝังไว้
ส่วน `THCLAWS_REQUIRE_POLICY=1` ใช้บังคับสำหรับ deployment ที่ส่ง policy
ผ่าน MDM อย่างเดียว

ตั้ง `expires_at` ของ policy ที่ฝังไว้ให้ยาว: การต่ออายุแปลว่าต้อง build
และแจกจ่าย binary ใหม่ และ policy ที่ฝังไว้แล้วหมดอายุจะปฏิเสธการเริ่ม
ทำงาน

วิธีนี้ปิดอุบัติเหตุเรื่องไฟล์หาย แต่ไม่หยุดคนที่ตั้งใจ — binary แบบ
open-core เป็นของดาวน์โหลดสาธารณะ และไม่มี client ตัวไหนกันได้ การจำกัด
ว่า binary ไหนรันได้เป็นเรื่องของกลไกควบคุมอุปกรณ์หรือเครือข่าย

### 6. ตรวจว่าโหลดสำเร็จ

```bash
./thclaws --version
# รัน GUI หรือ CLI — ข้อความ branding ควรขึ้นเป็น "ACME Agent",
# `/plugin install` จาก host ที่ไม่อนุญาตควรถูกปฏิเสธ ฯลฯ
```

ถ้า binary ปฏิเสธการเริ่มทำงานพร้อมข้อความ `signature verification
failed` หรือ `expired` แปลว่า policy กับ key ไม่ตรงกัน — ดู
[Troubleshooting](#troubleshooting)

---

## สถานะรายเฟส

รูปแบบไฟล์ policy เสถียรตั้งแต่ v0.5.0 ส่วน policy แต่ละตัวจะบังคับใช้
ได้เมื่อเฟสของมัน ship:

| Policy block | Phase | สถานะ | ออกใน |
|---|---|---|---|
| `branding` (โลโก้, ชื่อ, อีเมลติดต่อ, banner) | 1 | ✅ Ship แล้ว (ฝั่ง Rust) | v0.5.0 |
| `plugins` (allow-list, ห้าม script ภายนอก, ห้าม MCP ภายนอก) | 2 | ✅ Ship แล้ว | v0.5.0 |
| `gateway` (จัดเส้นทาง HTTP, fail-closed, แนบ identity) | 3 | ✅ Ship แล้ว | v0.5.0 |
| `sso` (OIDC discovery, PKCE, เก็บ token, identity ไปยัง gateway) | 4 | ✅ Ship แล้ว (ทดสอบจริงกับ Google) | v0.6.0 |
| `audit` (บันทึก tool call ฝั่ง client, sink แบบ file + http) | 5 | ✅ ทำแล้ว — [RFC 0001](docs/rfc/0001-tool-call-audit.md), [#203](https://github.com/thClaws/thClaws/issues/203) | v0.120.0 |
| `runtime` (บังคับ permission mode, รายการ tool ที่ห้าม, สวิตช์ Remote + `--serve`) | 8 | ✅ ทำแล้ว | release ถัดไป |

ไฟล์ policy ที่มี block ครบทุกตัวใช้ได้กับ build v0.5.x ขึ้นไปทั้งหมด
block ของเฟสที่ยังไม่ทำจะถูกยอมรับแต่เฉยๆ เมื่อเฟสนั้น ship ไฟล์ policy
เดิมจะเริ่มถูกบังคับใช้โดยไม่ต้องลงนามใหม่

### ข้อจำกัดที่รู้อยู่ บอกไว้ตั้งแต่ต้น

ทุกข้อในนี้ผู้ตรวจสอบความปลอดภัยหาเจอได้อยู่แล้ว เราจึงอยากให้คุณได้ยิน
จากเราเอง:

| ข้อจำกัด | ในทางปฏิบัติแปลว่าอะไร |
|---|---|
| string บางจุดใน React GUI ยังขึ้นว่า "thClaws" | branding ฝั่งหลังบ้าน (REPL banner, ชื่อหน้าต่าง GUI, system prompt) ทำงานเต็มที่ แต่ string บางตัวในหน้าต่าง GUI ยังไม่ผ่าน branding module เป็นเรื่องผิวๆ |
| plugin allow-list กันที่การติดตั้ง ไม่ใช่ที่ดิสก์ | เมื่อ `allow_external_mcp: false` MCP server แบบ stdio ถูกปฏิเสธ และแบบ HTTP ต้องตรง `allowed_hosts` แต่ไฟล์ที่คัดลอกลงโฟลเดอร์ skills หรือ plugins ตรงๆ (ด้วยมือหรือด้วย `Bash` ของ agent) ยังโหลดอยู่ ให้ใช้คู่กับ `runtime.deny_tools` และ egress filtering |
| `WebFetch` / `WebSearch` ไม่วิ่งผ่าน gateway | มันคือการเข้าเว็บทั่วไป ให้ใช้ network firewall ดูแล |
| `gateway.fail_closed` บังคับด้วยโครงสร้างโค้ด | ไม่มีเส้นทางไหนสร้าง provider ตรงได้ขณะ gateway ทำงาน แต่ไม่มี guard แยกที่ชั้น HTTP |
| audit sink เป็น fail-open | SIEM ที่ติดต่อไม่ได้ไม่เคยบล็อก tool call ของที่หายถูกนับและแสดงใน `/policy status` ถ้าท่าทีด้าน compliance ของคุณต้องการ audit แบบ fail-closed บอกเราได้ — เป็น change request ไม่ใช่ flag |
| ยังเพิกถอน key ไม่ได้โดยไม่ build ใหม่ | ถ้า signing key รั่ว ทางแก้คือ keypair ใหม่ binary ใหม่ และลงนาม policy ใหม่ วันนี้ยังไม่มี kill switch จากระยะไกล |
| ทดสอบจริงกับ IdP เฉพาะ Google Workspace | Okta และ Entra ID รองรับและมี unit test พร้อม template ของ policy ให้กันเวลา smoke test หนึ่งรอบกับ tenant ของคุณเอง |
| deployment แบบเซิร์ฟเวอร์ร่วม (multiuser) บังคับ auto-approve | worker ตัวเดียวที่รับใช้หลายคนส่ง prompt ขออนุมัติไปหาคนที่ถูกต้องไม่ได้ ให้เปิด `audit` ที่นั่น |

---

## เลือกว่าจะเปิด block ไหน

แต่ละ block เป็นอิสระต่อกันและเฉยๆ จนกว่า policy จะเปิด การ deploy จึง
ทยอยได้ ผู้ดูแลระบบมักคว้า `plugins` ก่อนเพราะเข้าใจง่าย ทั้งที่
`gateway` กับ `runtime` คือสองตัวที่เปลี่ยนภาพความเสี่ยงจริง

| Block | ตอบคำถามอะไร | เปิดเมื่อ |
|---|---|---|
| `branding` | — (การยอมรับ ไม่ใช่ความปลอดภัย) | อยากให้พนักงานเห็นว่านี่คือโครงสร้างพื้นฐานภายใน |
| `plugins` | อะไร *ติดตั้ง* ได้บ้าง | ห่วงเรื่องโค้ดบุคคลที่สามเข้าถึงเครื่อง |
| `gateway` | ข้อมูลไปไหน และใครจ่าย | เกือบทุกกรณี เป็นบล็อกที่คุ้มค่าที่สุด |
| `sso` | ใครใช้ได้ | มี IdP อยู่แล้วและอยากให้กระบวนการเข้า/ออกมีผล |
| `audit` | เกิดอะไรขึ้น | compliance ต้องการหลักฐาน หรือคุณรันแบบ multiuser |
| `runtime` | อะไรเกิดขึ้นได้บ้าง | ต้องการคำว่า "ทำไม่ได้" ไม่ใช่ "ถูกบันทึกไว้" |

`audit` ตอบว่า *เกิดอะไรขึ้น*; `runtime` ตัดสินว่า *อะไรเกิดขึ้นได้*
ผู้ตรวจสอบถามข้อแรก สถาปนิกความปลอดภัยถามข้อสอง

### แผน rollout ที่ใช้ได้ผล

| ระยะ | เปิดอะไร | ได้เรียนรู้อะไร |
|---|---|---|
| 1. ทีม pilot | `branding` อย่างเดียว | ว่า build, การ push ผ่าน MDM และ pipeline ของ policy ทำงานทั้งหมด โดยไม่มีพฤติกรรมเปลี่ยนให้โทษ |
| 2. Pilot | `+ gateway` | ว่า allow-list ของโมเดลที่ gateway ตรงกับที่คนต้องใช้จริงหรือไม่ เตรียมใจกับ "ไม่มีโมเดล X" สักสัปดาห์ |
| 3. Pilot | `+ sso` | ว่า client ที่ IdP ลงทะเบียนถูกหรือไม่ |
| 4. Pilot | `+ audit` | ว่า SIEM ของคุณรับ schema ได้ และปริมาณเท่าไร |
| 5. ทั้ง fleet | สี่อย่างเดิม | — |
| 6. ทั้ง fleet | `+ plugins`, `+ runtime` | ตัวที่จำกัด เอาไว้ท้ายสุด หลังรู้แล้วว่าการใช้งานปกติหน้าตาเป็นอย่างไร |

สองกฎที่ช่วยเลี่ยงเหตุการณ์: **อย่าใส่ข้อจำกัดใหม่พร้อม binary ใหม่ใน
การเปลี่ยนแปลงครั้งเดียวกัน** (พอมีอะไรพัง คุณจะไม่รู้ว่าอันไหนเป็นเหตุ)
และ **ให้กลุ่ม pilot ใช้ `expires_at` สั้นกว่าทั้ง fleet** การหมดอายุจะ
ได้ล้มก่อนบนเครื่องที่คุณกำลังเฝ้าดูอยู่

---

## เรื่องปฏิบัติการ

### การหมุนเวียน key

แนวปฏิบัติที่ดี: หมุนเวียน keypair ปีละครั้ง (หรือเมื่อสงสัยว่ารั่ว)
การหมุนเวียนต้องทำ:

1. สร้าง keypair ใหม่ (`thclaws-policy-tool keygen`)
2. Build binary ของ thClaws ใหม่โดยฝัง public key ใหม่
3. แจกจ่าย binary ใหม่ไปยังเครื่องผู้ใช้ (ทับของเดิม)
4. ลงนาม policy ที่ใช้อยู่ทั้งหมดใหม่ด้วย private key ใหม่
5. ยกเลิก private key เก่า

จนกว่าขั้นที่ 3 จะสำเร็จบนเครื่องหนึ่ง เครื่องนั้นยังเชื่อถือ policy ที่
ลงนามด้วย key เก่าอยู่ ใน v0.5.x ยังไม่มีกลไกเพิกถอนจากระยะไกล — การ
ยกเลิกคือ "เลิกลงนามด้วย key เก่า แล้วส่ง binary ใหม่ที่ไม่เชื่อ key เก่า
ออกไป" เพียงพอสำหรับ deployment ส่วนใหญ่ ส่วนการเพิกถอนแบบสดคล้าย
CRL/OCSP เพิ่มทีหลังได้ถ้ามีความต้องการ

**ลำดับสำคัญตอนเกิดเหตุ:** ส่ง binary ใหม่ออกไป **ก่อน** ยกเลิก key เก่า
ไม่งั้น binary ที่ยังอยู่จะตรวจ policy ใหม่ไม่ผ่าน

### การหมดอายุของ policy

ตั้ง `expires_at` เป็นวันที่คุณจะลงนามใหม่ก่อนถึงแน่ๆ การตั้งรายปีเป็น
เรื่องปกติ binary ปฏิเสธการเริ่มทำงานเมื่อ policy หมดอายุ — ไม่มีช่วง
ผ่อนผัน

สำหรับการ rollout เป็นระยะที่แก้ policy บ่อย การตั้ง 90 วันช่วยให้ความ
เปลี่ยนแปลงยังอยู่ในสายตา ส่วน deployment ที่นิ่งแล้ว 12 เดือนสมเหตุสมผล

### การผูก policy กับ fingerprint ของ binary

ไม่บังคับ: ตรึง policy ไว้กับ build หนึ่งโดยตั้ง
`binding.binary_fingerprint` กรณีใช้งาน: กันพนักงานที่ไม่พอใจคัดลอก
binary ของบริษัทไปลงเครื่องส่วนตัวแล้วใช้ policy เดิมคุยผ่าน gateway ของคุณ

```bash
# คำนวณ fingerprint ของ binary ที่เพิ่ง build:
./target/release/thclaws-policy-tool fingerprint ./target/release/thclaws
# ผลลัพธ์: sha256:abc123...
```

ใส่ลงใน policy:

```json
"binding": {
  "org_id": "acme-corp",
  "binary_fingerprint": "sha256:abc123def456..."
}
```

การจับคู่แบบ prefix ถูกยอมรับ คุณจึงใช้ fingerprint บางส่วนได้
(`"sha256:abc123"`) ถ้าคุณ build ใหม่บ่อยโดยไม่ได้แก้โค้ด (เช่น แค่
อัปเดต git SHA ที่ฝังอยู่)

### Audit logging

Audit logging เกิดขึ้นที่ **ชั้น gateway** ของคุณ ไม่ใช่ภายใน thClaws เอง
เมื่อบังคับ `policies.gateway.enabled: true` (Phase 3 ขึ้นไป) ทุก
provider call จะวิ่งผ่าน gateway ของคุณพร้อม SSO token ของผู้ใช้ใน auth
header — audit log ที่ gateway ของคุณมีอยู่แล้วจึงจับได้ว่าใครทำอะไร

นี่คือการออกแบบโดยตั้งใจ — การทำ audit log ซ้ำสองที่สร้างความเสี่ยงที่
ข้อมูลจะไม่ตรงกัน

บริบทฝั่ง client (tool ไหนรัน ใครอนุมัติ ถูก confine อย่างไร แตะไฟล์ไหน)
คือ **Phase 5** (v0.120.0 ขึ้นไป): block `policies.audit` ที่เขียน record
บางเบา ไม่มี payload และผูกกับ session JSONL ดีไซน์และ schema ของ record:
[RFC 0001](docs/rfc/0001-tool-call-audit.md)

```json
"audit": {
  "enabled": true,
  "sinks": [
    { "type": "file", "path": "/var/log/thclaws/audit-%Y-%m-%d.jsonl" },
    { "type": "http", "url": "https://siem.acme.example/thclaws",
      "auth_header_template": "Bearer {{env:THCLAWS_AUDIT_TOKEN}}",
      "batch": 50, "flush_secs": 5 }
  ],
  "include_summary": true,
  "correlate_gateway": true
}
```

- หนึ่งบรรทัด JSON ต่อหนึ่ง tool call (`tool_call` / `tool_denied`) บวก
  `session_start` / `session_end` แต่ละ record มีชื่อ tool, ใครอนุมัติ
  (`auto`, `repl`, `gui`, `bot:line`, …), โหมด confine ของ Bash และว่ามัน
  ถูกบังคับจริงหรือไม่, path ของไฟล์ที่ถูกแตะ, summary 256 ไบต์, ค่า
  SHA-256 ของ input และ output และเวลา **ไม่มีตัว input หรือ output เอง
  เด็ดขาด** — สองอย่างนั้นอยู่ใน session JSONL และ digest ทำให้ผู้ตรวจสอบ
  ยืนยัน record กับไฟล์นั้นได้
- `file.path` รับ strftime token ถ้าไม่ระบุจะเป็นไฟล์รายวันใต้ data dir
  ของผู้ใช้ ส่วน `http` ส่ง NDJSON เป็นชุดผ่าน POST โดย auth header ใช้
  template `{{env:NAME}}` / `{{sso_token}}` แบบเดียวกับ `gateway`
- `actor` คืออีเมลจาก SSO เมื่อ `policies.sso` ทำงานอยู่, เป็น member id
  บน hosted multiuser runner, ถ้าไม่ใช่ทั้งสองก็เป็น login ของ OS
- `correlate_gateway` เพิ่ม `x-thclaws-session` / `x-thclaws-turn` ให้ทุก
  provider request ที่ผ่าน gateway ขององค์กร เพื่อให้ log สองฝั่ง join กันได้
- **Fail-open**: sink ที่ล้มเหลวไม่เคยบล็อก tool call จำนวนที่ drop แสดง
  ใน `/policy status` และใน record `session_end`
- `enabled: true` พร้อม `sinks` ว่าง จะปฏิเสธการเริ่มทำงาน เหมือน gateway
  ที่เปิดแต่ไม่มี URL

### Checklist ตรวจก่อนปิดงาน

ไล่ตามรายการนี้บน endpoint จริง — ไม่ใช่เครื่อง build — ก่อนประกาศว่า
deployment เสร็จ ทุกบรรทัดคือสิ่งที่เคยล้มเหลวเงียบๆ กับใครบางคนมาแล้ว

- [ ] `/policy status` ระบุไฟล์ policy ของคุณ ผู้ออก และ block ที่คุณ
      คาดไว้ ถ้าขึ้น `no org policy active` ทุกข้อที่เหลือไม่มีความหมาย
- [ ] วันหมดอายุที่แสดงคือวันที่ตั้งใจ และมีคนเป็นเจ้าของรายการปฏิทิน
      สำหรับลงนามใหม่ก่อนถึงวัน
- [ ] เปลี่ยนชื่อไฟล์ policy หนึ่งครั้ง แล้วยืนยันว่าเครื่องทำตัวตามที่
      วางแผน — พฤติกรรม community หรือปฏิเสธการเริ่มทำงานถ้า build นี้
      บังคับต้องมี policy การรู้ว่าจะได้แบบไหนคือประเด็น
- [ ] แก้ policy ที่ deploy ไปแล้วหนึ่งตัวอักษร แล้วยืนยันว่า binary
      ปฏิเสธด้วย `signature verification failed` แล้วคืนค่ากลับ
- [ ] เปิด `gateway`: `/models` แสดงแคตตาล็อกของ gateway และมี request
      ปรากฏใน log ของ gateway เองโดยระบุชื่อผู้ใช้ที่ลงชื่อเข้าใช้ ไม่ใช่
      service account ที่ใช้ร่วมกัน
- [ ] เปิด `gateway`: ตั้ง `OPENAI_API_KEY` ส่วนตัวใน environment แล้ว
      ยืนยันว่ามันถูกเมิน
- [ ] เปิด `sso`: `/sso login` สำเร็จกับ tenant จริง จากนั้นปิดบัญชี
      ทดสอบที่ IdP แล้วยืนยันว่าสิทธิ์จบตอน refresh ครั้งถัดไป
- [ ] เปิด `plugins`: การติดตั้งจาก host ที่ไม่อนุมัติถูกปฏิเสธพร้อม
      ข้อความบอกชื่อ host
- [ ] เปิด `audit`: มี record ไปถึง sink และ record ของ `Bash` มี
      `confine.enforced: true` บนแพลตฟอร์มที่คุณรองรับ ถ้าเป็น `false`
      แปลว่า image นั้นไม่มีตัว confine ของ OS ให้หาสาเหตุก่อนขึ้น
      production
- [ ] เปิด `runtime`: `--permission-mode auto` ไม่ชนะ `ask` ที่ถูกบังคับ
      และ tool ที่ถูกห้ามไม่ปรากฏในรายการ tool ของ agent
- [ ] ไฟล์ policy ที่ deploy ไปเป็นของ root และผู้ใช้ที่ล็อกอินอยู่เขียน
      ไม่ได้

### ความผิดพลาดที่พบบ่อย

| ความผิดพลาด | อาการ | ทางแก้ |
|---|---|---|
| แก้ policy ที่ลงนามแล้วในที่ตั้งบน endpoint | `signature verification failed` ทั้ง fleet | แก้ที่ต้นทาง ลงนามใหม่ deploy ใหม่ |
| Build ใหม่โดยไม่อัปเดต `binding.binary_fingerprint` | `binding mismatch` หลังการอัปเดตที่ดูปกติ | คำนวณ fingerprint ใหม่ หรือพึ่งการจับคู่แบบ prefix |
| ทดสอบกับ build ที่ไม่มี key ฝังไว้ | "กลไกไม่ทำงานเลย" | `/policy status` ก่อน |
| ลงทะเบียน client ที่ IdP เป็น Web application | `redirect_uri_mismatch` ตอนล็อกอินครั้งแรก | ลงทะเบียนใหม่เป็น Native / Desktop / public |
| ใช้ issuer ของ Azure แบบ v1 | `discovery doc … missing authorization_endpoint` | issuer ต้องลงท้ายด้วย `/v2.0` |
| คิดเอาเองว่า MDM profile ทำงานแล้ว | เครื่องกลุ่มหนึ่งไม่ถูกจำกัด แบบเงียบๆ | ตรวจบน endpoint จริงในทุกกลุ่ม |
| ตั้ง `expires_at` สั้นกับ policy ที่ *ฝัง* ไว้ | ทั้ง fleet หยุด และต้อง build ใหม่เพื่อแก้ | ฝังแล้วให้ตั้งวันหมดอายุยาว สั้นได้เฉพาะแบบไฟล์บนดิสก์ |

### อัปเดต policy โดยไม่ต้อง build binary ใหม่

binary ฝัง **public key** ไว้ตอนคอมไพล์ ส่วน **ไฟล์ policy** ถูกโหลดจาก
ดิสก์ตอนเริ่มโปรแกรม ดังนั้น:

- policy ใหม่ด้วย key เดิม → แค่แทนที่ `policy.json` แล้วเปิด thClaws
  ใหม่ ไม่ต้อง build
- key ใหม่ (หมุนเวียน) → build binary ใหม่ แล้วแจกจ่ายใหม่

ในทางปฏิบัติแปลว่าการแก้ policy ถูก (push ไฟล์ใหม่ผ่าน MDM) ส่วนการ
หมุนเวียน key เป็นเหตุการณ์ที่ต้องวางแผน

### หมายเหตุการ deploy ผ่าน MDM

- **macOS** — ใช้ configuration profile deploy `/etc/thclaws/policy.json`
  และ (ถ้าต้องการ) `/etc/thclaws/policy.pub` เป็น file payload แบบ plist
  มาตรฐาน
- **Windows** — Group Policy file copy หรือ Intune file deployment ไปที่
  `%PROGRAMDATA%\thclaws\policy.json` ในเอกสารเราใช้ POSIX path เพื่อความ
  ชัดเจน ส่วน runtime อ่าน `$THCLAWS_POLICY_FILE` คุณจึงระบุ path เองได้
- **Linux** — เครื่องมือ config management ที่คุณใช้อยู่ (Ansible,
  Puppet, configmap+kubectl) วางไฟล์ที่ `/etc/thclaws/policy.json`

### Provider / โมเดลที่อนุญาต

ใน v0.5.x ไม่มี policy block ที่บอกว่า "อนุญาตเฉพาะ provider เหล่านี้"
โดยตรง — เรื่องนั้นบังคับผ่าน gateway: ตั้งค่า gateway ให้รับเฉพาะ call
ของ provider/โมเดลที่คุณอนุมัติ แล้ว call อื่นจะล้มที่ gateway วิธีนี้ทำ
ให้ไฟล์ policy พูดถึง *เจตนา* ส่วน gateway เป็นผู้ชี้ขาดว่า *โมเดลไหน
ใช้ได้จริง*

---

## Troubleshooting

### binary ปฏิเสธการเริ่มทำงาน: `signature verification failed`

ไฟล์ policy ถูกลงนามด้วย key ที่ไม่ตรงกับ key ที่ฝังอยู่ใน (หรือที่
binary นี้เข้าถึงได้) สาเหตุที่เป็นไปได้:

1. policy ถูกลงนามด้วย private key ผิดตัว (ตรวจวัสดุ key)
2. binary ถูก build โดยไม่ได้ฝัง public key ของคุณ (`build.rs` หาไม่เจอ
   — ตรวจว่า `THCLAWS_POLICY_PUBKEY_PATH` ถูกตั้ง หรือมีไฟล์ที่ path
   มาตรฐานอยู่ตอน build)
3. ไฟล์ policy ถูกแก้หลังลงนาม — แม้ไบต์เดียวก็เปลี่ยนรูป canonical JSON
   และทำให้ลายเซ็นเป็นโมฆะ ให้ลงนามใหม่หลังแก้ทุกครั้ง

รัน `thclaws-policy-tool inspect policy.json` เพื่อดู `issuer` ที่ประกาศ
ไว้ใน policy แล้วเทียบว่าตรงกับที่ทีมปฏิบัติการของคุณออกให้หรือไม่

### binary ปฏิเสธการเริ่มทำงาน: `policy expired`

เลยวันที่ `expires_at` แล้ว ให้ลงนามใหม่พร้อมวันหมดอายุใหม่:

```bash
# แก้ policy.json เพื่อเลื่อน expires_at
./thclaws-policy-tool sign policy.json --private-key ~/secure/acme-org.key
# แล้ว deploy ใหม่
```

### binary ปฏิเสธการเริ่มทำงาน: `no public key configured`

มีไฟล์ policy ที่ลงนามแล้วอยู่ แต่ไม่มี key สำหรับตรวจสอบ เลือกทำอย่างใด
อย่างหนึ่ง:

1. binary ไม่ได้ถูก build พร้อม key ที่ฝังไว้ ให้ build ใหม่โดยตั้ง
   `THCLAWS_POLICY_PUBKEY_PATH` **หรือ**
2. วาง public key ไว้ที่ `/etc/thclaws/policy.pub` (หรือ
   `~/.config/thclaws/policy.pub`) แล้ว binary จะหยิบไปใช้ตอน runtime

ข้อสองเหมาะกับ build แบบ open-core ที่ใช้ประเมิน ส่วนข้อหนึ่งคือคำตอบที่
ถูกสำหรับ deployment แบบ EE บน production

### binary ปฏิเสธการเริ่มทำงาน: `binding mismatch`

คุณตั้ง `binding.binary_fingerprint` ไว้ แต่ binary ที่กำลังรันมี
fingerprint ต่างออกไป เป็นไปได้ว่า:

1. binary ที่ deploy ไม่ใช่ตัวที่ policy ผูกไว้ (มี build ใหม่กว่าหรือ
   เก่ากว่าอยู่)
2. fingerprint ถูกคำนวณจากไฟล์คนละตัว (เช่น debug build กับ release build)

ให้คำนวณ fingerprint ของ binary ที่ deploy จริง
(`thclaws-policy-tool fingerprint <path>`) แล้วอัปเดต policy

### `/models refresh` ล้มเหลว

ไม่ใช่ปัญหาของ policy — แคตตาล็อกโมเดลถูกดึงจาก
`https://thclaws.ai/api/model_catalogue.json` ถ้า gateway ของคุณบล็อก
traffic ขาออกไปโดเมนนั้น ให้ทำ mirror ไฟล์ไว้ภายในแล้วตั้ง
`THCLAWS_CATALOGUE_URL` (วางแผนไว้สำหรับ v0.5.0; ตอนนี้ URL ยัง hardcode
อยู่ — เปิด issue ได้ถ้าเรื่องนี้กระทบคุณ)

---

## คำถามที่พบบ่อย

**ถาม: binary ของ Enterprise เป็น closed source หรือเปล่า**
ตอบ: ไม่ โค้ดเหมือน release โอเพนซอร์สทุกประการ ความต่างเดียวคือ public
key ตัวไหนถูกฝังไว้ ไลเซนส์เป็น MIT/Apache-2.0 ทั้งสองแบบ ส่วนเชิงพาณิชย์
คือ **สัญญา support, โครงสร้างการลงนามที่เราดูแลให้, ความช่วยเหลือในการ
deploy และการแพ็กการตั้งค่าเฉพาะลูกค้า** ไม่ใช่ตัวโค้ด

**ถาม: ผู้ใช้ปิด policy ด้วยการแก้ settings.json ได้ไหม**
ตอบ: ไม่ได้ ค่าในไฟล์ policy ทับ `settings.json` สำหรับ key ที่ขัดกัน
ทุกตัว ไม่มีเส้นทางผ่าน config ระดับผู้ใช้ที่จะปิดการบังคับใช้ได้เมื่อมี
policy ที่ตรวจผ่านโหลดอยู่แล้ว

**ถาม: ถ้าผู้ใช้ลบไฟล์ policy จะเกิดอะไรขึ้น**
ตอบ: thClaws ถอยกลับไปเป็นพฤติกรรม open-core ป้องกันได้โดย deploy ไฟล์
policy ด้วยสิทธิ์ระบบไฟล์ที่เหมาะสม (root เป็นเจ้าของ ผู้ใช้อ่านได้อย่าง
เดียว) และใช้เครื่องมือจัดการเครื่องปลายทางตรวจจับและ deploy ไฟล์ที่หาย
กลับไป ตัว binary เองไม่บังคับว่าไฟล์ต้องมีอยู่ — มันบังคับไม่ได้ เพราะ
ไม่มีทางที่ binary จะรู้แบบออฟไลน์ว่า "ตรงนี้ควรมี policy แต่มันหายไป"

นี่คือโมเดลเดียวกับ `/etc/sudoers` หรือไฟล์ config ที่ admin deploy ตัวอื่น:
ระบบไฟล์คือขอบเขตความเชื่อถือ ไม่ใช่ตัว binary

(ถ้าคุณไม่มี MDM ให้ดู
[การแจกจ่ายเมื่อไม่มีระบบจัดการเครื่องปลายทาง](#การแจกจ่ายเมื่อไม่มีระบบจัดการเครื่องปลายทาง)
— การฝัง policy ไว้ใน binary ปิดช่องว่างนี้ได้)

**ถาม: เราต้องการฟีเจอร์ X ที่ไม่มีใน policy block ไหนเลย เพิ่มให้ได้ไหม**
ตอบ: น่าจะได้ เราตั้งใจสร้างฟีเจอร์ EE ไว้ใน open core (ไม่ได้กั้นไว้หลัง
กำแพงเงิน) ความต้องการระดับองค์กรส่วนใหญ่จึงลงเอยเป็น policy block ใหม่
ที่ใครก็ใช้ได้ ส่ง feature request ได้ที่
https://github.com/thClaws/thClaws/issues พร้อมสถานการณ์เฉพาะของคุณ
เราเคย ship ภายในวันเดียวกับคำขอลักษณะนี้มาแล้ว — ดู issue #30 เป็นตัวอย่าง

**ถาม: จะขอ support เชิงพาณิชย์ได้อย่างไร**
ตอบ: อีเมลไปที่ [enterprise@thaigpt.com](mailto:enterprise@thaigpt.com)
พร้อมบริบทของ deployment (ขนาดองค์กร สภาพแวดล้อมเป้าหมาย ข้อกำหนดด้าน
กฎระเบียบ) เราให้บริการ:

- binary ที่ build เฉพาะ พร้อม public key และ branding ของคุณ
- ตั้งโครงสร้างการลงนาม (keygen แบบออฟไลน์ การผูกกับ HSM)
- ความช่วยเหลือในการ deploy (MDM profile, template ตั้งค่า gateway)
- support ที่มี SLA และการตอบ issue แบบมีลำดับความสำคัญ
- พัฒนา policy primitive เฉพาะสำหรับความต้องการที่ไม่เข้ากับ roadmap ของ
  open core

สำหรับการประเมินและทำ PoC ใช้ build โอเพนซอร์สกับขั้นตอนในเอกสารนี้ได้
ครบถ้วน ไม่ต้องมีสัญญา

---

## อ้างอิง: เครื่องมือ

### subcommand ของ `thclaws-policy-tool`

| Subcommand | จุดประสงค์ |
|---|---|
| `keygen --public PUB --private KEY` | สร้าง keypair Ed25519 ใหม่ |
| `sign INPUT --private-key KEY [--output OUT]` | ลงนามไฟล์ policy JSON |
| `verify INPUT --public-key PUB` | ตรวจ policy ที่ลงนามแล้วกับ key |
| `inspect INPUT` | แสดงโครงสร้างของ policy แบบอ่านง่าย |
| `fingerprint BINARY` | คำนวณ SHA-256 ของ binary thClaws |

รัน `thclaws-policy-tool <subcommand> --help` เพื่อดูตัวเลือกทั้งหมด

### Environment variable

| ตัวแปร | จุดประสงค์ | ใช้ตอน |
|---|---|---|
| `THCLAWS_POLICY_PUBKEY_PATH` | เปลี่ยน path ของ pubkey ที่จะฝังตอน build | Build |
| `THCLAWS_POLICY_PUBLIC_KEY` | เนื้อหา pubkey (base64/PEM) สำหรับ override ตอน runtime | Runtime |
| `THCLAWS_POLICY_FILE` | เปลี่ยน search path ของ policy.json | Runtime |
| `THCLAWS_POLICY_FILE_EMBED` | ไฟล์ policy ที่ลงนามแล้ว ให้ฝังไปกับ binary | Build |
| `THCLAWS_REQUIRE_POLICY` | `1` = build นี้ปฏิเสธการเริ่มทำงานถ้าไม่มี policy ที่ไหนเลย | Build |

### Path ที่ระบบไปค้นหา

```
ไฟล์ policy (JSON):
  1. $THCLAWS_POLICY_FILE
  2. /etc/thclaws/policy.json
  3. ~/.config/thclaws/policy.json
  4. (ตัวที่ฝังตอนคอมไพล์ — ถ้ามี)

Public key:
  1. (ฝังตอนคอมไพล์ — เชื่อถือสูงสุด)
  2. $THCLAWS_POLICY_PUBLIC_KEY (เนื้อหาจาก env)
  3. /etc/thclaws/policy.pub
  4. ~/.config/thclaws/policy.pub
```

---

## ติดต่อ

- เรื่องเชิงพาณิชย์ / EE: [enterprise@thaigpt.com](mailto:enterprise@thaigpt.com)
- ปัญหาด้านความปลอดภัย: ดู [SECURITY.md](SECURITY.md) (ช่องทางที่แนะนำคือ
  Private Vulnerability Reporting บน GitHub)
- รายงานบั๊ก / ขอฟีเจอร์แบบสาธารณะ:
  [github.com/thClaws/thClaws/issues](https://github.com/thClaws/thClaws/issues)
- พูดคุยทั่วไป: [github.com/thClaws/thClaws/discussions](https://github.com/thClaws/thClaws/discussions)

thClaws พัฒนาโดย **บริษัท ไทยจีพีที จำกัด (ThaiGPT Co., Ltd.)**
เป็นโอเพนซอร์สภายใต้ MIT/Apache-2.0 ส่วน Enterprise Edition คือห่อเชิง
พาณิชย์บน codebase เดียวกัน
