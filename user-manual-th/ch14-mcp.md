# บทที่ 14 — MCP servers

MCP — **Model Context Protocol** — คือมาตรฐานเปิดของ Anthropic สำหรับ
ให้ LLM เข้าถึงเครื่องมือภายนอก โดย MCP server จะรันเป็น
subprocess (stdio) หรือฟังบน HTTP ก็ได้ จากนั้น thClaws จะค้นหา tools
ของ server นั้นแล้วลงทะเบียนเข้า tool registry โดยใช้ชื่อ server เป็น namespace

## สอง transport

### Stdio (subprocess)

ไบนารี local ที่สื่อสารด้วย JSON-RPC ผ่าน stdin/stdout เป็นรูปแบบ
ที่พบบ่อยที่สุด โดย NPM package ทุกตัวในตระกูล `@modelcontextprotocol/server-*`
ก็ทำงานแบบนี้

```json
{
  "mcpServers": {
    "weather": {
      "command": "npx",
      "args": ["-y", "@h1deya/mcp-server-weather"]
    },
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "ghp_…" }
    }
  }
}
```

### HTTP Streamable (remote)

Server ที่เข้าถึงผ่าน HTTPS สำหรับ server ที่ต้องมีการป้องกัน
thClaws จะจัดการ OAuth 2.1 พร้อม PKCE ให้โดยอัตโนมัติ

```json
{
  "mcpServers": {
    "agentic-cloud": {
      "transport": "http",
      "url": "https://api.agentic.cloud/mcp",
      "headers": { "Authorization": "Bearer …" }
    }
  }
}
```

สำหรับ server ที่ใช้ Bearer token เพียงตั้งค่า `headers` ก็พอ
ส่วน server ที่ป้องกันด้วย OAuth ให้เว้น `headers` ไว้ เมื่อใช้งาน
ครั้งแรก thClaws จะเปิดเบราว์เซอร์ให้ทำ OAuth flow แล้ว cache token ไว้ที่
`~/.config/thclaws/oauth_tokens.json`

## ไฟล์การตั้งค่า

thClaws อ่าน MCP servers จาก (รวมตามลำดับ ถ้าชื่อชนกัน project ชนะ):

1. Plugin manifests — บล็อก `mcpServers`
2. `~/.config/thclaws/mcp.json`
3. `~/.claude/mcp.json` (เข้ากันได้กับ Claude Code)
4. `.thclaws/mcp.json` (ระดับโปรเจกต์)
5. `.claude/mcp.json` (เข้ากันได้กับ Claude Code)

## การเพิ่ม server ขณะรันอยู่

แทนที่จะแก้ JSON ด้วยมือ ให้ใช้ `/mcp add`:

```
❯ /mcp add weather https://mcp.weather-example.com/v1
[mcp-http] weather: probing with ping...
mcp 'weather' added (project, 2 tool(s)) → .thclaws/mcp.json
  - weather__get-forecast
  - weather__get-alerts
```

Server จะถูกบันทึกลง `mcp.json` เชื่อมต่อให้ทันที และลงทะเบียน tools
เข้า session ปัจจุบันโดยไม่ต้อง restart ใช้ได้ทั้ง CLI REPL และ GUI
ทั้งสองแท็บ หากต้องการเขียนลง `~/.config/thclaws/mcp.json` แทน ให้ใช้
`--user`

ลบ:

```
❯ /mcp remove weather
mcp 'weather' removed from .thclaws/mcp.json (restart to drop active tools)
```

## Marketplace

`/mcp marketplace` เปิดดู MCP servers ที่ทีม thClaws คัดสรรไว้
รูปแบบเหมือน skill marketplace — สามคำสั่งสำรวจ + install ด้วยชื่อ:

```
❯ /mcp marketplace
MCP marketplace (baseline 2026-04-29, 1 server(s))
── data ──
  weather-mcp              — Global weather (current + forecast) via Open-Meteo
install with: /mcp install <name>   |   detail: /mcp info <name>
```

```
❯ /mcp info weather-mcp
name:         weather-mcp
description:  Global weather MCP server — current conditions and...
transport:    stdio
command:      python -m thclaws_weather
source:       https://github.com/thClaws/marketplace.git#main:mcp/weather-mcp
note:         Run `pip install -e <clone-path>` to install dependencies...
install with: /mcp install weather-mcp
```

