//! `analyze_screen` — capture the TV screen and return a validated six-field
//! JSON description of it, as a native ZeroClaw [`Tool`].
//!
//! This replaces the out-of-process MCP server (`tv-screen-tools`): no stdio
//! child, no JSON-RPC framing, no second armv7 binary to deploy and keep in
//! sync with the gateway.
//!
//! # Why the image never reaches the agent
//!
//! [`execute`] returns *only* the six-field JSON as `ToolResult::output`. The
//! capture is read, encoded, and sent to the VLM entirely inside this tool; the
//! base64 payload is never surfaced to the model. This is deliberate — the
//! whole latency win came from keeping large payloads out of the agent's
//! context, and ZeroClaw 0.8.2 masks image paths as `[media attachment]` to a
//! non-vision LLM anyway, so returning them would cost tokens and buy nothing.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_config::policy::SecurityPolicy;

/// The six-field contract. Copied verbatim from the MCP server's
/// `DEFAULT_VLM_PROMPT` so both paths produce an identical schema — if these
/// ever diverge, the postprocess sidecar's `validate.rs` starts rejecting one
/// of them and the latency comparison stops meaning anything.
const DEFAULT_VLM_PROMPT: &str = "Analyze this TV screen: \
State the screen type. Identify brand logos, \
location, setting, named person if clearly recognizable \
and any products or visible advertisements. \
Reply with ONLY a JSON object, no other text, \
containing ALL SIX of these keys. The \"error\" key is REQUIRED on every reply - \
set it to null when there is no error. Never omit it:
    {
      \"screen_type\": \"one of: live_tv, app, menu, ad, game, unknown\",
      \"title\": \"short title of what is on screen\",
      \"summary\": \"max 2 sentences\",
      \"detected_elements\": [
          {\"name\": \"scoreboard\", \"description\": \"VT 1 - LOW, 2nd inning\", \"confidence\": 0.9},
          {\"name\": \"ad_banner\", \"description\": \"Pepsi advertisement on outside wall\", \"confidence\": 0.8}
      ],
      \"suggested_actions\": [\"list of actions viewers could take\"],
      \"error\": null
    }
Emit the JSON as a single line with NO NEWLINES and NO INDENTATION

Limits, strictly enforced:
1. detected_elements: AT MOST 3 items.
2. suggested_actions: AT MOST 3 items. Be creative, not generic TV volume recommendations. \
Make sure it is related to the detected elements/scenes. Each max 15 words.
3. Be terse. Do not explain your reasoning.
4. \"confidence\" is a bare number between 0 and 1, never a string.
5. \"error\" must be null, or an object {\"code\": \"...\", \"message\": \"...\"}. Never a bare string.
Do not wrap the JSON in markdown code fences. Do not add any text before or after it. ";

/// Runtime configuration, read from the gateway process environment.
///
/// Note this is the *gateway's* environment now, not `[mcp.servers.env]` in
/// `analyze-screen-config.toml` — that block only ever configured the stdio
/// child, which no longer exists. See the deployment notes in the runbook.
#[derive(Debug, Clone)]
pub struct AnalyzeScreenConfig {
    /// Absolute path to the capture binary (e.g. `/root/tzcapturesample`).
    pub capture_bin: PathBuf,
    /// Where the capture lands. The binary writes `./capture.png` relative to
    /// its *own* cwd, so this tool sets the child's `current_dir` to this
    /// path's parent rather than relying on the gateway's cwd.
    pub capture_output: PathBuf,
    pub capture_timeout_secs: u64,
    /// Base URL ending at `/v1`; `/chat/completions` is appended.
    pub vlm_base_url: String,
    pub vlm_model: String,
    pub vlm_timeout_secs: u64,
    pub vlm_prompt: String,
    pub downscale_max_edge: u32,
    pub downscale_enabled: bool,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

impl AnalyzeScreenConfig {
    pub fn from_env() -> Self {
        Self {
            capture_bin: PathBuf::from(env_or("SCREENSHOT_BIN", "/root/tzcapturesample")),
            capture_output: PathBuf::from(env_or("SCREENSHOT_OUTPUT", "/root/capture.png")),
            capture_timeout_secs: env_or("SCREENSHOT_TIMEOUT_SECS", "10").parse().unwrap_or(10),
            vlm_base_url: env_or("VLM_BASE_URL", ""),
            vlm_model: env_or("VLM_MODEL", "Qwen/Qwen3-VL-8B-Instruct"),
            vlm_timeout_secs: env_or("VLM_TIMEOUT_SECS", "60").parse().unwrap_or(60),
            vlm_prompt: env_or("VLM_PROMPT", DEFAULT_VLM_PROMPT),
            downscale_max_edge: env_or("DOWNSCALE_MAX_EDGE", "1280").parse().unwrap_or(1280),
            downscale_enabled: env_or("DOWNSCALE_ENABLED", "true") == "true",
        }
    }
}

/// Capture the current TV screen and describe it via a vision model.
pub struct AnalyzeScreenTool {
    security: Arc<SecurityPolicy>,
    cfg: AnalyzeScreenConfig,
}

impl AnalyzeScreenTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self {
            security,
            cfg: AnalyzeScreenConfig::from_env(),
        }
    }

    pub fn with_config(security: Arc<SecurityPolicy>, cfg: AnalyzeScreenConfig) -> Self {
        Self { security, cfg }
    }

    /// Run the capture binary and return the path to the fresh PNG.
    ///
    /// The child's cwd is pinned to the output directory because
    /// `tzcapturesample` writes `./capture.png` and ignores any argument
    /// telling it otherwise. Pinning it here removes the "every process must
    /// start from /root" landmine that the MCP version carried.
    async fn capture(&self) -> anyhow::Result<PathBuf> {
        let out = &self.cfg.capture_output;
        let dir = out.parent().unwrap_or_else(|| Path::new("/"));

        let before = tokio::fs::metadata(out)
            .await
            .ok()
            .and_then(|m| m.modified().ok());

        let result = tokio::time::timeout(
            Duration::from_secs(self.cfg.capture_timeout_secs),
            tokio::process::Command::new(&self.cfg.capture_bin)
                .current_dir(dir)
                .output(),
        )
        .await;

        let output = match result {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                anyhow::bail!("failed to spawn {}: {e}", self.cfg.capture_bin.display())
            }
            Err(_) => anyhow::bail!(
                "capture binary timed out after {}s",
                self.cfg.capture_timeout_secs
            ),
        };

        if !output.status.success() {
            anyhow::bail!(
                "capture binary exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        // Prefer a file whose mtime actually changed; fall back to mere
        // existence for filesystems with coarse timestamps.
        let after = tokio::fs::metadata(out)
            .await
            .ok()
            .and_then(|m| m.modified().ok());
        if after.is_some() && (before.is_none() || after != before) {
            return Ok(out.clone());
        }
        if out.is_file() {
            return Ok(out.clone());
        }
        anyhow::bail!("no PNG at {} after capture", out.display())
    }

    /// Read the capture, optionally downscale, and return PNG bytes.
    async fn prepare_image(&self, path: &Path) -> anyhow::Result<Vec<u8>> {
        let raw = tokio::fs::read(path)
            .await
            .map_err(|e| anyhow::anyhow!("reading capture at {}: {e}", path.display()))?;

        #[cfg(feature = "downscale")]
        if self.cfg.downscale_enabled {
            let max = self.cfg.downscale_max_edge;
            let bytes = raw.clone();
            // Decode/resize/encode is CPU-bound; keep it off the async worker.
            let resized = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Vec<u8>>> {
                let img = image::load_from_memory(&bytes)?;
                if img.width().max(img.height()) <= max {
                    return Ok(None);
                }
                let out = img.resize(max, max, image::imageops::FilterType::Triangle);
                let mut buf = std::io::Cursor::new(Vec::new());
                out.write_to(&mut buf, image::ImageFormat::Png)?;
                Ok(Some(buf.into_inner()))
            })
            .await??;
            if let Some(small) = resized {
                return Ok(small);
            }
        }

        Ok(raw)
    }

    /// POST the image to the VLM and return its raw text content.
    async fn analyze(&self, png: &[u8]) -> anyhow::Result<String> {
        if self.cfg.vlm_base_url.is_empty() {
            anyhow::bail!("VLM_BASE_URL is not configured");
        }
        let url = format!(
            "{}/chat/completions",
            self.cfg.vlm_base_url.trim_end_matches('/')
        );

        let b64 = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(png)
        };

        let body = json!({
            "model": self.cfg.vlm_model,
            "temperature": 0,
            "max_tokens": 400,
            // Qwen3.6 otherwise burns the entire token budget on reasoning
            // drafts and returns empty content. This one line was worth ~14s.
            "chat_template_kwargs": { "enable_thinking": false },
            "messages": [{ "role": "user", "content": [
                { "type": "image_url",
                  "image_url": { "url": format!("data:image/png;base64,{b64}") } },
                { "type": "text", "text": self.cfg.vlm_prompt }
            ]}]
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(self.cfg.vlm_timeout_secs))
            .build()?;

        let mut req = client.post(&url).json(&body);
        if let Ok(key) = std::env::var("VLM_API_KEY") {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await?;
        let status = resp.status();
        let payload: Value = resp.json().await?;
        if !status.is_success() {
            anyhow::bail!("VLM returned {status}: {payload}");
        }

        let content = payload["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();

        // Guard 1: empty content. Turning this from a silent failure into a
        // loud one is what made the original pipeline debuggable at all.
        if content.is_empty() {
            let finish = payload["choices"][0]["finish_reason"]
                .as_str()
                .unwrap_or("unknown");
            anyhow::bail!("VLM returned empty content (finish_reason={finish})");
        }

        // Guard 2: prose instead of JSON. Named here so the error points at
        // this layer rather than at the sidecar's validator downstream.
        let cleaned = strip_fences(&content);
        if !cleaned.starts_with('{') {
            anyhow::bail!("VLM returned non-JSON content: {}", truncate(&cleaned, 200));
        }

        Ok(cleaned)
    }
}

fn strip_fences(s: &str) -> String {
    s.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
        .to_string()
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

#[async_trait]
impl Tool for AnalyzeScreenTool {
    fn name(&self) -> &str {
        "analyze_screen"
    }

    fn description(&self) -> &str {
        "Capture the current TV screen and analyze it in one step. Takes no arguments. \
Returns a JSON object describing the screen: screen_type, title, summary, \
detected_elements, suggested_actions, error."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<ToolResult> {
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        let t0 = Instant::now();

        let path = match self.capture().await {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("capture failed: {e:#}")),
                })
            }
        };
        let capture_ms = t0.elapsed().as_millis();

        let png = match self.prepare_image(&path).await {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("image prepare failed: {e:#}")),
                })
            }
        };

        let vlm_start = Instant::now();
        match self.analyze(&png).await {
            Ok(text) => {
                eprintln!(
                    "[analyze_screen] capture={}ms vlm={}ms total={}ms",
                    capture_ms,
                    vlm_start.elapsed().as_millis(),
                    t0.elapsed().as_millis()
                );
                // Only the six-field JSON. Never the image.
                Ok(ToolResult {
                    success: true,
                    output: text,
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("VLM analysis failed: {e:#}")),
            }),
        }
    }
}

