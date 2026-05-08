# บทที่ 1 — thClaws คืออะไร?

![logo](../user-manual-img/logo/thClaws-logo-line-art-banner.png)

thClaws คือ **workspace สำหรับ AI agent ที่เขียนด้วย Rust แบบ native** ซึ่ง
รันบนเครื่องของคุณเอง ไม่ได้ทำได้แค่เขียนโค้ด แต่ยังแก้โค้ด ทำงาน
อัตโนมัติแทนคุณ ค้น knowledge base ของคุณ และประสานงานทีม agent
หลายตัวได้ — ทั้งหมดรวมอยู่ใน binary เดียว แค่บอกเป็นภาษาธรรมชาติว่า
ต้องการอะไร แล้ว agent จะอ่านไฟล์ รันคำสั่ง ใช้ tool และพูดคุยโต้ตอบ
กับคุณระหว่างทำงาน

สี่ surface รวมอยู่ใน binary เดียว ใช้ `Agent` loop, `Session` และ tool registry ชุดเดียวกัน:

- **Desktop GUI** (`thclaws` โดยไม่ใส่ flag) — หน้าต่าง native ประกอบด้วย
  แท็บ Terminal ที่รัน REPL ตัวเดียวกับโหมด `--cli`, แท็บ Chat แบบ
  streaming, Files browser และแท็บ Team (ตัวเลือกเสริม)
- **CLI REPL** (`thclaws --cli`) — prompt โต้ตอบใน terminal เหมาะกับการใช้
  ผ่าน SSH, เซิร์ฟเวอร์ headless หรือเมื่อไม่ต้องการ overhead ของ GUI
- **โหมดไม่โต้ตอบ** (`thclaws -p "prompt"` รูปแบบเต็มคือ `--print`)
  — รันแค่หนึ่ง turn แล้วออก สะดวกสำหรับสคริปต์ CI pipeline หรือ
  one-liner ใน shell ใส่ `-v` / `--verbose` เพื่อให้แสดง token usage
  ของ turn บน stderr โดยไม่รบกวน stdout
- **Webapp** (`thclaws --serve --port 7878`) — engine ตัวเดียวกันผ่าน
  WebSocket/HTTP เปิดจาก laptop คุณเอง เข้าถึงระยะไกลผ่าน SSH tunnel
  ได้ — "thClaws ทุกที่" โดยไม่ต้องเปิด port

## สิ่งที่ทำให้ thClaws แตกต่าง

- **native บนเครื่อง คุมข้อมูลเอง** — Rust binary ตัวเดียว ไม่ต้องมี service
  เบื้องหลัง ไม่ต้อง cloud จะรันกับ Ollama แบบ offline ล้วน ๆ ก็ได้
- **รองรับหลาย provider อย่างเท่าเทียม** — Anthropic (native + Claude
  Agent SDK), OpenAI (Chat + Responses/Codex), Google Gemini & Gemma,
  Alibaba DashScope (Qwen), DeepSeek, Z.ai (GLM Coding Plan), NVIDIA
  NIM, NSTDA Thai LLM (OpenThaiGPT, Typhoon, Pathumma, THaLLE),
  OpenRouter, Agentic Press, Azure AI Foundry, Ollama (local +
  Anthropic-compat + Cloud), LMStudio และ slot OpenAI-compatible
  ทั่วไป (`oai/*`) — สลับกลางคันด้วย `/model` หรือ `/provider` ได้
  ([บทที่ 6](ch06-providers-models-api-keys.md))
- **เหมาะกับทุกสายงาน ไม่ใช่แค่วิศวกร** — Chat tab แบบ streaming สำหรับ
  นักวิจัย PM ฝ่ายกฎหมาย/การตลาด Terminal REPL สำหรับวิศวกร — ใช้
  session และ config ชุดเดียวกัน สลับไปมาได้โดยไม่เสีย context
  ([บทที่ 4](ch04-desktop-gui-tour.md))
