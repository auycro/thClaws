# บทที่ 9 — Knowledge bases (KMS)

**knowledge base** (KMS — Knowledge Management System) คือโฟลเดอร์ของ markdown page ที่คุณดูแลเอง พร้อมกับ `index.md` ที่ทำหน้าที่เป็นสารบัญซึ่ง agent อ่านทุก turn แนวคิดนี้ได้แรงบันดาลใจมาจาก [LLM wiki pattern](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f) ของ Andrej Karpathy โดย thClaws ใส่ KMS มาให้ในตัวอยู่แล้ว ไม่มี embeddings ไม่มี vector store มีแค่ grep กับ read

Use case:

- **บันทึกส่วนตัว** — ทุกสิ่งที่คุณเรียนรู้เกี่ยวกับ API, library หรือ codebase ของลูกค้า
- **เอกสารอ้างอิงของโปรเจกต์** — architectural decision, design principle และ pattern ที่เฉพาะเจาะจงกับ repo นั้น ๆ
- **Playbook ของทีม** — standard operating procedure หรือ checklist สำหรับ onboarding
- **เนื้อหาเฉพาะภาษา** — รองรับภาษาไทยได้ทันทีตั้งแต่เริ่ม เพราะการค้นหาทำงานผ่าน Grep เป็นหลัก

## แตกต่างจาก memory หรือ AGENTS.md อย่างไร

| | ขอบเขต | ขนาด | การค้นข้อมูล |
|---|---|---|---|
| **AGENTS.md** | inject ข้อความเต็มทุก turn | เล็ก (ไม่กี่ KB) | ไม่ต้องค้น เพราะอยู่ใน prompt อยู่แล้ว |
| **Memory** | ข้อเท็จจริงแยกตามชนิด | เล็ก (index + body refs) | frontmatter ทำ index ไว้ให้ แล้วค่อยดึง body เมื่อจำเป็น |
| **KMS** | wiki ทั้งชุด โหลดแบบ lazy | ไม่จำกัด (เป็นพัน page ก็ไหว) | ใช้ Grep ค้น แล้วอ่านเฉพาะ page ที่ต้องการ |

หลักคร่าว ๆ คือ memory ไว้เก็บเรื่องเกี่ยวกับ *ตัวคุณ* และ *วิธีทำงานของคุณ* ส่วน AGENTS.md ไว้เก็บ convention ของโปรเจกต์ ขณะที่ KMS ไว้เก็บ *เนื้อหา* ที่ agent จะเข้าไปเปิดดู

## Scope

มีสอง scope ที่มีโครงสร้างภายในเหมือนกัน

- **User** — `~/.config/thclaws/kms/<name>/` — ใช้ได้ในทุกโปรเจกต์
- **Project** — `.thclaws/kms/<name>/` — อยู่กับ repo และตามไปกับ git ถ้าถูก track ไว้

หากมีชื่อซ้ำกันทั้งสอง scope ฝั่ง **project** จะถูกเลือกใช้ก่อน

## Layout ของ KMS directory

```
<kms_root>/
├── index.md      ← table of contents, one line per page. The agent reads this every turn.
├── log.md        ← append-only change log (humans + agent write here)
├── SCHEMA.md     ← optional: shape rules for pages
├── pages/        ← individual wiki pages, one per topic
│   ├── auth-flow.md
│   ├── api-conventions.md
│   └── troubleshooting.md
└── sources/      ← raw source material (URLs, PDFs, notes) — optional
```

`/kms new` จะสร้างทุกอย่างข้างบนให้พร้อมเนื้อหา starter เล็ก ๆ เพื่อให้คุณเริ่มเขียนต่อได้ทันที

## Multi-KMS: ผูก KMS ชุดใดก็ได้เข้ากับการสนทนา

รายการ KMS ที่ active ของโปรเจกต์อยู่ใน `.thclaws/settings.json`:

```json
{
  "kms": {
    "active": ["notes", "client-api", "team-playbook"]
  }
}
```

