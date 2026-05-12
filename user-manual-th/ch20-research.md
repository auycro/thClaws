# บทที่ 20 — Background research (`/research`)

`/research <query>` spawn งาน background ที่:

1. ค้นเว็บ
2. iterate กับ LLM ดูว่าขาดอะไร
3. จัดผลเป็น KMS pages หลายหน้าที่ cross-link กัน
4. cache แต่ละ source ที่อ้างเป็นไฟล์ Markdown ใน `<kms>/sources/`

ผลอยู่ใน knowledge base เป็น artifact ถาวร — ค้นได้ข้าม session,
อ้างจาก chat ได้, แก้ได้เหมือน KMS page อื่น

## Quick start

```
> /research what is the LangGraph agent strategy in local-deep-research
[research started: id=research-a3f1c2] query: what is the LangGraph …
  /research status research-a3f1c2     check progress
  /research show research-a3f1c2       stream result
  /research cancel research-a3f1c2     cancel
```

panel "Research" ขึ้นที่ขอบขวาของ GUI แสดง phase, iteration progress,
และ score ต่อรอบ CLI users เห็นบรรทัด completion ที่ prompt ถัดไป:

```
[research done: id=research-a3f1c2 → langgraph-agent-strategy/2026-05-09-…__concept.md]
```

หลังจบ, KMS เป้าหมาย auto-attach กับ session ทันที ทำให้คำถามต่อ
อย่าง *"สรุป LangGraph approach ให้หน่อย"* ทำให้ LLM เรียก
`KmsRead` หน้าที่เพิ่งเขียนแทนที่จะตอบจาก training data

## คำสั่งย่อย

```
/research <query>                          เริ่ม run ใหม่ (default config)
/research [flags...] <query>               เริ่มพร้อม override
/research                                  list ทุก job (newest first)
/research list                             เหมือนกัน
/research status <id>                      detail (phase, iter, score)
/research show <id>                        print synthesized page ใน chat
/research cancel <id>                      cancel; ผลที่ทำมาทิ้ง
/research wait <id>                        block CLI prompt จน job จบ
```

### Flags ตอน start

| Flag | Default | ความหมาย |
|---|---|---|
| `--kms <name>` | auto จาก query | KMS เป้าหมาย ชื่อที่ derive อัตโนมัติใช้ topic slug ที่ LLM แตกออก (เช่น `obon-festival`) — ไม่มี `research-` prefix |
| `--min-iter N` | 2 | hard floor — pipeline ต้องรันอย่างน้อย N รอบแม้ LLM scores ครบก่อน |
| `--max-iter K` | 8 | hard ceiling |
| `--score-threshold 0.X` | 0.80 | score ที่ LLM evaluator ต้องให้ (0.0-1.0) เพื่อ short-circuit ระหว่าง min กับ max รับทั้ง decimal (`0.85`) และ percent integer (`85`) เพิ่มจาก 0.75 → 0.80 — LLM มัก score generously หลัง iter 2 default เดิมตัด research เร็วเกินไป |
| `--max-pages N` | 7 | cap จำนวน KMS pages ต่อ run *ceiling ไม่ใช่ target* — query แคบจะออก 1-2 pages |
| `--budget-time SEC|2m|1h` | 15m | wall-clock budget เกินแล้วงานจบเป็น `Failed` พร้อม budget-exhausted message |

## ผลที่ลง KMS

โครงสร้างใน KMS เป้าหมายหลัง `/research` รัน:

```
<kms-name>/
├── pages/
│   ├── <YYYY-MM-DD>-<query>__<page-slug>.md    ← หนึ่งไฟล์ต่อ page ใน plan
│   ├── <YYYY-MM-DD>-<query>__<page-slug>.md
│   └── _summary.md                              ← per-run section index
├── sources/
│   ├── <url-slug>.md                            ← cache fetch body ต่อ URL ที่อ้าง
│   └── <url-slug>.md
├── index.md                                     ← auto-managed
├── log.md
└── SCHEMA.md
```

