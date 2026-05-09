# บทที่ 17 — Agent Teams

Agent Teams ช่วยให้คุณรัน **thClaws agent หลายตัวขนานกัน** โดยประสานงาน
ผ่าน mailbox และ task queue ที่วางอยู่บน filesystem เหมาะกับงานที่
แตกเป็นหลายสายได้จริง ๆ เช่น ทำ backend กับ frontend พร้อมกัน หรือให้
agent ตัวหนึ่งเขียน test ขณะที่อีกตัวพัฒนา feature

Team เป็นแบบ **opt-in** เพราะจะเปิด process เพิ่มและเผา token เร็วมาก

**เปิดจาก GUI** คลิกไอคอนเฟือง → หมวด *Workspace* จะมีแถว *Agent Teams*
พร้อม pill on/off คลิกเพื่อสลับ ระบบจะเขียน `teamEnabled: true` ลง
`.thclaws/settings.json` ให้เอง พร้อมแจ้งเตือนสีเหลือง "Restart the app
for this to take effect" — team tool ลงทะเบียนตอน session spawn ดังนั้น
session ที่รันอยู่ต้อง respawn ใหม่ก่อนจะเห็น tool

**เปิดจาก CLI หรือแก้มือ:**

```json
// .thclaws/settings.json
{ "teamEnabled": true }
```

