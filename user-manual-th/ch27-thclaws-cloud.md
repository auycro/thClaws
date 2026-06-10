# บทที่ 27 — thClaws.cloud

thClaws.cloud คือ catalog และ hosted runtime สำหรับ agent ของ
thClaws ทำให้แนวคิด **folder-คือ-agent** (บทที่ 8) กลายเป็นของที่
browse ได้ publish ขึ้น catalog ได้ ติดตั้งลงเครื่องอื่นได้ หรือเช่า
hosted workspace มารันก็ได้ จากมุมของ desktop thClaws การใช้ cloud
จะรู้สึกเหมือน git สำหรับ AI agent — paste CLI token ครั้งเดียวใน
Settings แล้วทุก catalog op (`/cloud get`, `/cloud publish`,
`/cloud list`, …) ทำงานเป็น slash command ภายใน thClaws session

> **ขอบเขตของบทนี้ (ฝั่ง client เท่านั้น).** การ browse catalog การ
> publish agent ของตัวเอง การติดตั้ง agent ลง folder และบล็อก
> `agent.{name, description, uuid}` ใน `settings.json` ส่วน runbook
> สำหรับการรัน catalog server เองอยู่ใน
> [`dev-plan/34`](../dev-plan/34-thclaws-cloud-control-plane.md) และ
> source tree `thclaws-cloud/` ที่ workspace-private

## โมเดล folder-คือ-agent — สรุปคร่าว ๆ

ในทุกที่ที่ thClaws รันได้ **AI agent คือโฟลเดอร์** หนึ่ง โดยที่ราก
ของโฟลเดอร์มี 3 ไฟล์หลัก:

- `AGENTS.md` — คำสั่งของ agent (system prompt + persona)
- `manifest.json` — metadata สำหรับ catalog (slug, license, icon, tag)
  ใช้เฉพาะตอนจะ publish
- `./.thclaws/` — state ภายในเครื่อง (settings, KMS, session, memory)

เวลาคุณ `cd` เข้าไปใน folder นั้นแล้วรัน thClaws คือคุณ "รัน agent
ตัวนั้น" เวลา publish catalog ก็จะแพ็คไฟล์เหล่านี้ทั้งหมดเป็น tarball
เวลาคนอื่นรัน `/cloud get <slug>` จาก session ของเขาเองก็จะได้ folder
เดียวกัน — cloud เป็นแค่ทางขนย้าย folder ระหว่างเครื่อง

## ตั้งค่า URL catalog + CLI token

ของสองอย่างที่ผูก desktop เข้ากับ catalog server:

1. **Cloud URL** — `settings.json::cloud.url` ค่า default คือ public
   instance (`https://thclaws.cloud`) จะ override ไปชี้ที่
   `http://localhost` หรือ self-hosted instance ของตัวเองก็ได้
2. **CLI token** — สตริง `thc_…` จากหน้า dashboard ของ catalog ถูก
   เก็บใน OS keychain (ไม่เคยอยู่ใน `settings.json`)

Settings → **thClaws.cloud** มีช่องให้ใส่ทั้งสองตัว วาง URL วาง token
ที่ได้จากปุ่ม *Mint CLI token* ในหน้า dashboard แล้วกด Save — slash
command ทุกอันในบทนี้ก็พร้อมใช้ทันที

Token ไม่เคยผ่าน shell argument หรือ environment variable เลย — GUI
เก็บลง OS keychain (macOS Keychain / Windows Credential Manager /
Linux Secret Service) ตรง ๆ และทุก request ส่งเป็น Bearer header
จากภายใน engine process ไม่รั่วลง `ps` หรือ shell history

> **ทำไมไม่มี CLI subcommand?** รุ่นก่อนหน้ามี
> `thclaws cloud login --token …` แต่ถูกเอาออกเพราะ token ที่ผ่าน
> `argv` จบลงใน shell history และเครื่องมือ `ps`-style ทุกตัว
> ตอนนี้เหลือ Settings UI + keychain เป็นทางเดียว ถ้ารัน
> `thclaws cloud …` แบบเก่าจะ print error ชี้ไป flow ใหม่

