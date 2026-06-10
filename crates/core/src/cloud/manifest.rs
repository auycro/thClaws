//! Agent package manifest — Rust mirror of `manifest.schema.json`.
//! Server-side re-validates against the JSON Schema; CLI uses these
//! types to parse + emit `manifest.json`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProviderKey {
    pub name: String,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Pricing {
    #[serde(default)]
    pub purchase_usd: f64,
    #[serde(default = "default_true")]
    pub rentable: bool,
    #[serde(default = "default_min_rental_share")]
    pub min_rental_share: f64,
}

fn default_min_rental_share() -> f64 {
    0.05
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thclaws_min_version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provider_keys: Vec<ProviderKey>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_mb_estimate: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    #[serde(default = "default_fs_scope")]
    pub filesystem_scope: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub network_outbound: Vec<String>,
    #[serde(default = "default_shell_exec")]
    pub shell_execution: String,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            filesystem_scope: default_fs_scope(),
            network_outbound: Vec::new(),
            shell_execution: default_shell_exec(),
        }
    }
}

fn default_fs_scope() -> String {
    "workspace".to_string()
}

fn default_shell_exec() -> String {
    "sandboxed".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Preview {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub demo_session_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample_prompts: Vec<String>,
}

/// Catalog-side metadata about an agent's GUI Shell (dev-plan/39 Tier 1).
/// Distinct from `gui_shell::manifest::ShellManifest` which describes the
/// shell ITSELF (entry, bridge version, runtime permissions). This block
/// lives in the agent's `manifest.json` and tells the catalog how to
/// market the shell (screenshots, copy) + hosted runners how to route to
/// it. `screenshots` paths resolve inside the agent tarball.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CatalogShellMeta {
    /// Short marketing description shown next to screenshots. Falls
    /// back to `manifest.description` when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Screenshot paths relative to the agent folder root (e.g.
    /// `shells/dashboard/screenshots/01.png`). Catalog surfaces these as
    /// the detail-page hero carousel when present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub screenshots: Vec<String>,
    /// Future-proof permission strings (dev-plan/39 Tier 3 enforces).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    /// Server-assigned UUID (dev-plan/34 Option A). Present in
    /// tarballs after the agent has been published at least once.
    /// On `cloud get` the CLI strips it from the on-disk settings.json
    /// so the recipient starts as an unbound copy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default = "default_license")]
    pub license: String,
    #[serde(default)]
    pub pricing: Pricing,
    #[serde(default)]
    pub requires: Requires,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub preview: Preview,
    /// Shell id (relative to the agent folder root, e.g.
    /// `shells/dashboard`) that should be served at the workspace's
    /// root URL instead of the default chat UI. None = chat at root
    /// (current behavior). dev-plan/39 Tier 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_shell: Option<String>,
    /// Relative path inside the agent folder to a 16:9 cover image
    /// (PNG/JPG/WebP). The catalog renders this as the detail-page
    /// hero banner + browse-grid card thumbnail. Cap: 5 MB after
    /// gzipping. Absent → the catalog falls back to a generated
    /// monogram tile (or the gemini-image backfill fills it in).
    ///
    /// Server reads `raw_manifest["featured_image"]` post-publish and
    /// uploads the extracted file to S3 under
    /// `agents/<slug>/featured/<sha>.<ext>` — see
    /// `thclaws-cloud/api/src/thclaws_cloud/routers/agents.py:495`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub featured_image: Option<String>,
    /// Catalog-facing shell metadata — screenshots, marketing copy,
    /// declared permissions. dev-plan/39 Tier 1 (catalog surfacing) +
    /// Tier 3 (permission enforcement).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<CatalogShellMeta>,
}

fn default_license() -> String {
    "proprietary".to_string()
}

impl Manifest {
    pub fn from_path(path: &std::path::Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {}", path.display(), e))?;
        serde_json::from_str(&raw).map_err(|e| format!("parsing {}: {}", path.display(), e))
    }

    /// Build the "for-the-wire" manifest the CLI ships in a tarball at
    /// publish time. Identity (id, name, description, uuid) comes from
    /// `./.thclaws/settings.json::agent`; catalog metadata (version,
    /// pricing, requires, etc.) comes from the on-disk `manifest.json`
    /// (which the project may keep slim — without identity fields).
    pub fn fuse_for_publish(
        agent: &crate::config::AgentConfig,
        local_manifest_path: &std::path::Path,
    ) -> Result<Self, String> {
        let id = agent
            .id
            .clone()
            .ok_or_else(|| "settings.json::agent.id is required to publish".to_string())?;
        let name = agent
            .name
            .clone()
            .ok_or_else(|| "settings.json::agent.name is required to publish".to_string())?;
        let description = agent
            .description
            .clone()
            .ok_or_else(|| "settings.json::agent.description is required to publish".to_string())?;

        // The on-disk manifest.json supplies catalog fields. Parsed as a
        // JSON Value first so missing-identity fields (the post-Option-A
        // shape) deserialize cleanly even though `Manifest` declares
        // them as required — we fill those in from `agent` here.
        let raw = std::fs::read_to_string(local_manifest_path).map_err(|e| {
            format!(
                "reading {}: {} (publish requires a manifest.json — at minimum {{\"version\": \"0.1.0\"}})",
                local_manifest_path.display(),
                e
            )
        })?;
        let mut v: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| format!("parsing {}: {}", local_manifest_path.display(), e))?;
        let obj = v
            .as_object_mut()
            .ok_or_else(|| "manifest.json must be a JSON object".to_string())?;
        obj.insert("id".to_string(), serde_json::Value::String(id));
        obj.insert("name".to_string(), serde_json::Value::String(name));
        obj.insert(
            "description".to_string(),
            serde_json::Value::String(description),
        );
        if let Some(uuid) = &agent.uuid {
            obj.insert("uuid".to_string(), serde_json::Value::String(uuid.clone()));
        } else {
            obj.remove("uuid");
        }
        let fused: Manifest =
            serde_json::from_value(v).map_err(|e| format!("fusing manifest for publish: {e}"))?;
        fused.validate_basic()?;
        Ok(fused)
    }

    pub fn validate_basic(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("manifest.id is empty".into());
        }
        let id_re = regex::Regex::new(r"^[a-z0-9](?:[a-z0-9-]{0,62}[a-z0-9])?$").unwrap();
        if !id_re.is_match(&self.id) {
            return Err(format!(
                "manifest.id '{}' must be lowercase letters/digits/hyphens, ≤64 chars",
                self.id
            ));
        }
        let ver_re = regex::Regex::new(r"^\d+\.\d+\.\d+(?:-[A-Za-z0-9.-]+)?$").unwrap();
        if !ver_re.is_match(&self.version) {
            return Err(format!(
                "manifest.version '{}' must be semver",
                self.version
            ));
        }
        if self.name.is_empty() || self.name.len() > 80 {
            return Err("manifest.name must be 1–80 chars".into());
        }
        if self.description.is_empty() || self.description.len() > 500 {
            return Err("manifest.description must be 1–500 chars".into());
        }
        Ok(())
    }
}