// ⚠️ SEAM: confirm the ToolKind variant. `ScreenshotTool` uses
// `ToolKind::Plugin`; check `crates/zeroclaw-api/src/attribution.rs` for a
// better-fitting variant (e.g. Vision) before settling on this.
zeroclaw_api::tool_attribution!(AnalyzeScreenTool, zeroclaw_api::attribution::ToolKind::Plugin);

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::autonomy::AutonomyLevel;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    #[test]
    fn tool_name() {
        let tool = AnalyzeScreenTool::new(test_security());
        assert_eq!(tool.name(), "analyze_screen");
    }

    #[test]
    fn tool_description_mentions_screen() {
        let tool = AnalyzeScreenTool::new(test_security());
        assert!(tool.description().contains("screen"));
    }

    #[test]
    fn tool_schema_takes_no_arguments() {
        let tool = AnalyzeScreenTool::new(test_security());
        let schema = tool.parameters_schema();
        assert!(schema["properties"].as_object().unwrap().is_empty());
    }

    #[test]
    fn tool_spec() {
        let tool = AnalyzeScreenTool::new(test_security());
        let spec = tool.spec();
        assert_eq!(spec.name, "analyze_screen");
        assert!(spec.parameters.is_object());
    }

    #[test]
    fn prompt_declares_all_six_keys() {
        for key in [
            "screen_type",
            "title",
            "summary",
            "detected_elements",
            "suggested_actions",
            "error",
        ] {
            assert!(
                DEFAULT_VLM_PROMPT.contains(key),
                "prompt is missing required key: {key}"
            );
        }
    }

    #[test]
    fn strip_fences_removes_markdown() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("  {\"a\":1}  "), "{\"a\":1}");
    }

    #[tokio::test]
    async fn read_only_autonomy_is_refused() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });
        let tool = AnalyzeScreenTool::new(security);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }
}
