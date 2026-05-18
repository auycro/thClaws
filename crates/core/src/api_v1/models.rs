//! `GET /v1/models` — OpenAI-compatible model list.
//!
//! Walks the [`crate::model_catalogue::EffectiveCatalogue`] (cache layer
//! → embedded baseline) and emits one row per known model id. `owned_by`
//! is the provider name as the catalogue records it
//! (`anthropic`, `openai`, `openrouter`, `gemini`, etc.).
//!
//! No network calls — pure local lookup. Clients that need fresh data
//! refresh the catalogue via `/models refresh` in the REPL/GUI.

use axum::response::Json;
use serde::Serialize;

use super::AuthOk;
use crate::model_catalogue::EffectiveCatalogue;

#[derive(Serialize)]
pub struct ModelListResponse {
    pub object: &'static str,
    pub data: Vec<ModelRow>,
}

#[derive(Serialize)]
pub struct ModelRow {
    pub id: String,
    pub object: &'static str,
    pub created: i64,
    pub owned_by: String,
}

pub async fn list_models(_auth: AuthOk) -> Json<ModelListResponse> {
    let cat = EffectiveCatalogue::load();
    let created = chrono::Utc::now().timestamp();
    let mut rows: Vec<ModelRow> = Vec::new();

    // Walk cache first (preferred), then fall back to baseline for ids
    // only the baseline knows about. Dedupe by id.
    let mut seen = std::collections::HashSet::new();
    let layers = [cat.cache.as_ref(), Some(&cat.baseline)];
    for layer in layers.into_iter().flatten() {
        for (provider_name, provider_cat) in &layer.providers {
            for (model_id, entry) in &provider_cat.models {
                if !seen.insert(model_id.clone()) {
                    continue;
                }
                // Skip non-chat rows (embeddings, audio, image) so the
                // list matches what a chat client can actually call.
                if entry.chat == Some(false) {
                    continue;
                }
                rows.push(ModelRow {
                    id: model_id.clone(),
                    object: "model",
                    created,
                    owned_by: provider_name.clone(),
                });
            }
        }
    }

    // Sort by id so clients see a stable, alphabetical list — matches
    // OpenAI's own list ordering and makes diffs easy to spot.
    rows.sort_by(|a, b| a.id.cmp(&b.id));

    Json(ModelListResponse {
        object: "list",
        data: rows,
    })
}
