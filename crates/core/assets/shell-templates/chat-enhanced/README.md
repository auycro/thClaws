# Chat Enhanced

Conversational shell with quick-prompt chips and streaming bubbles. Edit QUICK_PROMPTS to seed your own one-tap actions.

## Customise

Edit `index.html` — search the file for `Author:` comments for places
to swap in your own copy, fields, or prompt templates. The shell talks
to the agent via the documented `window.thclaws.*` bridge.

## Preview locally

```bash
thclaws shell preview .
```

Opens a hot-reloading dev server with a mock agent so you can iterate on
the UI without wiring up a real session. Edit `mock.json` to script
agent responses.
