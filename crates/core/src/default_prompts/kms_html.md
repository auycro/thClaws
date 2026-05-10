The user ran `/kms html {kms_name}` and wants a beautiful,
self-contained interactive HTML website built **from** the KMS — the
result is a stand-alone artifact they'll keep in the workspace, not
a file inside the KMS itself.

Output destination: `{output_dir}/index.html` (absolute path).
Create the directory with the existing tooling if it doesn't exist.

## What goes in the site (and what doesn't)

The KMS has two kinds of content:

- **Pages** (`pages/*.md`) — the user's editorial knowledge. Their
  own notes, syntheses, decisions, summaries. **This is the
  primary content of the site.** Every page belongs in the
  generated HTML, fully readable.
- **Sources** (`sources/*.md`) — cached copies of external
  references that pages cite. Often long, raw, and not authored by
  the user. **Do NOT read source files by default.** Citations
  alone don't justify a Read; only open a source file when you
  have a *specific* editorial reason that wouldn't be solved by
  the page text alone (e.g. the page is leaning hard on one quote
  and you want to verify wording, or you're building the cover
  hero and need a single canonical URL).

Default citation rendering: `[N](../sources/<stem>.md)` becomes a
plain `[N]` superscript-style marker — not a link. The reader is
not expected to navigate into sources from the SPA. If a page text
*itself* contains a usable external URL inline, use that. The
"sources" directory can be entirely ignored unless reading a
specific frontmatter buys real editorial lift.

## Workflow — three phases, not one big shot

### Phase 1: Explore (read-only) — pages only

You don't have the KMS data in this prompt. Discover it yourself.
**Pages are the only thing you read in this phase. Sources are
off-limits unless you hit a specific need later.**

Suggested tool sequence:

1. `KmsRead(kms: "{kms_name}", page: "_index")` — start with the
   index/manifest if one exists. Falls through to the file listing
   below if not.
2. `KmsSearch(kms: "{kms_name}", pattern: ".")` (or any liberal
   pattern) to enumerate page slugs.
3. `KmsRead` 4–8 representative **pages** to understand the content
   style, common frontmatter fields, and the kinds of relationships
   in the data (citations, cross-page wikilinks, etc.). Do NOT read
   every page yet — sample first, full reads come in Phase 3.

Do **not** Read source files in this phase. Don't enumerate them,
don't peek at frontmatter, don't list them in the chat. Sources
exist on disk but the site treats them as opaque references.

When this phase ends you should be able to answer:
- What is this KMS *about* in 2 sentences?
- Roughly the page count.
- What's the dominant content style (terse notes? long essays?
  research synthesis? meeting logs?)
- Are there themes / clusters worth surfacing as nav?

### Phase 2: Design components (think, then sketch)

Before writing any HTML, sketch the **component vocabulary** the site
needs. Print the sketch to chat as plain prose — don't dump straight
into a file. Example shape:

> **Site shell**: top-bar with KMS name + meta; left sidebar with
> grouped page nav; main content column; footer with generated date.
>
> **Component vocabulary**:
> - `PageCard` — title, subtitle, meta row (created/updated)
> - `SectionLead` — large pull-quote intro on the landing view
> - `CitationMarker` — plain `[N]` superscript, non-interactive
>   (sources unread by default)
> - `WikilinkChip` — internal navigation chip with dotted underline
> - `Toc` — page list grouped by tag/topic when inferable
>
> **Design tokens**: serif body (Charter / Source Serif), sans UI
> (Inter), accent color tied to dominant theme (e.g. green for a
> coding KMS, indigo for research). Dark mode via
> `prefers-color-scheme`.

Keep it tight (½ screen of prose). Then go to Phase 3.

### Phase 3: Read remaining pages + assemble single file

1. **`KmsRead` every page** you haven't read yet. Capture:
   slug, title, body markdown, frontmatter. Pages are the substance
   of the site — embed every one.
2. **Sources stay closed.** Inline citations like `[1](../sources/x.md)`
   render as plain `[1]` markers. Skip the sources appendix
   entirely unless reading specific frontmatter genuinely improves
   the result (rare). When in doubt, don't read.
3. Compose ONE `index.html` containing:
   - Inline `<style>` with your component CSS
   - Inline `<script>` implementing **multi-view via JS hash
     routing** (`#/` overview, `#/page/<slug>`). No `#/sources`
     view by default.
   - JSON data island: `<script id="kms-data" type="application/json">`
     containing every **page** body + frontmatter — that's it. No
     source data.
   - Markdown rendering implemented client-side (write a small
     converter or use clever CSS on `<pre>` — your call, no CDN)
4. Write the file with the `Write` tool to
   `{output_dir}/index.html`.

## Hard rules

- **Pages only. Source files stay closed by default.** No bulk
  reads of `sources/*.md`. Open one only when a specific editorial
  question can't be answered from page content alone.
- **Citations render as plain `[N]` markers.** No source links by
  default.
- **Single file.** No external CSS/JS, no CDN, no `fetch`. Double-
  click-to-open works offline.
- **Embed all page bodies** in the JSON island. The site is
  self-contained for the editorial knowledge.
- **Multi-view via JS routing.** Hash-based URLs so deep links work.
- **No frameworks.** Plain HTML + CSS + vanilla JS only.
- Don't write into the KMS directory. Output goes to `{output_dir}`
  exclusively.

## Aesthetic brief (the html-benefit ethos)

This is editorial, not documentation. Make it feel hand-crafted:
- Inline SVG when the data has structure (relationships, timelines,
  process diagrams). A simple SVG beats bullet lists.
- Confident type pairing. System fonts are fine — `ui-serif`,
  `Charter`, `Inter`. Real hierarchy.
- Color thoughtfully — primary, accent, two neutrals. Dark mode
  optional but appreciated.
- Subtle motion: hover states, view-switch transitions, scroll
  anchoring. Restraint.
- Mobile-friendly — responsive with CSS, not JS.
- Citations rendered as actual chips (not raw `[1]` text); wikilinks
  rendered with their own affordance separate from external links.

## Final report

After the Write call succeeds, print one short message:

```
✓ wrote {output_dir}/index.html (<bytes> bytes)
  pages: <N>   sources: <M>
  components: <comma-separated list of component names you built>
  open with: open {output_dir}/index.html
```

Stop after the file is written. Don't loop, don't ask follow-ups.
