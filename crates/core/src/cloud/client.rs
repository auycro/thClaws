//! HTTP client for the thClaws.cloud catalog backend.

use serde::{Deserialize, Serialize};

pub struct Client {
    base_url: String,
    http: reqwest::Client,
    token: Option<String>,
}

impl Client {
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .user_agent(concat!("thclaws-cli/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
            token,
        }
    }

    fn auth_header(&self) -> Result<String, String> {
        let t = self
            .token
            .as_deref()
            .ok_or("not logged in — paste your CLI token in Settings → thClaws.cloud (mint one at /dashboard)")?;
        Ok(format!("Bearer {}", t))
    }

    pub async fn me(&self) -> Result<Me, String> {
        let auth = self.auth_header()?;
        let res = self
            .http
            .get(format!("{}/api/auth/me", self.base_url))
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode: {}", e))
    }

    pub async fn list_agents(&self, mine: bool) -> Result<Vec<AgentSummary>, String> {
        let mut url = format!("{}/api/agents", self.base_url);
        if mine {
            url.push_str("?mine=true");
        }
        let mut req = self.http.get(&url);
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {}", t));
        }
        let res = req.send().await.map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode: {}", e))
    }

    pub async fn publish(&self, tarball: Vec<u8>) -> Result<PublishResult, String> {
        let auth = self.auth_header()?;
        let part = reqwest::multipart::Part::bytes(tarball)
            .file_name("agent.tar.gz")
            .mime_str("application/gzip")
            .map_err(|e| format!("mime: {}", e))?;
        let form = reqwest::multipart::Form::new().part("file", part);

        let res = self
            .http
            .post(format!("{}/api/agents/publish", self.base_url))
            .header("Authorization", auth)
            .multipart(form)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        res.json().await.map_err(|e| format!("decode: {}", e))
    }

    pub async fn download(
        &self,
        slug: &str,
        version: Option<&str>,
    ) -> Result<DownloadResult, String> {
        let auth = self.auth_header()?;
        let mut url = format!("{}/api/agents/{}/download", self.base_url, slug);
        if let Some(v) = version {
            url.push_str(&format!("?version={}", urlencoding::encode(v)));
        }
        let res = self
            .http
            .get(&url)
            .header("Authorization", auth)
            .send()
            .await
            .map_err(|e| format!("network: {}", e))?;
        if !res.status().is_success() {
            return Err(format!(
                "status {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            ));
        }
        let version = res
            .headers()
            .get("x-agent-version")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let sha256 = res
            .headers()
            .get("x-agent-sha256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let uuid = res
            .headers()
            .get("x-agent-uuid")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let bytes = res
            .bytes()
            .await
            .map_err(|e| format!("body: {}", e))?
            .to_vec();
        Ok(DownloadResult {
            version,
            sha256,
            uuid,
            bytes,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Me {
    pub email: String,
    pub display_name: Option<String>,
    pub can_publish: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSummary {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub categories: Vec<String>,
    pub tags: Vec<String>,
    pub current_version: Option<String>,
    pub purchase_usd: f64,
    pub author_handle: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishResult {
    pub slug: String,
    pub version: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub url: String,
    /// Server-assigned UUID for this agent. Stable across versions and
    /// across folder renames — the CLI writes this back to
    /// `./.thclaws/settings.json::agent.uuid` so re-publish from the
    /// same folder targets the same catalog entry.
    pub uuid: String,
}

#[derive(Debug)]
pub struct DownloadResult {
    pub version: String,
    pub sha256: String,
    /// Server-authoritative UUID from the `X-Agent-UUID` header.
    /// Preferred over peeking inside the tarball — the on-disk
    /// manifest.json may pre-date the Option-A identity split.
    pub uuid: Option<String>,
    pub bytes: Vec<u8>,
}
