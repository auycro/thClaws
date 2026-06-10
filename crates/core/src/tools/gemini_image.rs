//! `TextToImage` + `ImageToImage` — native Gemini image generation.
//!
//! Calls `generativelanguage.googleapis.com/v1beta/models/<model>:generateContent`
//! directly (no provider abstraction — these tools are scope-limited
//! to Gemini image models). Models:
//!
//!   - `gemini-3.1-flash-image` (default; faster, cheaper)
//!   - `gemini-3.1-pro-image`   (higher fidelity, more expensive)
//!
//! Both tools are **opt-in**: they're registered only when
//! `imageToolsEnabled: true` is set in `.thclaws/settings.json` AND
//! the user's `GEMINI_API_KEY` (or `GOOGLE_API_KEY`) is present in
//! env. The settings flag is the user's "yes I want these surfaces"
//! signal; the env-key check is `requires_env()`'s standard hide-
//! when-credentials-missing pattern (same as HAL tools).
//!
//! Output: image bytes are written to `output/img-<YYYYMMDD-HHMMSS>-
//! <sha8>.png` inside the workspace and the tool returns a multimodal
//! `ToolResultContent::Blocks` carrying both a text summary (so non-
//! vision models still see the artifact) and an Image block (so
//! vision models can immediately reason about the result).
//!
//! `image-to-image` is just edit mode — pass an existing image path
//! alongside the prompt and Gemini transforms it. Same response
//! shape; same output dir.

use crate::error::{Error, Result};
use crate::tools::{req_str, Tool};
use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use reqwest::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::Duration;

const GEMINI_BASE: &str = "https://generativelanguage.googleapis.com";
const DEFAULT_MODEL: &str = "gemini-3.1-flash-image";
const PRO_MODEL: &str = "gemini-3.1-pro-image";

/// Resolve the API key from env. Returns Err if neither
/// `GEMINI_API_KEY` nor `GOOGLE_API_KEY` is set with a non-empty
/// value (after trim + wrapping-quote strip — same defensive
/// sanitisation `api_key_from_env` runs for provider keys).
fn resolve_key() -> Result<String> {
    for var in ["GEMINI_API_KEY", "GOOGLE_API_KEY"] {
        if let Ok(raw) = std::env::var(var) {
            let trimmed = raw.trim();
            let bytes = trimmed.as_bytes();
            let cleaned = if bytes.len() >= 2
                && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
                    || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
            {
                &trimmed[1..trimmed.len() - 1]
            } else {
                trimmed
            };
            if !cleaned.is_empty() {
                return Ok(cleaned.to_string());
            }
        }
    }
    Err(Error::Tool(
        "GEMINI_API_KEY (or GOOGLE_API_KEY) not set — required for native image-gen tools".into(),
    ))
}

/// Resolve `(base_url, api_key)` for the Gemini image API. A real
/// native key calls `generativelanguage` directly. When the key is
/// absent or the hosted-runner placeholder (`gateway-placeholder`)
/// and a thClaws Gateway access key is present, route through
/// `<gateway>/google` — the gateway injects the real upstream key
/// and meters the call against the `google/gemini-3.1-*-image`
/// model_pricing rows. The auth header stays `x-goog-api-key`; the
/// gateway accepts that scheme as the access-key carrier.
fn resolve_endpoint() -> Result<(String, String)> {
    if let Ok(key) = resolve_key() {
        if key != "gateway-placeholder" {
            return Ok((GEMINI_BASE.to_string(), key));
        }
    }
    let gw_key = std::env::var("THCLAWS_GATEWAY_API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(gw_key) = gw_key {
        let base = std::env::var("THCLAWS_GATEWAY_BASE_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                crate::providers::thclaws_gateway::GATEWAY_BASE_URL.to_string()
            });
        return Ok((format!("{base}/google"), gw_key));
    }
    resolve_key().map(|k| (GEMINI_BASE.to_string(), k))
}

