# ภาคผนวก A — Provider, โมเดล และราคา

ภาคผนวกนี้คือ catalog ของ **thClaws.cloud gateway** — รายการโมเดลที่
เรียกได้เมื่อชี้ agent ไปที่ `https://thclaws.cloud/gateway` หรือ
รัน [thClaws.cloud](ch27-thclaws-cloud.md) แบบ self-host เอง สำหรับ
desktop / CLI build จะมี catalog แยกของตัวเองที่ครอบคลุม 22 provider —
ดูที่ [บทที่ 6 — Provider, โมเดล และ API key](ch06-providers-models-api-keys.md)

> Catalog refresh ครั้งล่าสุด: **2026-06-02** ถ้าอยากดึงรายการโมเดล
> และอัตรา latest จาก upstream API + [LiteLLM pricing feed](https://github.com/BerriAI/litellm)
> ให้รัน `python3 scripts/refresh-model-catalogue.py` จากราก repo

## โครงสร้างราคา

ราคาด้านล่างคือ **ราคาที่คุณจ่ายจริง** — นั่นคือ upstream cost × platform markup
ภายใน gateway แยกออกเป็น 2 ชั้น:

| ชั้น | สิ่งที่เก็บ | ที่อยู่ |
|---|---|---|
| **อัตราใน DB row** | ราคา upstream แบบดิบจาก LiteLLM (USD ต่อ token) | ตาราง `model_pricing` seed โดย `scripts/refresh-model-catalogue.py` |
| **Platform markup** | ตัวคูณ `1.25×` ใช้ตอน meter | env var `THCLAWS_PLATFORM_MARKUP` บน gateway service |

วิธีนี้หมายความว่า: ถ้า Anthropic ปรับ Opus 4.8 ขึ้นเป็น $6/M คุณแค่
อัปเดต LiteLLM (หรือรอ sync รอบหน้า) gateway จะ pick up อัตราใหม่
ในการ refresh pricing ครั้งถัดไป — markup ยังคงเป็น env var ตัวเดียว
ที่ปรับแยก

ราคาทั้งหมดแสดงเป็น **ดอลลาร์สหรัฐ ต่อ 1,000,000 token** (1M token) ใน
DB เก็บเป็น microcent ต่อ 1k token (`µ¢/kt`) สูตรคือ `$/M = µ¢/kt / 100,000`

## Anthropic

| โมเดล | Tier | Input ($/M) | Output ($/M) | หมายเหตุ |
|---|---|---:|---:|---|
| `claude-haiku-4-5-20251001` | starter | $1.25 | $6.25 | Haiku ตัวล่าสุด (alias ที่ระบุวันที่ของ `claude-haiku-4-5`) |
| `claude-opus-4-8` | enterprise | $6.25 | $31.25 | Opus flagship ตัวปัจจุบัน |
| `claude-opus-4-7` | enterprise | $6.25 | $31.25 | Opus flagship ตัวก่อนหน้า |
| `claude-opus-4-6` | enterprise | $6.25 | $31.25 | |
| `claude-opus-4-5-20251101` | enterprise | $6.25 | $31.25 | |
| `claude-opus-4-1-20250805` | enterprise | $18.75 | $93.75 | Opus 4.1 รุ่นเก่า — Anthropic ยังเสิร์ฟอยู่ แต่งานใหม่ควรใช้ Opus 4.8 |
| `claude-opus-4-20250514` | enterprise | $18.75 | $93.75 | Opus 4.0 รุ่นเก่า |
| `claude-sonnet-4-6` | pro | $3.75 | $18.75 | โมเดล default ปัจจุบัน |
| `claude-sonnet-4-5-20250929` | pro | $3.75 | $18.75 | |
| `claude-sonnet-4-20250514` | pro | $3.75 | $18.75 | Sonnet 4.0 รุ่นเก่า |

> **หมายเหตุ** — คอลัมน์ `tier` เป็น **แค่ display** เท่านั้น thClaws.cloud
> ยกเลิก tier-gating ladder ไปแล้วใน v0.28 — user ที่มี credit เป็นบวก
> เรียกโมเดล active ตัวไหนก็ได้ ความต่างของราคาต่อการเรียกเป็น gate
> เดียวที่เหลือ ดูที่ ch27 § "Why no tier gate" สำหรับเหตุผลเบื้องหลัง

## OpenAI

| โมเดล | Tier | Input ($/M) | Output ($/M) | หมายเหตุ |
|---|---|---:|---:|---|
| `gpt-4o` | pro | $3.125 | $12.50 | |
| `gpt-4o-mini` | starter | $0.1875 | $0.75 | โมเดล chat ของ OpenAI ที่ถูกที่สุดบน gateway |
| `o1` | enterprise | $18.75 | $75.00 | โมเดล reasoning — token output รวม reasoning token ที่ซ่อนอยู่ |

OpenAI expose โมเดล chat-capable ประมาณ 76 ตัวผ่าน `/v1/models` (gpt-5.x, o3,
o4 ทุก variant พร้อม dated snapshot) ตอนนี้ใน gateway seed ไว้แค่ 3 ตัว
ด้านบน ถ้าต้องการตัวเฉพาะ ให้รัน refresh script ด้วย `--providers openai --apply`
มันจะดึง pricing จาก LiteLLM มาให้อัตโนมัติ รายการ upstream ทั้งหมดที่
มีให้ใช้:

- `gpt-5.5`, `gpt-5.5-pro`, `gpt-5.4`, `gpt-5.4-pro`, `gpt-5.4-mini`, `gpt-5.4-nano`,
  `gpt-5.3-codex`, `gpt-5.3-chat-latest`
- `gpt-5.2`, `gpt-5.2-pro`, `gpt-5.2-codex`, `gpt-5.2-chat-latest`
- `gpt-5.1`, `gpt-5.1-codex`, `gpt-5.1-codex-max`, `gpt-5.1-codex-mini`
- `gpt-5`, `gpt-5-pro`, `gpt-5-codex`, `gpt-5-mini`, `gpt-5-nano`, `gpt-5-search-api`
- `gpt-4.1`, `gpt-4.1-mini`, `gpt-4.1-nano`
- `o3`, `o3-pro`, `o3-mini`, `o3-deep-research`, `o4-mini`, `o4-mini-deep-research`,
  `o1-pro`
- รุ่นเก่า: `gpt-4`, `gpt-4-turbo`, `gpt-3.5-turbo*`

## Google (Gemini)

| โมเดล | Tier | Input ($/M) | Output ($/M) | หมายเหตุ |
|---|---|---:|---:|---|
| `gemini-2.0-flash` | starter | $0.125 | $0.50 | โมเดลที่ถูกที่สุดบน gateway ในบรรดาทุก provider |
| `gemini-2.0-pro` | pro | $1.95 | $7.81 | ⚠ ดูหมายเหตุด้านล่าง — **ไม่มีใน LiteLLM** อัตรามาจาก seed ตอนแรกอาจจะ stale |

Google expose โมเดล Gemini 20+ ตัวผ่าน `/v1beta/models` (gemini-2.5-flash,
gemini-2.5-pro, gemini-3-pro-preview, gemini-3.1-flash-lite ฯลฯ) รัน
refresh script เพื่อ seed ด้วยราคาปัจจุบัน

## OpenRouter

| โมเดล | Tier | Input ($/M) | Output ($/M) | หมายเหตุ |
|---|---|---:|---:|---|
| `openrouter/auto` | starter | (pass-through) | (pass-through) | OpenRouter's auto-router; cost จริงขึ้นกับโมเดลที่ routed ไป gateway forward usage ไปตามนั้น |

DB row ของ `openrouter/auto` เก็บ `0/0` เพราะ upstream cost แปรไปตาม
request ในกรณีนี้พึ่ง metering ของ OpenRouter เอง

## Row ที่ inactive / deprecated

| โมเดล | เหตุผล |
|---|---|
| `anthropic/claude-haiku-4-5` | ถูกแทนที่ด้วย alias ที่ระบุวันที่ `claude-haiku-4-5-20251001` — `/v1/models` ของ Anthropic ไม่ return alias เปล่าแล้ว เก็บ row ไว้ด้วย `active=FALSE` เพื่อให้ usage row เก่า ๆ ยัง join ได้ |

## หมายเหตุเรื่องความถูกต้องของราคา

| Row | สถานะ |
|---|---|
| 14 จาก 16 active row | **ตรงเป๊ะ** กับราคาที่ LiteLLM ประกาศ ณ 2026-06-02 |
| `google/gemini-2.0-pro` | **ไม่มีใน LiteLLM** — อัตราปัจจุบันมาจาก seed ของ migration 006 ตอนแรก Google อาจจะเปลี่ยนไปแล้ว refresh script ไม่สามารถ reprice row นี้อัตโนมัติได้ ควรเช็คเป็นระยะกับ [ai.google.dev/pricing](https://ai.google.dev/pricing) |
| `openrouter/auto` | Pass-through ไม่มี canonical cost |

ถ้าพบ price drift ให้รัน:

```bash
python3 scripts/refresh-model-catalogue.py --reprice-only            # dry-run
python3 scripts/refresh-model-catalogue.py --reprice-only --apply    # commit
```

## ปรับ platform markup

ตัวคูณ 1.25× เป็น env var ตัวเดียวบน gateway service:

```yaml
# thclaws-cloud/docker-compose.yml
gateway:
  environment:
    THCLAWS_PLATFORM_MARKUP: ${THCLAWS_PLATFORM_MARKUP:-1.25}
```

เปลี่ยนเป็น `1.50` แล้ว bounce gateway คือวิธีที่เร็วที่สุดในการเพิ่ม
margin แบบ uniform ทั่วทุกโมเดล gateway จะ clamp ค่าใด ๆ ที่ต่ำกว่า
`1.0` (รันที่ราคา upstream เป๊ะ ๆ คือขั้นต่ำ — ต่ำกว่านี้คือขาดทุนต่อ
การเรียก) และจะ emit warning

ถ้าอยากตั้งราคาต่างกันต่อโมเดล แก้ row ใน `model_pricing` ตรง ๆ ได้ —
แต่ระวังว่า refresh script จะ reprice กลับไปเป็น LiteLLM ใน run
`--reprice` ครั้งถัดไป ทางออกคือตั้ง `active=FALSE` แล้ว `active=TRUE`
ด้วยอัตรา custom พร้อมตั้ง `--providers` scope ให้ไม่รวม provider นั้น

## โค้ดที่เกี่ยวข้อง

| เรื่อง | ไฟล์ |
|---|---|
| คำนวณ cost | `thclaws-cloud/gateway/src/pricing.rs` (`cost_cents_with_markup`) |
| Parse markup env var | `thclaws-cloud/gateway/src/config.rs` (`platform_markup`) |
| จุดเรียก meter | `thclaws-cloud/gateway/src/meter.rs` |
| Schema ของ pricing table | `thclaws-cloud/api/alembic/versions/006_*.py` |
| Refresh script | `scripts/refresh-model-catalogue.py` |