## เปิดดู catalog

จาก thClaws session (REPL หรือ Chat tab):

```
❯ /cloud status
thClaws.cloud — https://thclaws.cloud (token: ✓ stored)

❯ /cloud list
- hello-world           v0.1.0  Hello-world demo agent (jimmy)
- legal-doc-reviewer    v0.4.2  Reviews contracts paragraph-by-paragraph (acme)
- weekly-research       v1.0.0  Saturday-morning newsletter writer (rin)
...

❯ /cloud list --mine
- weekly-research       v1.0.0  Saturday-morning newsletter writer (you)
```

แต่ละแถวคือ agent หนึ่งตัวใน catalog ส่วน slug คือสิ่งที่ต้องส่งให้
`/cloud get`

## ติดตั้ง agent ลง folder

`/cloud get` จะติดตั้งลง **current directory ของ session** เสมอ flow
ปกติ:

```bash
# 1. จาก shell — สร้าง folder ว่างสำหรับ agent แล้ว cd เข้าไป
mkdir my-hello && cd my-hello

# 2. เปิด thClaws session ใน folder นั้น
thclaws            # GUI default หรือ --cli ก็ได้
```

จากนั้นใน session:

```
❯ /cloud get hello-world
Downloading hello-world (v0.1.0) …
Extracted to /Users/jimmy/my-hello/
  ✓ AGENTS.md
  ✓ manifest.json
  ✓ skills/greet.md
Done. /reload to pick up the new AGENTS.md.
```

engine จะดาวน์โหลด tarball แตกไฟล์ทั้งหมดลง cwd แล้ว `/reload` ครั้ง
ถัดไปจะอ่าน `AGENTS.md` ใหม่ ไม่ต้องออกไป shell อีกรอบ

### กลไก folder-safety

`cloud get` จะไม่ยอม overwrite folder ที่ไม่ว่าง ยกเว้นจะเป็น agent
ตัว **เดียวกัน** (match ด้วย UUID ดูด้านล่าง) หรือใส่ `--force` กฎ
เป็นแบบนี้:

| สถานะของ folder ปลายทาง | พฤติกรรม |
|---|---|
| ว่าง | ติดตั้งใหม่ |
| มี `AGENTS.md` / `manifest.json` และ `agent.uuid` ตรงกัน | อัปเดตทับได้ — เก็บ `.thclaws/` session state เดิมไว้ |
| มี `AGENTS.md` / `manifest.json` แต่ UUID **ไม่ตรง** | abort พร้อม error — folder นี้เป็นของ agent อื่น |
| มีไฟล์อื่น ๆ ที่ไม่เกี่ยวข้อง (note, scratch ฯลฯ) | abort ยกเว้นจะใส่ `--force` |

ออกแบบไว้แบบนี้โดยตั้งใจ — กันพิมพ์ผิดแล้วเขียนทับงานที่ทำค้างไว้
หรือ agent ของคนอื่นใน directory เดียวกัน

## Publish agent

เวลาคุณสร้าง agent ใน folder หนึ่งและอยากให้มันขึ้น catalog ให้เปิด
thClaws session ใน folder นั้น แล้วใช้ slash command:

```
❯ /cloud publish              # อัปโหลด cwd
❯ /cloud publish --dry-run    # preview เนื้อหา tarball ไม่อัปโหลด
```

`/cloud publish` ทำ 3 อย่าง:

1. **Tar + gzip** folder ทั้งก้อน — secret, session, KMS page และ
   directory `./.thclaws/` ถูกตัดออกอัตโนมัติ คุณ re-publish ทุกวันได้
   โดยไม่ทำให้ประวัติแชทรั่ว
2. **Upload** ขึ้น catalog ด้วย CLI token ของคุณ
3. **Stamp identity ของ agent กลับลง `settings.json`** (ดูหัวข้อถัดไป)

ถ้า `manifest.json` หายหรือ invalid `publish` จะ abort พร้อม error
ที่ชัด minimum field ที่ต้องมี: `id`, `name`, `description`, `version`

## บล็อก agent identity ใน `settings.json`

บล็อก top-level `agent` ใน `./.thclaws/settings.json` เก็บ identity
ของ folder นี้บน catalog:

```json
{
  "agent": {
    "id": "my-research-bot",
    "name": "My Research Bot",
    "description": "Saturday-morning newsletter writer",
    "uuid": "1f9c1d70-3a26-43c4-9c40-1b1b6e3e3a01"
  }
}
```

- **id / name / description** — คัดลอกมาจาก `manifest.json` ตอน
  publish ใช้โดย catalog UI และโดย safety check ของ `/cloud get`
- **uuid** — assign โดย catalog ครั้ง **แรก** ที่ publish จาก folder
  นี้ แล้วเขียนกลับลง `settings.json` ครั้งต่อไปที่ publish จะไปลง
  catalog row เดิม (เพิ่ม version) UUID คือสิ่งที่ `/cloud get` ใช้
  match ว่า "folder นี้คือ agent ตัวเดียวกันมั้ย"

ปกติไม่ต้องแก้บล็อกนี้เอง GUI Settings → **Agent identity** มี
panel ให้แก้ `name` / `description` (สะดวกก่อน publish — description
จะไปโผล่ในรายการ catalog) แต่ตั้งใจซ่อน `uuid` ไว้

### Fork agent ที่ดาวน์โหลดมา

ถ้า `/cloud get` agent ของคนอื่นมาแล้วอยาก fork ในชื่อตัวเอง จาก
session ของ folder agent นั้น:

```
❯ /cloud unbind            # ล้าง settings.json::agent.uuid
❯ # ใน session เดิม: แก้ AGENTS.md, manifest.json — เปลี่ยน `id` เป็นชื่อที่ว่าง
❯ /cloud publish           # ได้ UUID ใหม่
```

ถ้าไม่ `/cloud unbind` ก่อน publish ครั้งต่อไปจะพยายาม update catalog
row ของเจ้าของเดิม (และจะ fail ด้วย permission error — catalog gate
การ publish ตาม author)

## Hosted workspace (เช่าแทนที่จะติดตั้ง)

ถ้าไม่อยากติดตั้ง agent บนเครื่อง laptop ตัวเอง catalog ก็รัน agent
เป็น **hosted workspace** ให้ได้ — หนึ่ง container ต่อหนึ่ง
workspace มี URL ให้เปิดในเบราว์เซอร์ มี chat UI จริงที่ backend ใช้
engine ตัวเดียวกับที่คุณรันใน local

จาก web UI ของ catalog:

1. browse ไปหน้า detail ของ agent
2. กด *Install on hosted*
3. catalog จะ spin up workspace คัดลอกไฟล์ของ agent เข้าไป แล้ว
   redirect ไป chat UI ที่ `/u/<handle>/<slug>/`

Hosted workspace รองรับทั้ง BYOK (วาง provider key เองที่ *Settings
→ Hosted keys*) และ **thClaws.cloud gateway** (proxy แบบ pay-per-use
ที่มี credit billing ดูด้านล่าง) ตอนสร้าง workspace มี radio
toggle ให้เลือก

## Gateway แบบ pay-per-use (ทางเลือกแทน BYOK)

สำหรับผู้ใช้ที่ไม่อยาก manage account ของ Anthropic / OpenAI / Gemini
เอง thClaws.cloud มี **gateway** ให้ — เติม credit ครั้งเดียวแล้ว
เรียก model อะไรก็ได้ผ่าน `gateway.thclaws.cloud/<provider>/...` โดย
ใช้ token `gw_v1_…` Gateway จะ forward ไป upstream meter response
แล้วหักจาก balance

วิธีใช้ gateway จาก thClaws **desktop**:

1. mint gateway access key ใน catalog UI: **/gateway/keys** → *Mint
   new gateway key* → copy สตริง `gw_v1_…`
2. เติม credit: **/credit** → เลือก pack ($5 / $20 / $100) pack ใหญ่
   มี bonus credit