### Pages

แต่ละหน้าครอบคลุม topic เดียว — entity (บุคคล, paper, องค์กร),
concept, comparison (X vs Y), how-to, หรือ timeline. หน้าต่างๆ
cross-link กันด้วย Obsidian-style `[[slug]]` wikilinks ใช้งานได้
ทั้ง Obsidian, GitHub, และ `KmsRead`

ทุกหน้าเริ่มด้วย abstract 1-2 ประโยคซึ่งกลายเป็น KMS index summary
ตามด้วย `##` subsections, inline `[N]` citations, และ
`## Sources` ที่ pipeline auto-generate listing ทุก source ที่อ้าง
พร้อม clickable link ไปยัง cached copy:

```markdown
LLM Wiki คือ knowledge base ส่วนตัว/local ที่ LLM กับ user
ร่วมเขียน markdown notes สะสมไปเรื่อยๆ — popular โดย
[[andrej-karpathy|Andrej Karpathy]] [1](../sources/x-com-karpathy-status.md)

## ที่มา

แนวคิดเกิดต้นปี 2026 ตอนที่ …

## Sources

1. [Karpathy on LLM Wiki](../sources/x-com-karpathy-status.md) — https://x.com/karpathy/status/…
2. [Comparing LLM Wiki vs RAG](../sources/medium-com-llm-wiki-vs-rag.md) — https://medium.com/…
```

### Verify pass — กัน hallucinated citations

หลัง synthesize แต่ละ page เสร็จ pipeline จะรัน **verify pass** — LLM call แยกที่ audit page ที่ generate แล้วเทียบกับ source ที่อ้าง walk ทุก factual claim แล้วตัดสิน:

- `Supported` — source ที่อ้างพูดตรงตาม claim
- `Partial` — source แตะ topic แต่ไม่ตรง wording (เช่น claim บอก "100x faster" แต่ source บอกแค่ "significantly faster")
- `Unsupported` — citation ผิด (source ไม่ได้พูดอย่างนั้น) ปกติคือ hallucination หรือ miscitation
- `NoCitation` — page assert ข้อเท็จจริงโดยไม่มี `[N]` แนบ

Pass นี้เขียน 2 artifact:

1. **`verification_score: 0.85`** ใน frontmatter — fraction ของ factual claim ที่ rate `Supported` Sort หรือ filter KMS ด้วย field นี้เพื่อหา page ที่ต้องตรวจซ้ำ
2. **`## Verification` section** ท้าย page body — list **เฉพาะ** flagged items (Partial / Unsupported / NoCitation) แต่ละ item มี icon (🚫 / ⚠️ / ❓), verdict, cited `[N]`, paraphrased claim, และ note ของ verifier:

```markdown
## Verification

Auto-verification pass found 2 claim(s) that don't strictly match their cited source. Review before relying on the page for downstream decisions.

- 🚫 **unsupported** [3]: X is 100x faster than Y — _[3] says faster but not "100x"_
- ❓ **no citation** (uncited): Y was released in 2024
```

Page ที่ทุก claim เป็น `Supported` ได้ `verification_score` ใน frontmatter แต่ไม่มี `## Verification` section — page ที่สะอาดอยู่สะอาดต่อไป ถ้า verifier เอง fail (parse error, provider timeout) page จะถูกเขียนโดยไม่มี `verification_score` (ขาด field = honest; ใส่ 0.0 ปลอม = misleading) แล้ว run ทำต่อ

**ทำไมต้องมี** — critique ของ LLM-Wiki ที่ดังที่สุด ("organised persistent mistakes") ชี้ที่ synthesizer ที่ hallucinate fact แล้วอ้าง real source ที่ไม่ได้พูดอย่างนั้น Verify pass จับ class นี้ก่อน page ลง KMS ค่าใช้จ่าย: ~+25% ของ total `/research` LLM cost สำหรับ 4-page run; soft-fail จึงไม่ abort pipeline