/// Pick the Gemini image model from the input. Allows the user to
/// say "flash" / "pro" as shortcuts, or pass the full model id, or
/// omit for the default. Anything not in {flash, pro, full-id} is
/// rejected so the tool fails fast on typos.
fn resolve_model(input: &Value) -> Result<String> {
    let raw = input.get("model").and_then(|v| v.as_str()).unwrap_or("");
    Ok(match raw {
        "" | "flash" | "gemini-3.1-flash-image" => DEFAULT_MODEL.into(),
        "pro" | "gemini-3.1-pro-image" => PRO_MODEL.into(),
        other => {
            if other.starts_with("gemini-") && other.contains("image") {
                other.to_string()
            } else {
                return Err(Error::Tool(format!(
                    "unknown model {other:?} — expected one of: flash, pro, gemini-3.1-flash-image, gemini-3.1-pro-image"
                )));
            }
        }
    })
}

/// Aspect-ratio + size whitelist mirrors what the Gemini image API
/// currently accepts. Defaults: 16:9 + 1K (good tradeoff between
/// quality + token cost; ~5–8s wall time on flash).
fn resolve_aspect(input: &Value) -> &'static str {
    match input
        .get("aspect_ratio")
        .and_then(|v| v.as_str())
        .unwrap_or("16:9")
    {
        "1:1" => "1:1",
        "3:4" => "3:4",
        "4:3" => "4:3",
        "9:16" => "9:16",
        _ => "16:9",
    }
}
fn resolve_size(input: &Value) -> &'static str {
    match input.get("size").and_then(|v| v.as_str()).unwrap_or("1K") {
        "512" => "512",
        "2K" => "2K",
        _ => "1K",
    }
}

/// POST to `models/<id>:generateContent`, return the first
/// `inlineData` part's bytes. `parts` is the list of message parts
/// — text-only for generation, [image, text] for edit-mode. Times
/// out at 120s (image gen averages 5–15s; this leaves headroom for
/// pro-model + larger sizes).
async fn call_gemini_image(
    base_url: &str,
    api_key: &str,
    model: &str,
    parts: Vec<Value>,
    aspect_ratio: &str,
    size: &str,
) -> Result<Vec<u8>> {
    let body = json!({
        "contents": [{ "parts": parts }],
        "generationConfig": {
            "responseModalities": ["IMAGE"],
            "imageConfig": {
                "aspectRatio": aspect_ratio,
                "imageSize": size,
            }
        }
    });
    let url = format!(
        "{}/v1beta/models/{}:generateContent",
        base_url.trim_end_matches('/'),
        model
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| Error::Tool(format!("http client: {e}")))?;
    let resp = client
        .post(&url)
        .header("x-goog-api-key", api_key)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("http: {e}")))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::Tool(format!(
            "gemini http {status}: {}",
            &body[..body.len().min(400)]
        )));
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| Error::Tool(format!("gemini response not json: {e}")))?;
    let parts = v
        .pointer("/candidates/0/content/parts")
        .and_then(|p| p.as_array())
        .ok_or_else(|| Error::Tool("gemini response missing /candidates/0/content/parts".into()))?;
    for part in parts {
        if let Some(data_b64) = part.pointer("/inlineData/data").and_then(|v| v.as_str()) {
            return B64
                .decode(data_b64)
                .map_err(|e| Error::Tool(format!("base64 decode: {e}")));
        }
    }
    Err(Error::Tool(
        "gemini returned no inlineData part — refusal or quota hit?".into(),
    ))
}

/// Detect the actual image format from magic bytes. Gemini's
/// `inlineData` is not always PNG — flash frequently returns JPEG —
/// and labeling JPEG bytes `image/png` makes Anthropic reject the
/// tool-result image block ("media type … appears to be image/jpeg").
fn sniff_ext(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if bytes.len() > 11 && &bytes[8..12] == b"WEBP" {
        "webp"
    } else {
        "png"
    }
}