3. ตั้งให้ thClaws ชี้ไปที่ gateway:
   ```bash
   export ANTHROPIC_API_KEY=gw_v1_…
   export ANTHROPIC_BASE_URL=https://thclaws.cloud/gateway/anthropic
   export OPENAI_API_KEY=gw_v1_…
   export OPENAI_BASE_URL=https://thclaws.cloud/gateway/openai/v1
   # …ทำเหมือนกันกับ GEMINI_*, OPENROUTER_*
   ```
   (หรือใช้ช่อง `*_API_KEY` / `*_BASE_URL` ใน GUI Settings →
   Providers ก็ได้)
4. รัน thClaws ตามปกติ call จะไปผ่าน gateway ค่าใช้จ่ายโผล่ใน
   **/credit/usage**

สำหรับ workspace **hosted** gateway จะถูก wire ให้อัตโนมัติเมื่อเลือก
*Gateway* ตอนสร้าง workspace — runner จะได้ env var ที่ inject ให้
แล้วโดยไม่ต้อง copy-paste

### Tier gating ของ model

Model ถูกแบ่งเป็น 3 tier — `starter`, `pro`, `enterprise` ค่า
`model_tier` ของ account (ตั้งใน catalog dashboard) ควบคุมว่า gateway
จะยอมรับ model ใดบ้าง Account starter จะได้ Haiku / gpt-4o-mini /
Gemini Flash ส่วนการเรียก Sonnet ด้วย starter account จะคืน `403`
จาก gateway พร้อมลิงก์ upgrade Tier กับ balance แยกกัน — มี credit
$100 ก็ไม่ได้ปลด enterprise model ให้ starter account

## สรุปอ้างอิงคำสั่ง

catalog op ทุกอันรันใน thClaws session ที่เปิดอยู่ — ถ้ารัน
`thclaws cloud …` แบบ CLI เก่าทุกตัวจะ print error ชี้ไป
slash-command equivalent

| คำสั่ง | ที่ใช้ | ทำอะไร |
|---|---|---|
| Settings → **thClaws.cloud** | GUI | Cloud URL + CLI token (paste / clear) — ทางเดียวที่ใช้ login/logout |
| `/cloud status` | In-session slash | แสดง URL ที่ resolve + state ของ token |
| `/cloud list [--mine]` | In-session slash | browse catalog |
| `/cloud get <slug> [--force]` | In-session slash | ติดตั้งลง cwd ของ session |
| `/cloud publish [--dry-run]` | In-session slash | อัปโหลด cwd ของ session |
| `/cloud unbind` | In-session slash | ล้าง `agent.uuid` ให้ publish ครั้งต่อไปสร้าง row ใหม่ใน catalog |
| Settings → **Agent identity** | GUI | แก้ `agent.name` / `description` ของ folder นี้ |
| Settings → **Agent identity** | GUI | แก้ `agent.name` / `description` ของ folder นี้ |
| `/credit` (web) | Catalog UI | เติม credit + ดู balance + ดูราคา model |
| `/gateway/keys` (web) | Catalog UI | mint access key `gw_v1_…` |
| `/credit/usage` (web) | Catalog UI | ค่าใช้จ่ายรายการ + แยกตาม workspace |

## thClaws.cloud ไม่ใช่อะไร

ตั้งความคาดหวังเรื่องสำคัญสองสามข้อ:

- **ไม่ใช่ที่ host model** Agent ใน catalog ยังคงเรียก inference จาก
  Anthropic / OpenAI / Gemini อยู่ — ผ่านทั้ง BYOK key ของคุณเอง หรือ
  cloud gateway ในฐานะ proxy เก็บเงิน thClaws.cloud ไม่ได้ train หรือ
  serve LLM เอง
- **ไม่ใช่ที่เก็บ session** ประวัติแชทยังคงอยู่ใน
  `./.thclaws/sessions/` บนเครื่องที่ run agent ตัวนั้น cloud เก็บ
  ไฟล์ agent ไม่ใช่ประวัติบทสนทนา
- **ไม่จำเป็นต้องใช้** ทุกบทก่อนหน้าบทนี้ทำงานได้โดยไม่ต้องใช้
  network เลย cloud เป็นของเสริม — ติดตั้ง thClaws เขียน `AGENTS.md`
  ก็ได้ agent ใช้งานได้แล้วโดยไม่ต้องสมัครอะไร