### Sources

ทุก URL ที่อ้างมี cached copy ที่ `<kms>/sources/<url-slug>.md`
พร้อม frontmatter (URL ต้นฉบับ, citation index, fetch date)
ชื่อไฟล์เป็น deterministic slug ของ URL — URL เดียวกันใน research
runs ต่างๆ map ไปไฟล์เดียวกัน ดังนั้น archive ไม่ระเบิด

ถ้า `HAL_API_KEY` ตั้งไว้ (Settings → Providers → Service keys
→ HAL Public API), `/research` fetch ผ่าน HAL headless browser
scrape — clean Markdown รวม code blocks, tables, nested lists.
ถ้าไม่มี key, fallback ไป `WebFetch` ที่แปลง HTML→Markdown
(หยาบกว่าแต่ใช้ได้)

### Run summary

`pages/_summary.md` สะสม section ต่อ `/research` run:

```markdown
## 2026-05-09 — what is the LangGraph agent strategy

- [[2026-05-09-langgraph-agent__concept-overview|Concept overview]] — Core idea …
- [[2026-05-09-langgraph-agent__research-subtopic-tool|research_subtopic tool]] — Parallel fanout …
- [[2026-05-09-langgraph-agent__rag-comparison|vs RAG]] — Differences in …
```

Wikilinks render native ใน Obsidian; ใน GitHub web view หรือ
`KmsRead` ใช้ run-prefixed filenames เปิดได้ตรงๆ

## Live progress

### GUI — right-edge sidebar

panel "Research" mirror Plan / Todo sidebars ที่ขอบขวา:

- **Phase** — step ปัจจุบัน (`iteration 3/8: searching 4 subtopics`,
  `synthesizing 5 pages in parallel`, `writing pages to KMS`)
- **Iteration progress bar** — N segments สีบอก done / in-progress /
  pending
- **Score history** — แถวต่อรอบที่จบ พร้อม 0-100% bar และจำนวน
  source delta
- **Phase log** — 10 phase ล่าสุดแบบ distinct ปัจจุบัน highlight
- **Footer** — `Show result` / `Cancel` ตามสถานะ

panel auto-focus job ล่าสุดที่ active ถ้าไม่มี job ใดๆ ก็ซ่อน
ขอบขวากระชับ

### CLI — completion line

CLI print announcement เหนือ readline prompt สำหรับ job ที่จบ
ตั้งแต่ prompt ก่อนหน้า:

```
[research done: id=research-a3f1c2 → obon-festival/2026-05-09-…md]
[research failed: id=research-x9z8] HAL request failed: HTTP 429
```

แต่ละ id ประกาศครั้งเดียวต่อ process ใช้ `/research show <id>`
ดู synthesized page ใน chat, `/research wait <id>` block จน
terminal (มีประโยชน์ใน scripts)

## Pipeline ทำงานยังไง

1. **Initial broad search** — WebSearch 1 ครั้งสำหรับ raw query 10 results
2. **Subtopic extraction** — LLM เสนอ 3-5 search queries ที่ focus จาก seed
3. **Iteration loop** (1..max_iter):
   - ต่อ subtopic: parallel WebSearch, top-3 fetch, accumulate
   - Evaluate (LLM scores 0.0-1.0 + free-form notes อธิบาย gaps)
   - Stop เมื่อ `iter ≥ min_iter AND score ≥ threshold`, หรือถึง
     `max_iter`, หรือ LLM ตอบ "ไม่มี subtopics เพิ่ม"
   - ไม่งั้น generate next-round subtopics จาก eval notes
4. **Page plan** — LLM group sources เป็น ≤ `max_pages` หน้า
   coherent. Page count เป็น *ceiling ไม่ใช่ target* — query แคบ
   ออกหน้าน้อยลง