/// Save image bytes under `output/img-<ts>-<sha8>.<ext>` and return
/// the relative path. Always creates the output dir; never overwrites
/// (sha-prefix in the filename makes collisions vanishingly unlikely).
fn save_image(bytes: &[u8], ext: &str) -> Result<PathBuf> {
    let sha = Sha256::digest(bytes);
    let sha_hex = format!("{:02x}{:02x}{:02x}{:02x}", sha[0], sha[1], sha[2], sha[3]);
    // `chrono::Utc::now()` rather than `SystemTime` so the filename
    // stamp is human-readable. Local-time would be tempting but
    // produces non-monotonic sort order across DST transitions.
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let name = format!("img-{ts}-{sha_hex}.{ext}");
    let dir = std::path::Path::new("output");
    std::fs::create_dir_all(dir).map_err(|e| Error::Tool(format!("mkdir output: {e}")))?;
    let path = dir.join(&name);
    std::fs::write(&path, bytes)
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(path)
}

/// Build the multimodal `ToolResultContent::Blocks` for an image
/// result — a text summary line + the inline image. The text line
/// is what compaction sees + what non-vision models can reason
/// about; the Image block carries the pixels for vision models.
fn build_image_result(bytes: Vec<u8>, path: &std::path::Path) -> ToolResultContent {
    let summary = format!(
        "Wrote {} ({} bytes, sha256-4={:02x}{:02x}{:02x}{:02x})",
        path.display(),
        bytes.len(),
        Sha256::digest(&bytes)[0],
        Sha256::digest(&bytes)[1],
        Sha256::digest(&bytes)[2],
        Sha256::digest(&bytes)[3],
    );
    let media_type = match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    };
    ToolResultContent::Blocks(vec![
        ToolResultBlock::Text { text: summary },
        ToolResultBlock::Image {
            source: ImageSource::Base64 {
                media_type: media_type.into(),
                data: B64.encode(&bytes),
            },
        },
    ])
}

// ─── TextToImage ─────────────────────────────────────────────────

pub struct TextToImageTool;

#[async_trait]
impl Tool for TextToImageTool {
    fn name(&self) -> &'static str {
        "TextToImage"
    }
    fn description(&self) -> &'static str {
        "Generate a brand-new image from a text prompt via the native Gemini \
         image-generation API. Output is written to `output/img-<timestamp>-<sha8>.png` \
         and returned inline as an image block. Requires `GEMINI_API_KEY` (or \
         `GOOGLE_API_KEY`) in env and `imageToolsEnabled: true` in \
         `.thclaws/settings.json` — the tool stays hidden otherwise. \
         Models: `flash` (default; gemini-3.1-flash-image — faster) or `pro` \
         (gemini-3.1-pro-image — higher fidelity). Aspect ratios: 1:1, 3:4, \
         4:3, 9:16, 16:9 (default). Sizes: 512, 1K (default), 2K. Cost: a \
         single 1K image is roughly $0.01–0.03 depending on model."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Description of the image to generate. Be specific about subject, style, composition, colors. Gemini follows long prompts well — a 2–3 sentence brief beats a one-word tag."
                },
                "model": {
                    "type": "string",
                    "description": "Which Gemini image model. Accepts the shortcuts `flash` / `pro`, or the full id (gemini-3.1-flash-image / gemini-3.1-pro-image). Default: flash.",
                    "enum": ["flash", "pro", "gemini-3.1-flash-image", "gemini-3.1-pro-image"]
                },
                "aspect_ratio": {
                    "type": "string",
                    "description": "Output aspect ratio. Default 16:9.",
                    "enum": ["1:1", "3:4", "4:3", "9:16", "16:9"]
                },
                "size": {
                    "type": "string",
                    "description": "Output size (long-edge resolution tier). Default 1K.",
                    "enum": ["512", "1K", "2K"]
                }
            },
            "required": ["prompt"]
        })
    }
    fn requires_env(&self) -> &'static [&'static str] {
        &["GEMINI_API_KEY"]
    }
    fn requires_approval(&self, _input: &Value) -> bool {
        // Costs money + writes a file. Both reasons to gate on the
        // user's permission mode — same risk profile as `WebFetch`
        // hitting a paid API.
        true
    }
    async fn call(&self, input: Value) -> Result<String> {
        // Fall-through text path — agent's surface might not render
        // the multimodal Blocks variant. Just return the saved path.
        let result = self.call_multimodal(input).await?;
        Ok(result.to_text())
    }
    async fn call_multimodal(&self, input: Value) -> Result<ToolResultContent> {
        let prompt = req_str(&input, "prompt")?;
        let model = resolve_model(&input)?;
        let aspect = resolve_aspect(&input);
        let size = resolve_size(&input);
        let (base, key) = resolve_endpoint()?;
        let parts = vec![json!({ "text": prompt })];
        let bytes = call_gemini_image(&base, &key, &model, parts, aspect, size).await?;
        let path = save_image(&bytes, sniff_ext(&bytes))?;
        Ok(build_image_result(bytes, &path))
    }
}