- **ยึดมาตรฐานเปิด ไม่ผูกกับ vendor** — ใช้
  [MCP](https://modelcontextprotocol.io/) สำหรับ tool,
  [`AGENTS.md`](https://agents.md) สำหรับ instruction (มาตรฐานที่
  Google, OpenAI, Cursor, Sourcegraph, Factory ใช้), `SKILL.md` สำหรับ
  workflow และ `.mcp.json` สำหรับตั้งค่า MCP server — config
  ขนย้ายไปใช้กับเครื่องมืออื่นที่พูดมาตรฐานเดียวกันได้
- **ประกอบ agent เองจาก building block** —
  **Skill** ([บทที่ 12](ch12-skills.md)) สำหรับ workflow ที่ใช้ซ้ำได้,
  **MCP server** ([บทที่ 14](ch14-mcp.md)) สำหรับเสียบ tool ภายนอก
  (GitHub, DB, Browser, Slack ฯลฯ),
  **Plugin** ([บทที่ 16](ch16-plugins.md)) สำหรับแพ็กทุกอย่างรวมกัน,
  **Knowledge base** ([บทที่ 9](ch09-knowledge-bases-kms.md)) สำหรับ
  wiki ที่ agent ค้น/อ่าน/เขียนเอง พร้อม `/dream` ที่ consolidate KMS
  จาก session ล่าสุดให้อัตโนมัติ
- **3 ระดับของ agent orchestration** —
  **`Task` tool** (model ตัดสินใจ block parent's turn),
  **`/agent <name>`** (user สั่งเอง รันขนานกับ main ไม่เข้า history),
  **Agent Team** (หลาย process, mailbox + task queue, แต่ละคนมี
  worktree ของตัวเอง) ดู [บทที่ 15](ch15-subagents.md) และ
  [บทที่ 17](ch17-agent-teams.md)
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
- **จำสิ่งที่สำคัญในระยะยาว** — `AGENTS.md` (หรือ `CLAUDE.md`) ในโปรเจกต์
  โดนฉีดเข้า prompt อัตโนมัติ memory store ที่
  `~/.config/thclaws/memory/` เก็บข้อเท็จจริงที่ agent เรียนรู้เกี่ยวกับ
  ตัวคุณและโปรเจกต์ ทั้งหมดเป็น markdown ที่คุณอ่าน แก้ไข หรือ commit ได้
  ([บทที่ 8](ch08-memory-and-agents-md.md))
- **ความปลอดภัยมาก่อน** — filesystem sandbox จำกัดขอบเขตของ tool
  ไฟล์อยู่ที่ working directory tool ที่เปลี่ยนสถานะต้อง approve
  (ยกเว้นจะตั้ง auto-approve เอง) API key เก็บใน OS keychain หรือ
  `.env` ตามที่คุณเลือกตอนเปิดใช้ครั้งแรก permission request จะติดป้าย
  ว่า agent ตัวไหนกำลังขอ (main, side-channel หรือ subagent)
  ป้องกันการ approve ผิดตัว ([บทที่ 5](ch05-permissions.md))
- **Settings อยู่ในไฟล์ JSON ไฟล์เดียว** — permission mode, thinking
  budget, allowed/disallowed tool, endpoint ของ provider, KMS ที่แนบไว้,
  max output tokens รวมอยู่ใน `.thclaws/settings.json` (ระดับโปรเจกต์
  commit ลง repo ได้) หรือ `~/.config/thclaws/settings.json` (ระดับ
  ผู้ใช้ทั้งระบบ)
- **Session resume** — `thclaws --resume last` ทำงานต่อจาก session
  ล่าสุด, `thclaws --resume <id>` กระโดดไป session ที่ระบุ session
  เก็บเป็น JSONL ที่ `.thclaws/sessions/` — git-friendly, grep-friendly
  ([บทที่ 7](ch07-sessions.md))
- **Deploy งานที่สร้างเสร็จได้เลย** — landing page, web app, API และ
  agent ไปอยู่บน [Agentic Press Hosting](https://agentic-press.com)
  (ร่วมมือกับ SIS Cloud Service และ Artech.Cloud) ได้ หรือใช้ host
  อื่นก็ได้ — flow การ deploy มาในรูป plugin host จึงสลับเปลี่ยนได้
  ไม่มีการล็อก client
- **Shell escape** — ใส่ `!` นำหน้าบรรทัดใน REPL เพื่อรันคำสั่ง shell
  โดยตรง ไม่เสีย token ไม่มี prompt ขออนุมัติ (เช่น `! git status`)

## สิ่งที่คุณต้องมี

- OS ที่รองรับ: macOS (arm64 หรือ x86_64), Linux (arm64 หรือ x86_64)
  หรือ Windows (arm64 หรือ x86_64)
- API key ของ LLM อย่างน้อยหนึ่งเจ้า — Anthropic, OpenAI, Gemini,
  OpenRouter, Agentic Press, DashScope, DeepSeek, Z.ai, NVIDIA NIM,
  NSTDA Thai LLM หรือ Azure AI Foundry (หรือจะติดตั้ง Ollama /
  LMStudio บนเครื่องเอง ถ้าต้องการใช้แบบ offline)

[บทที่ 2](ch02-installation.md) จะพาติดตั้งและเปิดใช้ครั้งแรก
[บทที่ 6](ch06-providers-models-api-keys.md) อธิบายว่าจะวาง
key ที่ไหนและอย่างไร

## คู่มือเล่มนี้จัดเรียงอย่างไร

**ส่วนที่ 1** (บทที่ 2–19) คือ reference อธิบายวิธีติดตั้ง ตามด้วยทุก
ฟีเจอร์ที่ผู้ใช้สัมผัสได้ ทีละเรื่อง พร้อมคำสั่งและการตั้งค่าที่จำเป็น
— sessions, memory, KMS, tools, skills, hooks, MCP, subagents,
plugins, teams, plan mode, scheduling

**ส่วนที่ 2** — walkthrough ของโปรเจกต์จริงตั้งแต่เริ่มต้น (landing page
แบบ static, เว็บจอง, agent รวบรวมข่าว จบด้วย deploy ขึ้น Agentic
Press Hosting) — เตรียมไว้สำหรับ revision ในอนาคต

ถ้าเพิ่งเริ่ม อ่านบทที่ 2 ต่อได้เลย ถ้าย้ายมาจาก Claude Code แนะนำให้
ข้ามไปบทที่ 6, 7, 11 และ 13 ถ้าคุ้นเคยพื้นฐานแล้วและสนใจของใหม่ ฟีเจอร์
ที่เพิ่งเพิ่มเข้ามาอยู่ในบทที่ 9 (`/dream`), บทที่ 15 (`/agent`
side-channels), บทที่ 18 (plan mode) และบทที่ 19 (scheduling)
