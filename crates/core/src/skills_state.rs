//! Skill-state broadcaster — the channel that `SkillTool::call` uses
//! to ask the worker to apply a skill's recommended model for the
//! duration of the current turn.
//!
//! Lifecycle:
//! 1. Worker (GUI / serve) registers a resolver via `set_resolver`
//!    that captures `events_tx` + the agent's `model_override` Arc.
//! 2. SkillTool resolves a skill that has `model:` in its frontmatter,
//!    calls `request_model(spec)` with the parsed candidates.
//! 3. The resolver iterates candidates, picks the first one the user
//!    has an API key for (`ProviderKind::has_key_available`), writes
//!    it into `model_override`, and emits a chat status event.
//! 4. The agent's run_turn reads `model_override` at the top of every
//!    iteration's request build, so the very next provider.stream
//!    call uses the recommended model.
//! 5. Agent clears `model_override` at end of run_turn so the next
//!    turn starts from the user's baseline model. Worker emits a
//!    revert chat status when it sees the Done event for a turn that
//!    had a swap active.
//!
//! CLI / non-GUI surfaces don't register a resolver — the SkillTool
//! call is a no-op signal in that case, the user keeps their current
//! model, and a one-line note is appended to the skill body so the
//! model can suggest a manual `/model` switch if it wants to.

use crate::skills::SkillModelSpec;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Per-turn flag set by the resolver when it actually applies an
/// override. The worker reads this when it observes `AgentEvent::Done`
/// to decide whether to emit a "[model → X (skill ended)]" revert
/// note, then clears it. Single-threaded relative to a turn (the
/// agent loop is sequential), so a plain AtomicBool is enough.
static SWAP_ACTIVE_THIS_TURN: AtomicBool = AtomicBool::new(false);

/// settings.json overrides keyed by skill name. The worker populates
/// this at boot (and on `/config` reloads) by mapping per-skill named
/// fields like `AppConfig::extract_save_skill_models` to their skill
/// names (`"extract-and-save"`). When a SkillTool resolves a skill
/// with a `model:` recommendation, the request_model resolver checks
/// this map first and uses the override spec if present, falling
/// back to the embedded SKILL.md frontmatter only when nothing was
/// configured.
fn skill_overrides() -> &'static Mutex<HashMap<String, SkillModelSpec>> {
    static M: OnceLock<Mutex<HashMap<String, SkillModelSpec>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Replace the entire skill-override map with the supplied entries.
/// Called by the worker after loading settings. Recovers from mutex
/// poisoning so a panic elsewhere can't silently disable overrides.
pub fn set_skill_overrides(overrides: HashMap<String, SkillModelSpec>) {
    let mut g = skill_overrides().lock().unwrap_or_else(|p| p.into_inner());
    *g = overrides;
}

/// Read the override for a given skill name (returns None if no
/// settings.json override is configured for it).
pub fn skill_override(name: &str) -> Option<SkillModelSpec> {
    let g = skill_overrides().lock().unwrap_or_else(|p| p.into_inner());
    g.get(name).cloned()
}

/// Mark that a skill applied a model override this turn. The
/// resolver in `shared_session` calls this after writing into the
/// agent's `model_override` slot.
pub fn mark_swap_active() {
    SWAP_ACTIVE_THIS_TURN.store(true, Ordering::SeqCst);
}

/// Read-and-clear the swap-active flag. Worker calls this when it
/// sees a turn end so the next turn starts with a clean state.
/// Returns the value as it was before clearing.
pub fn take_swap_active() -> bool {
    SWAP_ACTIVE_THIS_TURN.swap(false, Ordering::SeqCst)
}

/// What the resolver returned to the SkillTool. The tool uses this to
/// shape the one-line note appended to the skill body — visible to
/// the model so it can mention the swap (or its absence) in its
/// response.
#[derive(Debug, Clone, PartialEq)]
pub enum SkillModelOutcome {
    /// The resolver picked a model and wrote it into the override
    /// slot. Carries the chosen model name so the note can name it.
    Switched(String),
    /// None of the candidates had an available key. Carries the
    /// first candidate so the note can suggest the user add a key
    /// for it.
    KeptCurrent { recommended: String },
    /// No resolver registered (e.g. CLI surface). Effectively the
    /// same UX as KeptCurrent but reported separately so the worker
    /// can keep CLI output quiet without spurious warnings.
    NoResolver,
}

type Resolver = Box<dyn Fn(&SkillModelSpec) -> SkillModelOutcome + Send + Sync>;

fn resolver() -> &'static Mutex<Option<Resolver>> {
    static R: OnceLock<Mutex<Option<Resolver>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(None))
}

/// Register the skill-model resolver. Replaces any prior registration
/// — there's only one active worker per process. The closure should:
/// (a) iterate spec.candidates() for the first model with a usable
/// key, (b) write that model into the agent's `model_override` slot,
/// (c) emit a chat status `ViewEvent` so the user sees the switch.
pub fn set_resolver<F>(f: F)
where
    F: Fn(&SkillModelSpec) -> SkillModelOutcome + Send + Sync + 'static,
{
    if let Ok(mut g) = resolver().lock() {
        *g = Some(Box::new(f));
    }
}

/// Called by `SkillTool::call` whenever it loads a skill whose
/// frontmatter carries a `model:` recommendation. Recovers from
/// mutex poisoning so a panic elsewhere can't silently disable the
/// resolver — same posture as `plan_state::fire`.
pub fn request_model(spec: &SkillModelSpec) -> SkillModelOutcome {
    let g = resolver().lock().unwrap_or_else(|p| p.into_inner());
    match g.as_ref() {
        Some(f) => f(spec),
        None => SkillModelOutcome::NoResolver,
    }
}