```
❯ /mcp install --user weather-mcp
  cloned https://github.com/thClaws/marketplace.git → ~/.config/thclaws/mcp/weather-mcp
  registered 'weather-mcp' in ~/.config/thclaws/mcp.json (stdio transport)
  note: Run `pip install -e <clone-path>` …
```

สำหรับ entry แบบ stdio ที่มี `post_install_message` ทำตามคำแนะนำ
(โดยทั่วไปเป็น `pip install` / `npm install` สำหรับติดตั้ง runtime
dependencies) ก่อน server จะเริ่มทำงานได้ หลังติดตั้งให้ restart
หรือ reconnect เพื่อโหลด tools ใหม่

สำหรับ entry แบบ hosted (transport `sse`) จะไม่มีการ clone source
— เพียงเขียน entry ใน mcp.json ที่ชี้ไปยัง URL ที่ host ไว้ แล้ว
agent จะ connect ในการเปิด session ครั้งต่อไป

## ดูว่ามีอะไรให้ใช้บ้าง

```
❯ /mcp
  weather (2 tool(s))
    - weather__get-forecast
    - weather__get-alerts
  github (20 tool(s))
    - github__list_issues
    - github__create_issue
    …
```

## การตั้งชื่อ tool

ชื่อ tool ของ MCP ทั้งหมดจะถูกเติมนำหน้าด้วยชื่อ server + `__` เช่น
`weather__get_forecast` ไม่ใช่ `get_forecast` วิธีนี้ช่วยป้องกันการชนกัน
(server สองตัวสามารถมี tool ชื่อ `list` ได้พร้อมกัน) และยังทำให้ deny
tools ของ server ตัวเดียวได้โดยไม่กระทบตัวอื่น

```json
{
  "permissions": {
    "deny": ["github__create_issue"]
  }
}
```

รูปแบบ double-underscore นี้เข้ากันได้กับ tool-name regex ของทุก provider

## การอนุมัติ (Approvals)

MCP tools ทั้งหมดเป็นแบบ **prompt-to-approve** โดยไม่มีวิธีตั้งให้เป็น
auto ยกเว้นจะเปิดแบบ global ผ่าน `/permissions auto` หรือเปิดเป็นรายตัวผ่าน `allow`

## เมื่อสิ่งต่าง ๆ ไม่เป็นไปตามคาด

### Stdio servers ที่เริ่มต้นไม่สำเร็จ

```
[mcp] weather … spawn failed: command not found: npx
```

สาเหตุส่วนใหญ่คือเครื่องไม่มี npm / npx ให้ติดตั้ง Node หรือเปลี่ยนไปใช้ server อื่น
ส่วน thClaws ยังทำงานต่อได้แม้จะไม่มี tools ของ server นั้น

### HTTP servers ที่คืน 200 พร้อม error body

Gateway บางตัวที่เข้ากันได้กับ OpenAI จะห่อ error จาก upstream ไว้ใน
SSE data frame เดียวพร้อมกับ HTTP 200 ซึ่ง parser ของ thClaws จะตรวจจับ
แล้วแสดงเป็น hard error เพื่อไม่ให้หลุดไปแบบเงียบ ๆ หากเห็น
`upstream error: …` ใน output ของ tool แปลว่า remote ทำงานผิดปกติ
ให้แจ้งบั๊กไปยังผู้ดูแล server

### ปัญหาเกี่ยวกับ OAuth flow

Token ที่ cache ไว้ที่ `~/.config/thclaws/oauth_tokens.json` มีวันหมดอายุ
ซึ่ง thClaws จะรีเฟรชให้อัตโนมัติด้วย refresh token แต่หากรีเฟรชไม่สำเร็จ
(เช่น server หมุนเวียน key) ให้ลบรายการของ server นั้นแล้วเชื่อมต่อใหม่
เพื่อให้เกิด browser flow อีกครั้ง

## การเขียน MCP server ของคุณเอง

เนื้อหานี้อยู่นอกขอบเขตของคู่มือ แต่มีจุดเริ่มต้นให้เลือกสองทาง

- **TypeScript**: `@modelcontextprotocol/sdk` บน npm
- **Python**: `modelcontextprotocol` บน PyPI

สเปกอยู่ที่ modelcontextprotocol.io เมื่อสร้างเสร็จแล้ว ให้ลงทะเบียนผ่าน `/mcp
add` (สำหรับ HTTP) หรือแก้ไข `mcp.json` เอง (สำหรับ stdio)