// ─── ImageToImage ────────────────────────────────────────────────

pub struct ImageToImageTool;

#[async_trait]
impl Tool for ImageToImageTool {
    fn name(&self) -> &'static str {
        "ImageToImage"
    }
    fn description(&self) -> &'static str {
        "Edit or transform an existing image using a text prompt via the native \
         Gemini image-generation API (edit mode). Pass `input_path` (a path under \
         the workspace) + a `prompt` describing the change. Gemini reads the input \
         image, applies the edit, and writes a fresh PNG to `output/img-<ts>-<sha8>.png`. \
         Use this for: background removal, style transfer, adding/removing elements, \
         lighting changes, text overlays. Same env + settings gating as TextToImage. \
         Models: `flash` (default) or `pro`. Tip: edits with strict 'keep everything \
         else identical' clauses preserve composition best."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "input_path": {
                    "type": "string",
                    "description": "Path to the source image inside the workspace. Supported: PNG, JPEG, WebP."
                },
                "prompt": {
                    "type": "string",
                    "description": "What to change. Be explicit about what should STAY the same — e.g. 'Add the title \"Q4 Review\" in white sans-serif at the top. Keep everything else identical.' Vague prompts produce drift."
                },
                "model": {
                    "type": "string",
                    "enum": ["flash", "pro", "gemini-3.1-flash-image", "gemini-3.1-pro-image"]
                },
                "aspect_ratio": {
                    "type": "string",
                    "enum": ["1:1", "3:4", "4:3", "9:16", "16:9"]
                },
                "size": {
                    "type": "string",
                    "enum": ["512", "1K", "2K"]
                }
            },
            "required": ["input_path", "prompt"]
        })
    }
    fn requires_env(&self) -> &'static [&'static str] {
        &["GEMINI_API_KEY"]
    }
    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }
    async fn call(&self, input: Value) -> Result<String> {
        let result = self.call_multimodal(input).await?;
        Ok(result.to_text())
    }
    async fn call_multimodal(&self, input: Value) -> Result<ToolResultContent> {
        let input_path_raw = req_str(&input, "input_path")?;
        let prompt = req_str(&input, "prompt")?;
        let model = resolve_model(&input)?;
        let aspect = resolve_aspect(&input);
        let size = resolve_size(&input);
        let (base, key) = resolve_endpoint()?;

        // Sandbox-check the input path so the agent can't smuggle
        // an arbitrary system file into Gemini's context. Same
        // contract every file-touching tool uses.
        let abs = crate::sandbox::Sandbox::check(input_path_raw)
            .map_err(|e| Error::Tool(format!("input_path: {e}")))?;
        let in_bytes =
            std::fs::read(&abs).map_err(|e| Error::Tool(format!("read {}: {e}", abs.display())))?;
        let in_mime = match abs
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            _ => "image/png",
        };
        let parts = vec![
            json!({
                "inlineData": {
                    "mimeType": in_mime,
                    "data": B64.encode(&in_bytes),
                }
            }),
            json!({ "text": prompt }),
        ];
        let out_bytes = call_gemini_image(&base, &key, &model, parts, aspect, size).await?;
        let out_path = save_image(&out_bytes, sniff_ext(&out_bytes))?;
        Ok(build_image_result(out_bytes, &out_path))
    }
}