`index.md` ของ KMS ที่ active ทุกตัวจะถูกนำมาต่อกันใน system prompt ภายใต้หัวข้อ `## KMS: <name>` พร้อม pointer ชี้ไปยัง tool `KmsRead` และ `KmsSearch` สิ่งที่ agent เห็นจะมีหน้าตาแบบนี้

```
# Active knowledge bases

The following KMS are attached to this conversation. Their indices are below —
consult them before answering when the user's question overlaps.

## KMS: notes (user)

# notes
- [auth-flow](pages/auth-flow.md) — JWT refresh pattern we use
- [api-conventions](pages/api-conventions.md) — REST style guide

To read a specific page, call `KmsRead(kms: "notes", page: "<page>")`.
To grep all pages, call `KmsSearch(kms: "notes", pattern: "...")`.
```

พร้อมทั้งลงทะเบียน `KmsRead` และ `KmsSearch` ไว้ในรายการ tool ให้ด้วย

## Slash commands

### `/kms` (หรือ `/kms list`)

แสดงรายการ KMS ทั้งหมดที่ค้นพบ โดยมี `*` กำกับไว้หน้าตัวที่ผูกกับโปรเจกต์ปัจจุบัน

```
❯ /kms
* notes              (user)
  client-api         (project)
* team-playbook      (user)
  archived-docs      (user)
(* = attached to this project; toggle with /kms use | /kms off)
```

### `/kms new [--project] NAME`

สร้าง KMS ใหม่พร้อมไฟล์ starter ให้ในตัว

```
❯ /kms new meeting-notes
created KMS 'meeting-notes' (user) → /Users/you/.config/thclaws/kms/meeting-notes

❯ /kms new --project design-decisions
created KMS 'design-decisions' (project) → ./.thclaws/kms/design-decisions
```

- scope ดีฟอลต์คือ **user** (ใช้ได้ในทุกโปรเจกต์)
- ใส่ `--project` เพื่อให้ไปอยู่ใน `.thclaws/kms/` (ติดไปกับ repo)

### `/kms use NAME`

ผูก KMS เข้ากับโปรเจกต์ปัจจุบัน ระบบจะลงทะเบียน tool `KmsRead` /
`KmsSearch` เข้า session ทันที พร้อมแทรก `index.md` เข้า system
prompt — ไม่ต้อง restart ใช้ได้ทั้ง CLI REPL และ GUI ทั้งสองแท็บ

```
❯ /kms use notes
KMS 'notes' attached (tools registered; available this turn)
```

### `/kms off NAME`

ถอด KMS ออก มีผลทันทีเช่นกัน — เมื่อถอด KMS ตัวสุดท้ายออก `KmsRead` /
`KmsSearch` จะถูกลบจาก registry เพื่อไม่ให้ model เห็นเป็นทางเลือก

```
❯ /kms off archived-docs
KMS 'archived-docs' detached (system prompt updated)
```

### `/kms show NAME`

พิมพ์ `index.md` ของ KMS ออกมาให้ดูว่ามีอะไรบ้าง

```
❯ /kms show notes
# notes
- [auth-flow](pages/auth-flow.md) — JWT refresh pattern we use
- [api-conventions](pages/api-conventions.md) — REST style guide
...
```

## Sidebar (GUI)

ส่วน **Knowledge** ของ sidebar จะแสดง KMS ทุกตัวที่ค้นพบ พร้อม checkbox ให้ทุกรายการ ติ๊กเพื่อผูก เอาติ๊กออกเพื่อถอด ซึ่งก็คือ toggle เดียวกับ `/kms use` และ `/kms off` นั่นเอง

ปุ่ม `+` จะถามชื่อก่อนแล้วจึงถาม scope (OK = user, Cancel = project) จากนั้นจะสร้าง KMS ใหม่พร้อมไฟล์ starter ที่เปิดแก้ไขต่อได้ทันที

## Tool ที่ agent เรียกใช้

### `KmsRead(kms: "name", page: "slug")`

อ่าน `<kms_root>/pages/<slug>.md` โดยเติมนามสกุล `.md` ให้เองหากไม่ใส่มา หากมีการพยายาม path traversal จะถูกปฏิเสธ (`..`, absolute path หรืออะไรก็ตามที่อยู่นอก `pages/`)