5. **Parallel page synthesis** — 1 LLM call ต่อหน้า แต่ละ call เห็น
   full plan ทำให้ cross-links resolve ได้
6. **Cross-link rewrite** — `[[karpathy]]` กลายเป็น
   `[[<run-prefix>__karpathy]]` ให้ resolve ไปไฟล์จริงบน disk.
   Display text เก็บไว้ (`[[karpathy|Andrej Karpathy]]`)
7. **Sources section + citation linkifier** — pipeline rebuild
   `## Sources` จาก `[N]` ที่ใช้จริง และ rewrite inline `[N]` เป็น
   clickable link ไปยัง cached source files
8. **Write pages + update `_summary.md` + cache cited sources**

## Cost + speed

run typical 4-iteration พร้อม 4 subtopics และ 5 หน้า:

| Step | LLM calls |
|---|---|
| extract_subtopics | 1 |
| evaluate | 4 (1 ต่อรอบ) |
| extract_next_subtopics | 3 |
| plan_pages | 1 |
| derive_topic_slug (เมื่อไม่มี `--kms`) | 1 |
| write_research_page (parallel) | 5 |
| **รวม** | **~15** |

บวก HTTP: ~17 web searches + ~12 page fetches

Wall clock บน `gpt-4.1-mini`: 3-5 นาที (pages synthesize parallel
ทำให้ multiplier ของ page-count ไม่ dominate)

`/research` รัน background เต็มตัว — main chat session ไม่กระทบ
พิมพ์ต่อได้

## เคล็ดลับ

- **Pin KMS** — ส่ง `--kms <name>` ถ้าอยากให้ output สะสมใน knowledge
  base เดียวข้ามหลาย runs ถ้าไม่ส่ง `--kms`, pipeline derive per-query
  slug
- **`/kms use <name>` ก่อนถามต่อ** ทำให้ LLM consult หน้าที่เพิ่งเขียน
  KMS auto-activate หลัง research จบใน GUI session; CLI users อาจ
  ต้อง activate มือ
- **เปิด raw sources** ตอน verify claim — เปิด
  `<kms>/sources/<slug>.md` ตรงๆ cached body มี URL ต้นฉบับใน
  frontmatter ตามหาที่มาได้
- **tune `--max-pages`** ตามความกว้างของ topic: 1-2 สำหรับคำถาม
  ข้อเท็จจริงแคบ, 3-5 medium, 7+ broad overview
- **ตั้ง HAL** ให้ source archive สะอาดกว่า — clean-Markdown ของ
  HAL beat HTML conversion ของ `WebFetch` ชัดเจนเมื่อหน้ามี table,
  code blocks, หรือโครงสร้างซับซ้อน

## Troubleshooting

**"research time budget exhausted"** — เพิ่ม `--budget-time` หรือ
narrow query default 15 นาที

**"all WebSearch backends failed"** — เช็ค `TAVILY_API_KEY` /
`BRAVE_SEARCH_API_KEY` ใน Settings หรือรันโดยไม่มี key (fallback
DuckDuckGo, คุณภาพต่ำกว่า)

**Pages ไม่ cross-link** — LLM link เฉพาะที่เกี่ยวข้องจริง ถ้า query
แคบมาก (entity เดียว, concept เดียว), อาจมีหน้าเดียวจริงๆ —
wikilinks ไม่จำเป็น

**Sources section มี "(unknown source — index out of range)"** —
LLM hallucinate citation index นอก source list. ไม่บ่อย; มัก
หายเองหลัง retry entry ที่ resolve ไม่ได้เก็บไว้ให้เห็นว่า
claim ไหนยังไม่มี cite

**Job ค้างที่ "synthesizing N pages"** — page-synth LLM call ตัวใด
ตัวหนึ่งช้า เช็ค `/research status <id>` ดู phase. Cancel ด้วย
`/research cancel <id>` ถ้าเกินทน — partial results ไม่เก็บ
