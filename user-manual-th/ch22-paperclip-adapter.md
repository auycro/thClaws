# บทที่ 22 — Paperclip adapter

จ้าง agent ของ thClaws ใน [Paperclip](https://paperclip.ai) ให้ทำงาน
ร่วมกับ adapter ตัวอื่น ๆ ที่มากับ Paperclip (Claude, Codex, Cursor,
Gemini ฯลฯ) เพียงตั้งค่า adapter หนึ่งบล็อกพร้อม model id เดียว
Paperclip ก็จะส่งงานให้ thClaws ใช้ provider, KMS, skill, MCP และ
agent team ของมันได้เต็มชุด

shipped ครั้งแรกใน v0.9.5 เป็น package npm แยกต่างหาก —
[`@thclaws/paperclip-adapter`](https://www.npmjs.com/package/@thclaws/paperclip-adapter)
ไม่ได้ bundle มาในไบนารี desktop

## ทำไมต้องใช้

- **Agent ที่สลับ provider ได้ใน Paperclip** สลับระหว่าง Anthropic /
  OpenAI / Gemini / OpenRouter / DashScope / Codex subscription /
  15+ ราย โดยเปลี่ยนแค่ field `model` ใน config ของ agent ไม่ต้องเพิ่ม
  adapter Paperclip แยกต่อ provider
- **บิล Codex ผ่าน subscription** model id `chatgpt-codex/*` route ผ่าน
  บัญชี ChatGPT Plus / Pro / Team เดิมของคุณ (auto-import auth.json
  จาก Codex CLI) ไม่ต้องใช้ OpenAI API key เพิ่ม
- **ของแถมจาก thClaws ฟรี ๆ** ทุก run ใน Paperclip จะมี KMS,
  plan-mode, agent team, skill, MCP server และระบบ approval ของ thClaws
  พร้อมใช้งานโดยไม่ต้อง config เพิ่มต่องาน — ใช้สิ่งที่ตั้งไว้แล้วใน
  `.thclaws/` ของโปรเจกต์ได้เลย

## เมื่อไหร่ที่ไม่ควรใช้

- งาน Paperclip ที่ต้องใช้ tool surface ของ Claude Code เฉพาะ (ใช้
  adapter `claude_local` แทน) หรือ session model ของ Codex CLI (ใช้
  `codex_local`) — tool registry ของ thClaws ไม่ข้าม subprocess
  boundary ของ wrapper เหล่านั้น
- ต้องการ **session แบบ multi-turn ต่อเนื่องข้าม run** เพราะ MVP
  v0.1 wrap แค่ print mode ของ thClaws (`thclaws -p`) — แต่ละ run
  ของ Paperclip จะแยกกัน session continuation จะมาเมื่อ thClaws ส่ง
  format `--output-format stream-json` ออก

## สิ่งที่ต้องเตรียม

1. **Paperclip ที่รองรับ external adapter plugin** — การเปลี่ยนแปลง
   `adapter-plugin` Phase 1 ดู docs ของ Paperclip ของคุณ
2. **ไบนารี `thclaws` อยู่ใน `$PATH`** (หรือระบุ path เต็มใน config)
   ติดตั้งด้วย:
   ```sh
   git clone https://github.com/thClaws/thClaws
   cd thClaws/crates/core && cargo install --path .
   ```
3. **API key ของ provider อย่างน้อยหนึ่งตัว** ที่ thClaws อ่านได้ —
   ผ่าน shell env หรือ `~/.config/thclaws/.env` หรือ `.thclaws/.env`
   ของโปรเจกต์ adapter ไม่ได้ดูแล credential ให้ thClaws มันแค่ spawn
   binary

## ติดตั้ง

ใน Paperclip instance ของคุณ:

```sh
pnpm add @thclaws/paperclip-adapter
```

จากนั้นลงทะเบียนผ่าน plugin store ของ Paperclip (ขั้นตอนรายละเอียดอยู่
ใน docs ของ Paperclip เองหัวข้อ adapter plugins)

## จ้าง agent ของ thClaws

config ขั้นต่ำ:

```json
{
  "adapterType": "thclaws_local",
  "model": "claude-sonnet-4-6"
}
```

แค่นั้น UI ของ Paperclip มีรายการ model เด่น ๆ ให้เลือก (Claude
Sonnet 4.6, Claude Opus 4.7, GPT-4o, Codex GPT-5.4, Qwen variant,
Gemini variant, OpenRouter variant) แต่จะพิมพ์ model id ใด ๆ ที่
`ProviderKind::detect` ของ thClaws รู้จักก็ได้ เช่น
`openrouter/anthropic/claude-3.5-sonnet`, `dashscope/qwen-max`,
`gemini-2.5-flash`, `chatgpt-codex/gpt-5.4` เป็นต้น

## Field ใน agent config

| Field | Type | Default | หมายเหตุ |
|---|---|---|---|
| `adapterType` | string | required | ต้องเป็น `"thclaws_local"` |
| `model` | string | `claude-sonnet-4-6` | model id ใด ๆ ที่ thClaws รู้จัก |
| `cwd` | string | workspace ของ Paperclip | working directory แบบ absolute สำหรับ process ของ thClaws |
| `command` | string | `thclaws` | override path ของ binary เผื่อกรณีติดตั้งไว้ที่ prefix แปลก ๆ |
| `extraArgs` | string[] | `[]` | argument ที่ต่อท้าย spawn `thclaws -p` เช่น `["--max-tokens", "8000"]` |
| `env` | object | `{}` | env var ต่อ agent ใส่ `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `DASHSCOPE_API_KEY` ที่นี่ แทนการพึ่ง shell ของ host — ระบบ discovery `.env` ของ thClaws จะ layer ทับให้ |
| `promptTemplate` | string | none | template optional ที่ใช้กับ prompt ที่ Paperclip ส่งมาก่อนเข้า `thclaws -p` |
| `timeoutSec` | number | `0` (ไม่มี timeout จาก adapter) | timeout ต่อ run เป็นวินาที — timeout ระดับ job ของ Paperclip ยังคงทำงาน |

## สิ่งที่ agent เข้าถึงได้

ในทุก run ของ Paperclip ที่จ้าง agent `thclaws_local` thClaws จะได้
stack เต็มของมันตามปกติ:

- **Permission policy** อ่านจาก `.thclaws/settings.json` ของ
  workspace (หรือ `~/.config/thclaws/settings.json` เป็น fallback)
  job runner ของ Paperclip ไม่ได้ auto-approve tool ที่เปลี่ยนแปลง
  state อัตโนมัติ — ถ้าอยากให้ run ผ่านโดยไม่ต้อง approve ให้ตั้ง
  `"permissions": "auto"` ใน project settings (ดูบทที่ 5)
- **MCP server** ที่ผูกไว้ระดับโปรเจกต์ (`.thclaws/mcp.json`) หรือ
  ระดับ user (`~/.config/thclaws/mcp.json`) ใช้งานได้เลย ไม่ต้อง
  config เพิ่ม (ดูบทที่ 14)
- **Skill, KMS, hook, agent team** — เหมือนการรัน CLI standalone
  process ของ thClaws รันจนเสร็จแล้วออก

Output จับจาก stdout / stderr ตรง ๆ thClaws print ข้อความของ
assistant พร้อมบรรทัด `[tokens: …]` สรุปท้ายให้ ทั้งคู่ flow กลับเข้า
Paperclip ในชื่อ transcript ของ run

## ข้อจำกัด (v0.1 MVP)

- **ไม่มี session ต่อเนื่องข้าม run** ทุก run ของ Paperclip เป็น
  one-shot ของ `thclaws -p` ไม่มี `--resume` ระหว่าง run
- **ไม่มีการ render tool-call แบบ incremental** stdout buffer
  จนกว่า process จะออก แล้วถึงโผล่เป็น transcript block เดียว
- **Adapter ไม่ได้ดูแล credential ของ thClaws** API key มาจาก env
  var, ไฟล์ `.env` หรือ OS keychain — ตามที่ระบบ lookup ของ thClaws
  หาเจอ

ทั้ง rendering และ resume จะมาเมื่อ thClaws ส่ง `--output-format
stream-json` ออก ติดตามที่ [repo ของ adapter](https://github.com/thClaws/paperclip-adapter)

## ดูเพิ่มเติม

- [บทที่ 6 — Provider, โมเดล และ API key](ch06-providers-models-api-keys.md)
- [บทที่ 14 — MCP server](ch14-mcp.md)
- [บทที่ 17 — ทีมของ Agent](ch17-agent-teams.md)
- Technical manual:
  [`paperclip-adapter.md`](../thclaws-technical-manual/paperclip-adapter.md)
  สำหรับ contract ภายในของ adapter และ semantics ของการ spawn