agent จะเรียก tool นี้เมื่อเห็นรายการที่เกี่ยวข้องใน `index.md`

```
[assistant] I'll check the auth-flow page first…
[tool: KmsRead(kms: "notes", page: "auth-flow")]
[result] (page content)
```

### `KmsSearch(kms: "name", pattern: "regex")`

สแกนแบบ grep ครอบคลุม `<kms_root>/pages/*.md` ทั้งหมด แล้วคืนบรรทัดที่ตรงในรูปแบบ `page:line:text` หนึ่งรายการต่อบรรทัด

```
[assistant] Let me search for "bearer" across my notes…
[tool: KmsSearch(kms: "notes", pattern: "bearer")]
[result]
auth-flow:12:Bearer tokens expire after 15 minutes
api-conventions:34:Always include "Authorization: Bearer <token>"
```

### `KmsWrite`, `KmsAppend`, `KmsDelete`

Surface สำหรับ mutate KMS ที่ agent (และ `/dream` consolidator ด้านล่าง) ใช้ ทั้งสามตัวต้องการ approval โดย default

- `KmsWrite(kms, page, content)` — สร้างหรือเขียนทับ page รักษา YAML frontmatter ไว้, bump `updated:`, อัปเดต bullet ใน `index.md`, append `wrote | <page>` เข้า `log.md`
- `KmsAppend(kms, page, content)` — ต่อท้าย page ที่มีอยู่ เร็วกว่า `KmsWrite` สำหรับการอัปเดตทีละนิด (log, journal, accumulating notes) bump `updated:` ถ้า page มี frontmatter
- `KmsDelete(kms, page)` — ลบ page, ตัด bullet ออกจาก `index.md`, append `deleted | <page>` ใน `log.md` ใช้ตอน consolidate เพื่อปลด page ที่ซ้ำหรือล้าสมัย

ชื่อ page จะถูก validate เป็น path-segment — ไม่มี separator, ไม่มี traversal, และชื่อสงวน `index`, `log`, `SCHEMA` ใช้เป็นชื่อ page ไม่ได้ (KMS เป็นคนจัดการเอง)

## การ Consolidate ด้วย `/dream`

หลังจากทำงานไปไม่กี่สัปดาห์ KMS จะมี duplicate สะสม: page สอง page ที่พูดเรื่องเดียวกันแต่เนื้อหาไหลออกจากกัน, ข้อมูลเก่าที่ขัดกับสิ่งที่คุณพูดเมื่อวาน, insight จาก session ที่ไม่เคยถูกบันทึกเป็น page **`/dream`** คือ slash command ที่แก้ปัญหานี้ — มัน dispatch built-in `dream` agent เป็น side channel (บทที่ 15) ซึ่ง consolidate KMS ของ project ใน background ขณะที่คุณทำงานอื่นต่อได้

```
/dream                 # consolidate ทุกอย่าง
/dream auth            # ให้ bias ไปทาง topic "auth"
/agents                # ดู dream ที่ active + เริ่มเมื่อไหร่
/agent cancel <id>     # หยุด dream ที่ออกนอกเรื่อง
```

`/dream` ใช้ได้เฉพาะใน GUI (ต้องใช้ chat surface ในการ render side bubble) dream agent รันแบบ concurrent กับ main คุณจึงสั่ง main ต่อได้ระหว่าง dream ทำงาน

### มันทำอะไร

dream agent รัน 4 pass:

1. **Survey** — อ่านรายการ active KMS (จาก system prompt ของตัวเอง) และ `index.md` ของแต่ละ KMS เพื่อ enumerate page ที่มีอยู่
2. **Read sessions** — `Glob` หา 10 ไฟล์ล่าสุดใน `.thclaws/sessions/*.jsonl` แล้วอ่าน แต่ละ session คือ JSONL ของ message events; agent สแกนหาข้อเท็จจริงที่เสถียรซึ่ง user ได้สรุปไว้แล้วแต่ยังไม่อยู่ใน KMS
3. **Consolidate** — สำหรับแต่ละ insight, มันจะ `KmsSearch` ใน KMS ที่เกี่ยวข้องก่อน; ถ้ามี page ครอบคลุม topic อยู่แล้ว จะ `KmsAppend` แทนการสร้างใหม่ ถ้า page สองตัว overlap หนัก จะ merge ผ่าน `KmsWrite` แล้ว `KmsDelete` ตัวที่ซ้ำ
4. **Summarize** — เขียน page `dream-YYYY-MM-DD.md` ใน project KMS ลิสต์ทุกการเปลี่ยนแปลง (page ที่เพิ่ม, อัปเดต, ลบ, รวมถึง insight ที่ข้ามและเหตุผล) นี่คือ audit trail ของคุณ

```
❯ /dream
✓ dreaming (id: side-9c4f1e)

[dream] surveying 2 active KMS (project-knowledge, scratch)…
[dream] reading 10 most recent sessions…
[dream] consolidating project-knowledge:
[dream]   appended 4 lines to auth-flow.md
[dream]   merged old-deployment.md into deployment.md, deleted old-deployment.md
[dream]   added 2 new pages: tracing-conventions.md, kafka-topics.md
[dream] writing dream-2026-05-07.md…
[dream] ✓ done in 3m12s. See dream-2026-05-07.md for the change log.
```

### การ review ผลลัพธ์

dream agent รันด้วย `permission_mode: auto` — แก้และลบ page ได้โดยไม่ถาม **ขั้นตอน review คือ `git diff`** ถ้า project KMS ของคุณอยู่ใต้ git (ซึ่งควรจะอยู่ — `.thclaws/kms/` ก็แค่ markdown):

```bash
git diff .thclaws/kms/                        # ดูว่าเปลี่ยนอะไร
git checkout -- .thclaws/kms/                 # ทิ้งงานของ dream
git add .thclaws/kms/ && git commit -m "..."  # รับงาน
```

หน้า `dream-YYYY-MM-DD.md` คือคำอธิบายของ agent เองว่าทำอะไรไปบ้าง — อ่านอันนี้ก่อน แล้วค่อย spot-check diff ที่สำคัญ ถ้า summary บอกว่า "no new insights" และเขียน stub page นั่นคือ no-op outcome ที่ valid เช่นกัน

### การ customize

built-in dream agent shipped อยู่ใน binary (system prompt + tool whitelist) คุณ override ได้ที่ระดับ project โดยสร้าง `.thclaws/agents/dream.md` พร้อม frontmatter และคำสั่งของคุณเอง — ตัว disk ชนะ built-in เสมอ ใช้ได้ถ้าทีมคุณมีนโยบาย KMS curation เฉพาะ (เช่น "ห้ามลบ page ที่ tag `archive: keep`")

dream agent default ใช้ tool: `KmsRead, KmsSearch, KmsWrite, KmsAppend, KmsDelete, Read, Glob, Grep, TodoWrite` — ไม่มี `Bash`, ไม่มี `Edit`/`Write` กับ project source, ไม่มี `Memory*` มัน modify ได้แค่ KMS เท่านั้น

## การเขียน page: workflow การ ingest

ไม่ต้องมี tool พิเศษสำหรับเพิ่มเนื้อหา เพราะ agent เขียน markdown แบบเดียวกับเขียนไฟล์อื่นทั่วไป turn ingest ทั่วไปจะมีหน้าตาประมาณนี้

```
❯ I just read https://example.com/oauth-guide. Ingest the key points into 'notes'.

[assistant] Reading the page…
[tool: WebFetch(url: "https://example.com/oauth-guide")]
[tool: Write(path: "~/.config/thclaws/kms/notes/pages/oauth-client-credentials.md", ...)]
[tool: Edit(path: "~/.config/thclaws/kms/notes/index.md", ...)]
[tool: Edit(path: "~/.config/thclaws/kms/notes/log.md", ...)]
Wrote pages/oauth-client-credentials.md, added entry to index.md, appended to log.md.
```

gist ของ Karpathy อธิบาย workflow ไว้เป็นสามขั้นตอน ได้แก่