เมื่อ `teamEnabled: false` (ค่าดีฟอลต์) จะไม่มีการลงทะเบียน team tool ใด ๆ
และ inbox poller จะไม่ทำงาน tab Team ใน GUI ยังแสดงอยู่เสมอ — โดยจะ
ขึ้น empty-state ชี้ทาง ("No team agents running — ask the agent to
create a team") เพื่อให้รู้ทันทีเมื่อมีทีมใหม่ถูกสร้าง ส่วน sub-agent
(บทที่ 15) ยังใช้ได้ตามปกติไม่ว่าจะเปิด team หรือไม่

> ⚠ **ข้อจำกัดของ provider: model ตระกูล `agent/*` ใช้ thClaws teams ไม่ได้**
> Provider `agent/*` ([บทที่ 6](ch06-providers-models-api-keys.md))
> shell ออกไปเรียก `claude` CLI ในเครื่องเป็น subprocess ซึ่งใช้
> toolset ของ Claude Code เอง (`Agent`, `Bash`, `Edit`, `Read`,
> `ScheduleWakeup`, `Skill`, `ToolSearch`, `Write`) และมองไม่เห็น
> tool registry ของ thClaws — ดังนั้นแม้จะเปิด `teamEnabled: true`
> tool `TeamCreate` / `SpawnTeammate` / ฯลฯ ของเราก็เข้าถึงไม่ได้
> ถ้าจะใช้ thClaws teams ให้สลับไปใช้ provider อื่นที่ไม่ใช่ `agent/*`
> เช่น `claude-sonnet-4-6`, `claude-opus-4-7`, `gpt-4o` ฯลฯ ผ่าน
> `/model` หรือ `/provider` system prompt ตั้ง grounding ไว้ให้ model
> บอกผู้ใช้เรื่องนี้ตรง ๆ แทนที่จะเรียก `TeamCreate` built-in ของ
> Claude Code ที่เขียนลง `~/.claude/teams/` (มองไม่เห็นใน Team tab
> ของ thClaws)

หากใช้ provider ตระกูล `agent/*` แต่ `teamEnabled: false` system prompt
ก็จะ ground model ไว้คล้ายกัน คือบอก **ห้าม** เรียก built-ins ของ
Claude Code เช่น `TeamCreate` / `Agent` / `TodoWrite` /
`AskUserQuestion` / `ToolSearch` เพื่อกัน model hallucinate ว่าสร้าง
ทีมสำเร็จทั้ง ๆ ที่ไม่มีอะไรเขียนลง `.thclaws/team/` จริง ๆ ดูราย
ละเอียดใน dev-log 078

## กายวิภาค

```
.thclaws/team/
├── config.json                  team config (members, lead)
├── inboxes/{agent}.json         per-agent inbox (JSON array)
├── tasks/{id}.json              task queue entries
├── tasks/_hwm                   high-water mark for task IDs
└── agents/{agent}/status.json   heartbeat + current task
```

ทุกอย่างเก็บเป็นไฟล์ ไม่มี DB ไม่มี broker โดยใช้ advisory lock ผ่าน
`fs2` เพื่อให้การเขียน inbox เป็น atomic ข้าม process

## Team tools

ทั้งหมดนี้จะถูกเพิ่มเข้า registry ของ agent โดยอัตโนมัติเมื่อ `teamEnabled: true`

| Tool | วัตถุประสงค์ |
|---|---|
| `TeamCreate` | สร้าง team พร้อมตั้งชื่อ agent |
| `SpawnTeammate` | เปิด process ของ teammate (tmux pane หรือเบื้องหลัง) |
| `SendMessage` | เขียนข้อความลงใน inbox ของ teammate |
| `CheckInbox` | อ่านข้อความที่ยังไม่ได้อ่าน พร้อม mark ว่าอ่านแล้ว |
| `TeamStatus` | สรุปสถานะ agent + task queue |
| `TeamTaskCreate` | เพิ่ม task (ระบุ dependency ได้) |
| `TeamTaskList` | ลิสต์ task ตามสถานะ |
| `TeamTaskClaim` | รับ task ที่ pending และไม่ถูก block (teammate) |
| `TeamTaskComplete` | ทำเครื่องหมายว่าเสร็จ + แจ้ง lead |
| `TeamMerge` | Merge branch ของ worktree teammate กลับเข้า main |

## การตั้งทีมขึ้น

Prompt ทั่วไปของ lead

```
❯ Create a team with two members: "backend" (for the API) and
  "frontend" (for the React app). Use backend.md and frontend.md
  definitions under .thclaws/agents/. Spawn both now.
```

Lead จะเรียก `TeamCreate` ก่อน แล้วตามด้วย `SpawnTeammate` สองครั้ง
process ของ teammate แต่ละตัวจะ boot ในรูป `thclaws --team-agent backend`
(และอื่น ๆ ในทำนองเดียวกัน) โดยแต่ละตัวมี inbox และ status file ของตัวเอง

## รูปแบบการรัน

ถ้าอยู่ใน tmux session อยู่แล้ว `SpawnTeammate` จะเปิดแต่ละ teammate ใน split pane
ถ้าอยู่นอก tmux จะเปิด tmux session แบบ detached ให้ ซึ่ง attach ได้ด้วย
`/team`

```
❯ /team
(attaching to tmux session 'thclaws-team'…)
```

แต่ละ pane คือ REPL เต็มรูปแบบของ teammate แต่ละตัว สามารถพิมพ์คุยกับ
ตัวใดตัวหนึ่งได้โดยตรง

```
❯ (on lead) send to frontend: "the /users endpoint now returns a new
  `displayName` field — update the profile page"
```

ข้อความนั้นจะถูกแปลงเป็น `SendMessage` ลงใน inbox ของ frontend
teammate ฝั่ง frontend จะหยิบไปทำในการ poll รอบถัดไป (ทุก ๆ 1 วินาที)
จัดการงาน แล้วรายงานกลับหา lead ผ่าน `SendMessage` อีกครั้ง

## Task queue

แทนที่จะส่งข้อความตรง ๆ จะโพสต์เป็น task แทนก็ได้

```
TeamTaskCreate(
  id: "t3",
  description: "Write integration tests for /orders endpoints",
  agent: "backend",
  depends_on: ["t1", "t2"]
)
```

Teammate จะ auto-claim task ที่ pending และยังไม่ถูก block ตอนว่าง
(คือไม่มีข้อความใน inbox และไม่มี task ที่กำลังทำค้างอยู่) ส่วนเรื่อง
dependency: task ที่มี `depends_on` จะ claim ได้ก็ต่อเมื่อ dependency
ทุกตัวขึ้นสถานะ `completed` แล้วเท่านั้น

Workflow

1. Lead โพสต์ `t1`, `t2`, `t3` (โดย `t3` ขึ้นกับ `t1`+`t2`)
2. `backend` กับ `frontend` ต่างก็ claim งานที่ตัวเอง claim ได้
3. เมื่อทำเสร็จ → `TeamTaskComplete` จะยิง `idle_notification` ไปหา lead
4. พอ `t1` และ `t2` เสร็จครบ `t3` ก็จะ unblock ให้ใครว่างหยิบไปทำต่อ

## การแยก worktree และ filesystem sandbox ของ team

ใน agent def จะตั้ง `isolation: worktree` ก็ได้

```markdown
---
name: backend
model: claude-sonnet-4-6
tools: Read, Write, Edit, Bash, Glob, Grep
isolation: worktree
---

You own the backend services. Work in your own git worktree so you
don't collide with the frontend teammate.
```

เมื่อ spawn teammate ขึ้นมา thClaws จะสร้าง `<workspace>/.worktrees/backend`
บน branch `team/backend` แล้วรัน teammate process นั้นด้วย `cwd =
<workspace>/.worktrees/backend/` การเปลี่ยนแปลงที่เขียนแบบ relative
path จะถูกแยกไว้บน branch ของ teammate ตัวนั้นจนกว่า lead จะเรียก
`TeamMerge`

```
TeamMerge(only: ["backend"])
```

คำสั่งนี้จะรัน `git merge team/backend` เข้ามาใน branch ปัจจุบันของ
lead (โดยปกติคือ `main`) เพื่อดันงานของ teammate เข้าสู่สายหลัก
ใช้ `dry_run: true` ก่อนเพื่อดูว่ามี commit ที่ ahead จริงไหม ถ้า
ไม่มี ให้ ping teammate ให้ commit ในโฟลเดอร์ worktree ของตัวเอง
ก่อน

ถ้า `<workspace>` ยังไม่ใช่ git repo ตอน spawn teammate worktree ตัว
แรก thClaws จะรัน `git init` ให้พร้อม commit เปล่าเริ่มต้นโดยอัตโนมัติ
จึงไม่ต้อง pre-init เอง

### sandbox ของ teammate ต่างจาก standalone อย่างไร

อ่านเรื่อง standalone sandbox ที่[บทที่ 5](ch05-permissions.md#sandbox-ของ-filesystem)
ก่อน ในโหมดเดี่ยว: sandbox root = cwd ตอนเปิด session

สำหรับ teammate ใน team: sandbox root = **workspace ของ lead เสมอ**
(ไม่ใช่ cwd ของตัว teammate) `SpawnTeammate` ส่ง env var
`THCLAWS_PROJECT_ROOT` ติดมากับ teammate process — `Sandbox::init()`
อ่าน env นี้ก่อน fall back ไปใช้ `cwd` แค่กรณีเดี่ยว ผลคือ
teammate ทุกตัวมีสิทธิ์เขียน "ทุกที่ใต้ workspace" (เว้น `.thclaws/`)
ไม่ว่าตัวเองจะถูกวางไว้ใน folder ไหน

ส่วน **cwd** ของ teammate จะถูกชี้ไปที่ folder ที่เหมาะกับงาน:
- `isolation: worktree` → cwd = `<workspace>/.worktrees/<name>/`
- ไม่ตั้ง isolation → cwd = `<workspace>` (เหมือน lead)

ทั้งสองค่ามีผลคนละแง่ — cwd บอกว่า relative path จะ resolve ไปไหน
sandbox root บอกว่าขอบของสิทธิ์เขียนกว้างแค่ไหน

### สอง class ของไฟล์ที่ teammate เขียนได้

| รูปแบบ path | ลงที่ไหน | ใครเห็นเมื่อไร |
|---|---|---|
| relative (`src/server.ts`) จาก worktree | `<workspace>/.worktrees/backend/src/server.ts` บน branch `team/backend` | คนอื่นเห็นหลัง `TeamMerge` |
| absolute (`<workspace>/docs/api-spec.md`) จาก worktree | `<workspace>/docs/api-spec.md` บน `main` ของ workspace tree | คนอื่นเห็นทันที ไม่ต้อง merge |
| relative (`tests/api.test.ts`) จาก non-isolated teammate | `<workspace>/tests/api.test.ts` บน `main` | เห็นทันที |

แพตเทิร์นที่ใช้ในการสร้างทีม ShopFlow:

- **Shared contract** (API spec, shared TS types) — ให้ backend
  เขียนแบบ absolute path ไปที่ workspace ทันทีที่มี → frontend และ
  qa อ่านได้เลยโดยไม่รอ merge
- **Implementation** (handlers, models, server) — เขียนแบบ relative
  ใน worktree → lead `TeamMerge` ภายหลังเมื่อพร้อม
- **Tests** — qa (non-isolated) เขียนใน workspace tree เลย รัน
  ได้หลัง implementation merged

### path ที่ถูก deny เป็นพิเศษ

นอกเหนือจากกฎ standalone (`..` escape, symlink escape, ออกนอก root):

- ทุก teammate **ห้ามเขียน** `<workspace>/.thclaws/` — ใช้ team
  tool แทน
- thClaws ไม่บังคับห้ามเขียนข้าม worktree (เช่น backend เขียน
  `<workspace>/.worktrees/frontend/...`) — เป็นความรับผิดชอบของ
  prompt design ที่จะกัน เพราะ Claude Code reference impl
  ก็เลือกไม่บล็อกเช่นกัน ถ้าต้องการ guard ระดับ harness ให้เพิ่ม
  เป็น hook ใน `.thclaws/settings.json`

### ทำไมถึงเลือกโมเดลนี้

ทางเลือกที่ตรงไปตรงมากว่าคือ "sandbox = cwd ของ teammate" — ก็คือ
worktree teammate เขียนได้แค่ใน worktree ของตัวเอง แต่แบบนั้นแปลว่า
shared artifact (เช่น API spec) ต้องผ่านการ merge ก่อน frontend
ถึงจะได้อ่าน — เพิ่มขั้นตอนหลายตลบโดยไม่จำเป็น และ block ทีมขนาน
จนกลายเป็นทำงานเรียงแถว

โมเดลปัจจุบัน (sandbox = workspace, cwd = worktree) ตรงกับวิธีที่
คนใช้ git worktree จริง ๆ คือเปิด shell หนึ่งที่ root ของ repo
สำหรับ shared work และ `cd` เข้าไปใน worktree เฉพาะตอนแก้โค้ด
branch-specific เป็นแบบเดียวกับที่ Claude Code reference
implementation เลือก (`getOriginalCwd()` ใน
`utils/permissions/filesystem.ts`)

## Plan Approval (convention)

ถ้า prompt ของผู้ใช้พูดถึง "Plan Approval", "with plan approval",
หรือคำคล้าย ๆ — ระบบตีความว่าเป็น **lead↔teammate convention** ไม่ใช่
การถามผู้ใช้:

1. teammate ก่อนเริ่มงานสำคัญจะส่งแผนสั้น ๆ (1–3 บรรทัด: จะทำอะไร, แตะไฟล์ไหน) ไปหา lead ผ่าน SendMessage
2. lead รีวิวแล้วตอบกลับ "approved, proceed" หรือ "revise: …"
3. teammate รอ ack แล้วค่อยลงมือทำ

**lead เป็นผู้อนุมัติเสมอ** — ไม่มี handshake ของผู้ใช้แม้จะมีคนเฝ้าหน้าจออยู่
ก็ตาม โหมดนี้เปิดเฉพาะตอน prompt ผู้ใช้ระบุชัดเจน หากไม่ระบุ teammate
จะเริ่มทำงานทันทีโดยไม่ต้องรอ approve เพื่อรักษาพฤติกรรม default
รายละเอียดอยู่ใน `default_prompts/lead.md` และ `default_prompts/agent_team.md`

## Role guards (lead vs teammate)

เพื่อป้องกัน LLM lead ลบไฟล์ของ teammate โดยไม่ตั้งใจ (เช่น `rm -rf tests/`
ในรอบทดสอบจริง) BashTool / Write / Edit มี hard guard:

**lead — ห้ามรัน (ไม่ว่า `--accept-all` จะเปิดอยู่หรือไม่):**

| คำสั่ง | เหตุผล |
|---|---|
| `git reset --hard <ref>` | ทิ้ง commit ที่ทำไปแล้ว |
| `git clean -f` / `-d` | ลบไฟล์ untracked |
| `git push --force` / `git rebase` | rewrite ประวัติ shared |
| `git worktree remove` / `prune` | kill teammate process + worktree |
| `git checkout -- <path>` / `git restore --worktree` | ทิ้งงานยังไม่ commit ของ teammate |
| `git merge --abort` | ยุบ merge แทนที่จะ delegate |
| `rm -rf` / `-fr` / `-r` | ลบไฟล์แบบล้างบาง |
| `Write` / `Edit` ไฟล์อะไรก็ตาม | lead เป็น coordinator ไม่ใช่ผู้เขียนโค้ด |

**ข้อยกเว้น Write/Edit:** ถ้ามี `git merge` ที่ค้างอยู่ AND ไฟล์เป้าหมายมี marker `<<<<<<<` อยู่ — lead เขียนไฟล์ที่แก้ conflict แล้วได้ พอ commit merge เสร็จ `MERGE_HEAD` หาย guard ก็กลับมาเปิด

**teammate — ห้ามรัน:**

| คำสั่ง | เหตุผล |
|---|---|
| `git reset --hard <branch-name>` (เช่น `main`, `origin/main`, `team/backend`) | ดึง branch ของ teammate ไปทับ branch อื่น เสียงานเดิม |

ที่ยังอนุญาต (recovery บน branch ตัวเอง): `HEAD~N`, `HEAD@{N}`, `HEAD^`, hex SHA, `tags/...`

ถ้า LLM lead/teammate พยายามรันคำสั่งเหล่านี้ tool จะ return error แบบบอกเหตุผลให้ model ปรับวิธีใหม่ (เช่น "ลองให้ teammate ทำแทน" หรือ "ใช้ HEAD~N แทน main")

### ตัว stub editor สำหรับ teammate

SpawnTeammate set env var ให้ teammate ทุกตัว: `EDITOR=true VISUAL=true GIT_EDITOR=true GIT_SEQUENCE_EDITOR=true`

ทำให้คำสั่งที่เปิด editor (เช่น `git commit -e`, `git commit` แบบไม่มี `-m`, `git rebase -i`) **ไม่ค้าง** รอ user input ผ่าน `/dev/tty` — `true` builtin จะ exit 0 ทันที, git ใช้ message ที่ส่งมาแล้วผ่าน `-F`/`-t` หรือ commit empty ตามค่า default ป้องกัน `vi` หรือ `nano` หยุดทีมไว้กลางทาง

## Protocol messages

ประเภทข้อความมาตรฐานที่ teammate กับ lead ใช้แลกเปลี่ยนกัน

| Type | From → To | ความหมาย |
|---|---|---|
| `idle_notification` | teammate → lead | "ผมเพิ่งทำ task X เสร็จ งานต่อไปคืออะไร?" |
| `shutdown_request` | lead → teammate | "หยุดและออกอย่างสะอาด" |
| `user` | user → teammate | ข้อความอิสระ (ผ่าน `send to <agent>: …`) |

## การ monitor ใน GUI

tab Team จะแสดง pane ละหนึ่งอันต่อ teammate หนึ่งคน พร้อม pane `lead`
ที่มิเรอร์ terminal หลัก สี ANSI จะถูกแปลงเป็น HTML โดย เขียวสำหรับ
ข้อความ LLM ไซแอนสำหรับ prompt และข้อความใน inbox หรี่สำหรับ tool start
และบรรทัด token ส่วนเหลืองสำหรับ error หรือการชน max-iterations

สถานะดึงมาจาก `status.json` ของ teammate เอง (`idle` / `working` /
`stopped`) โดยจะไม่ตั้งธงว่า crash แบบผิด ๆ เพียงเพราะ heartbeat หายไป

## เมื่อไม่ควรใช้ team

งานส่วนใหญ่ใช้ agent เดียวคู่กับ sub-agent ผ่าน `Task` (บทที่ 15)
ก็เพียงพอแล้ว หยิบ team มาใช้ก็ต่อเมื่อการทำงานขนานเกิดขึ้นจริง ๆ
และคุ้มค่ากับต้นทุนที่จ่ายไป วิธีทดสอบง่าย ๆ คือ ถ้าแจก task ของ
teammate แต่ละตัวให้ contractor คนละคนไปทำได้โดยไม่ต้องประสานงานวุ่นวาย
นั่นแหละคือรูปแบบงานที่เหมาะกับ team