1. **Ingest** — อ่านแหล่งข้อมูล สกัดข้อเท็จจริงที่แตกต่างออกมา แล้วเขียน page, อัปเดต index และ append ลง log
2. **Query** — ตอบคำถามจาก wiki (agent จะทำขั้นนี้ให้เองเมื่อมี KMS ผูกไว้)
3. **Lint** — เป็นระยะ ๆ ให้ agent อ่านทุก page แล้วเสนอว่าตรงไหนควร merge, split หรือจัดการ orphan

ทั้งหมดนี้สั่งด้วยภาษาธรรมชาติได้เลย ไม่ต้องใช้ slash command พิเศษใด ๆ

## ขีดจำกัดการ scale และทิศทางในอนาคต

v0.2.x ตั้งใจให้ไม่มี embeddings โดย

- Grep เร็วพอใช้งานได้ถึงระดับไม่กี่ร้อย page
- การให้อ่าน `index.md` ก่อน ทำให้ agent มักเจอ page ที่เกี่ยวข้องได้โดยไม่ต้องค้นเลย
- page เป็น markdown ที่มนุษย์อ่านได้ จึงเปิดดูเองได้โดยไม่ต้องใช้เครื่องมือใด ๆ

เมื่อ KMS โตเกิน ~200 page หรือมีเนื้อหาภาษาอื่นที่ไม่ใช่อังกฤษซึ่ง grep จับคู่ข้ามไม่ได้สะอาดนัก คุณสามารถอัปเกรดเป็น hybrid RAG (hosted OpenAI embeddings) ได้ ซึ่งวางแผนไว้สำหรับ release ในอนาคต โดย API ฝั่ง client จะยังคงเหมือนเดิม

## หมายเหตุสำหรับภาษาไทย

Grep ทำงานกับภาษาไทยได้ทันทีเพราะใช้การค้นแบบ substring ไม่ได้ผ่าน tokenize agent ของคุณจึงค้นคำว่า `"การยืนยันตัวตน"` ข้าม Thai note ทั้งหมดได้ผลลัพธ์ทันทีโดยไม่ต้องตั้งค่าอะไรเพิ่ม

สำหรับเนื้อหาเทคนิคที่ผสมไทยกับอังกฤษ ให้เขียนศัพท์เทคนิคภาษาอังกฤษไว้ในหน้าเดียวกับข้อความภาษาไทย แล้วทั้งคู่จะถูกจับได้เมื่อค้นเรื่องที่เกี่ยวข้อง

## Troubleshooting

- **KMS ไม่ขึ้นใน sidebar** — ตรวจสอบว่าโฟลเดอร์มี `index.md` ที่ใช้ได้ (สร้างเองด้วยมือถ้าคุณปั้น KMS เอง) และอยู่ใน `~/.config/thclaws/kms/` หรือ `.thclaws/kms/`
- **การเปลี่ยนแปลงไม่สะท้อนในคำตอบของ agent** — `index.md` ถูกอ่านตอนเริ่ม turn ดังนั้น turn ที่กำลังรันอยู่จะยังใช้ snapshot ที่ถ่ายไว้ก่อนหน้า ให้เริ่ม turn ใหม่เพื่ออัปเดต
- error **"no KMS named 'X'"** จาก tool call — ชื่อเป็น case-sensitive และต้องตรงกับชื่อ directory ทุกตัวอักษร ให้ตรวจสอบด้วย `/kms list`
- **รายการ active เก่าค้างอยู่** — `.thclaws/settings.json` คือ source of truth หาก checkbox บน sidebar ไม่ตรงกับความจริง ให้แก้ไฟล์นี้ด้วยมือ

## อ่านต่อที่ไหน

- [บทที่ 8](ch08-memory-and-agents-md.md) — memory และ project instructions (อีกสอง mechanism ที่ใช้จัดการ context)
- [บทที่ 10](ch10-slash-commands.md) — เอกสารอ้างอิง slash command รวมถึงตระกูล `/kms`
- [บทที่ 11](ch11-built-in-tools.md) — เอกสารอ้างอิง tool รวมถึง `KmsRead` และ `KmsSearch`
