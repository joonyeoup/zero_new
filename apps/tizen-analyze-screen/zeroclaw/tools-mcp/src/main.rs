//! tv-screen-tools — MCP stdio server registering two tools with ZeroClaw:
//!
//!   screenshot     no args; runs the tizenscreenshot binary (10s timeout) or
//!                  watches for an externally written PNG; returns
//!                  {"image_path": "..."}
//!   analyze_image  {image_path}; downscales (optional), base64-encodes,
//!                  calls the VLM chat/completions endpoint with a fixed
//!                  analysis prompt, returns the VLM's raw text
//!
//! Protocol: MCP 2024-11-05, newline-delimited JSON-RPC over stdio (matches
//! ZeroClaw's `crates/zeroclaw-tools/src/mcp_protocol.rs`). Registered in
//! ZeroClaw config as [[mcp.servers]] with transport = "stdio"; tools appear
//! to the agent as `<server>__screenshot` / `<server>__analyze_image`.
//!
//! All configuration is via environment variables (set in the MCP server's
//! `env` map in ZeroClaw's config) — nothing hardcoded:
//!
//!   SCREENSHOT_MODE          exec | watch            (default exec)
//!   SCREENSHOT_BIN           path to tizenscreenshot
//!   SCREENSHOT_ARGS          space-separated args    (default none)
//!   SCREENSHOT_OUTPUT        fixed PNG output path
//!   SCREENSHOT_TIMEOUT_SECS  default 10
//!   VLM_BASE_URL             e.g. http://dgx:8000/v1
//!   VLM_MODEL                e.g. Qwen/Qwen3-VL-8B-Instruct
//!   VLM_API_KEY              optional bearer
//!   VLM_TIMEOUT_SECS         default 60
//!   VLM_PROMPT               analysis prompt override
//!   DOWNSCALE_ENABLED        true|false (default true)
//!   DOWNSCALE_MAX_EDGE       default 1280

use base64::Engine;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};
use wait_timeout::ChildExt;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn log(msg: &str) {
    let line = format!("[{}] [tv-screen-tools] {}\n",
        humantime::format_rfc3339_millis(SystemTime::now()), msg);
    eprint!("{}", line);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true).append(true).open("/tmp/tv-tools.log") {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

fn main() {
    log("MCP server starting (stdio)");
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            log(&format!("ignoring non-JSON line: {}", &line[..line.len().min(120)]));
            continue;
        };
        let method = req["method"].as_str().unwrap_or("");
        let id = req["id"].clone();
        // Notifications (no id) get no response.
        if id.is_null() {
            log(&format!("notification: {method}"));
            continue;
        }
        let response = match method {
            "initialize" => json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "tv-screen-tools", "version": env!("CARGO_PKG_VERSION") }
            }),
            "tools/list" => tools_list(),
            "tools/call" => tools_call(&req["params"]),
            "ping" => json!({}),
            other => {
                let err = json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {other}") }
                });
                write_line(&mut stdout, &err);
                continue;
            }
        };
        write_line(&mut stdout, &json!({ "jsonrpc": "2.0", "id": id, "result": response }));
    }
    log("stdin closed, exiting");
}

fn write_line(stdout: &mut std::io::Stdout, v: &Value) {
    let _ = writeln!(stdout, "{v}");
    let _ = stdout.flush();
}

fn tools_list() -> Value {
    json!({
        "tools": [
            {
                "name": "screenshot",
                "description": "Capture the TV screen right now. Takes no arguments. Returns a JSON object with the image_path of the captured PNG. Use this FIRST whenever the user asks about what is currently on the screen.",
                "inputSchema": { "type": "object", "properties": {}, "required": [] }
            },
            {
                "name": "analyze_image",
                "description": "Send a captured screen image to the vision model and get back a simple text description of everything visible on it. Call this AFTER the screenshot tool. Pass the image_path from the screenshot result if you have it; if the runtime shows it as a media attachment instead of a literal path, just call this with no arguments and the most recent screenshot is used.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "image_path": { "type": "string", "description": "Optional path to the PNG returned by the screenshot tool; defaults to the most recent screenshot" }
                    },
                    "required": []
                }
            }
        ]
    })
}

fn tools_call(params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let started = std::time::Instant::now();
    log(&format!("tool call: {name}"));
    let outcome = match name {
        "screenshot" => run_screenshot(),
        "analyze_image" => run_analyze_image(&params["arguments"]),
        other => Err(format!("unknown tool: {other}")),
    };
    log(&format!("tool {name} finished in {} ms", started.elapsed().as_millis()));
    match outcome {
        Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
        Err(msg) => json!({ "content": [{ "type": "text", "text": msg }], "isError": true }),
    }
}

// ---------------------------------------------------------------- screenshot

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Most recent screenshot captured by THIS server process. `analyze_image`
/// falls back to it when the agent cannot supply a literal path (ZeroClaw
/// 0.8.2 masks image paths in tool results as "[media attachment]" before a
/// non-vision LLM sees them).
static LAST_SCREENSHOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn run_screenshot() -> Result<String, String> {
    let started = std::time::Instant::now();
    let mode = env_or("SCREENSHOT_MODE", "exec");
    let out_path = PathBuf::from(env_or("SCREENSHOT_OUTPUT", "/tmp/screenshot.png"));
    let timeout = env_or("SCREENSHOT_TIMEOUT_SECS", "10").parse().unwrap_or(10);
    
    log(&format!("screenshot started: mode={} timeout={}s", mode, timeout));
    
    let path = match mode.as_str() {
        "watch" => {
            let watch_start = std::time::Instant::now();
            let result = watch_screenshot(&out_path, timeout);
            log(&format!("watch mode completed ({}ms)", watch_start.elapsed().as_millis()));
            result?
        }
        _ => {
            let exec_start = std::time::Instant::now();
            let result = exec_screenshot(&out_path, timeout);
            log(&format!("exec mode completed ({}ms)", exec_start.elapsed().as_millis()));
            result?
        }
    };
    
    *LAST_SCREENSHOT.lock().unwrap() = Some(path.clone());
    // std::thread::sleep(Duration::from_secs(5));
    log(&format!("=== TOTAL: screenshot completed in {}ms, path={}", started.elapsed().as_millis(), path.display()));
    Ok(json!({ "image_path": path.display().to_string() }).to_string())
}

fn exec_screenshot(out_path: &Path, timeout_secs: u64) -> Result<PathBuf, String> {
    let bin = std::env::var("SCREENSHOT_BIN")
        .map_err(|_| "SCREENSHOT_BIN is not configured".to_string())?;
    let args: Vec<String> = env_or("SCREENSHOT_ARGS", "")
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let before = mtime(out_path);

    let mut child = Command::new(&bin)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn {bin}: {e}"))?;
    let status = child
        .wait_timeout(Duration::from_secs(timeout_secs))
        .map_err(|e| format!("wait failed: {e}"))?;
    let status = match status {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("screenshot binary timed out after {timeout_secs}s"));
        }
    };
    let output = child.wait_with_output().map_err(|e| format!("read output failed: {e}"))?;
    if !status.success() {
        return Err(format!(
            "screenshot binary exited with {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // Fixed output path, freshly written.
    if out_path.is_file() && mtime(out_path) != before {
        return Ok(out_path.to_path_buf());
    }
    // Fallback pattern: binary printed the PNG path to stdout.
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(p) = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty() && Path::new(l).is_file())
    {
        return Ok(PathBuf::from(p));
    }
    if out_path.is_file() {
        return Ok(out_path.to_path_buf()); // coarse-mtime filesystems
    }
    Err(format!("no PNG at {} and stdout had no valid path", out_path.display()))
}

// fn run_screenshot() -> Result<String, String> {
//     let mode = env_or("SCREENSHOT_MODE", "exec");
//     let out_path = PathBuf::from(env_or("SCREENSHOT_OUTPUT", "/tmp/screenshot.png"));
//     let timeout = env_or("SCREENSHOT_TIMEOUT_SECS", "10").parse().unwrap_or(10);
//     let path = match mode.as_str() {
//         "watch" => watch_screenshot(&out_path, timeout)?,
//         _ => exec_screenshot(&out_path, timeout)?,
//     };
//     *LAST_SCREENSHOT.lock().unwrap() = Some(path.clone());
//     Ok(json!({ "image_path": path.display().to_string() }).to_string())
// }

// fn exec_screenshot(out_path: &Path, timeout_secs: u64) -> Result<PathBuf, String> {
//     let bin = std::env::var("SCREENSHOT_BIN")
//         .map_err(|_| "SCREENSHOT_BIN is not configured".to_string())?;
//     let args: Vec<String> = env_or("SCREENSHOT_ARGS", "")
//         .split_whitespace()
//         .map(str::to_string)
//         .collect();
//     let before = mtime(out_path);

//     let mut child = Command::new(&bin)
//         .args(&args)
//         .stdout(Stdio::piped())
//         .stderr(Stdio::piped())
//         .spawn()
//         .map_err(|e| format!("failed to spawn {bin}: {e}"))?;
//     let status = child
//         .wait_timeout(Duration::from_secs(timeout_secs))
//         .map_err(|e| format!("wait failed: {e}"))?;
//     let status = match status {
//         Some(s) => s,
//         None => {
//             let _ = child.kill();
//             let _ = child.wait();
//             return Err(format!("screenshot binary timed out after {timeout_secs}s"));
//         }
//     };
//     let output = child.wait_with_output().map_err(|e| format!("read output failed: {e}"))?;
//     if !status.success() {
//         return Err(format!(
//             "screenshot binary exited with {status}: {}",
//             String::from_utf8_lossy(&output.stderr).trim()
//         ));
//     }
//     // Fixed output path, freshly written.
//     if out_path.is_file() && mtime(out_path) != before {
//         return Ok(out_path.to_path_buf());
//     }
//     // Fallback pattern: binary printed the PNG path to stdout.
//     let stdout = String::from_utf8_lossy(&output.stdout);
//     if let Some(p) = stdout
//         .lines()
//         .rev()
//         .map(str::trim)
//         .find(|l| !l.is_empty() && Path::new(l).is_file())
//     {
//         return Ok(PathBuf::from(p));
//     }
//     if out_path.is_file() {
//         return Ok(out_path.to_path_buf()); // coarse-mtime filesystems
//     }
//     Err(format!("no PNG at {} and stdout had no valid path", out_path.display()))
// }

fn watch_screenshot(out_path: &Path, timeout_secs: u64) -> Result<PathBuf, String> {
    let before = mtime(out_path);
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        if out_path.is_file() && mtime(out_path) != before {
            std::thread::sleep(Duration::from_millis(200)); // let writer finish
            return Ok(out_path.to_path_buf());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!(
        "no fresh screenshot at {} within {timeout_secs}s (watch mode)",
        out_path.display()
    ))
}

// ------------------------------------------------------------- analyze_image

const DEFAULT_VLM_PROMPT: &str = "Describe this TV screen in 3 short sentences: \
                                    State the screen type, then name 3 most prominent visible elements \
                                    Do not speculate about content you cannot see. Do not list minor UI details";

/// Resolve the image to analyze: the caller-supplied path when it points at a
/// real file, otherwise the most recent screenshot (in-process memory, then
/// the fixed SCREENSHOT_OUTPUT path). The agent's LLM may only see a masked
/// "[media attachment]" placeholder instead of the real path — see
/// LAST_SCREENSHOT.
fn resolve_image_path(args: &Value) -> Result<PathBuf, String> {
    if let Some(p) = args["image_path"].as_str() {
        let path = Path::new(p);
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        log(&format!("image_path {p:?} is not a file; falling back to last screenshot"));
    }
    if let Some(p) = LAST_SCREENSHOT.lock().unwrap().clone() {
        if p.is_file() {
            return Ok(p);
        }
    }
    // Same default as run_screenshot() writes — these two must never drift, or
    // the fallback (and the sidecar's GET /screenshot) points at nothing.
    let fixed = PathBuf::from(env_or("SCREENSHOT_OUTPUT", "/tmp/screenshot.png"));
    if fixed.is_file() {
        return Ok(fixed);
    }
    Err("no usable image: no valid image_path argument and no screenshot captured yet".to_string())
}

// fn run_analyze_image(args: &Value) -> Result<String, String> {
//     let image_path = resolve_image_path(args)?;
//     let base_url = std::env::var("VLM_BASE_URL")
//         .map_err(|_| "VLM_BASE_URL is not configured".to_string())?;
//     let model = env_or("VLM_MODEL", "Qwen/Qwen3-VL-8B-Instruct");
//     let timeout: u64 = env_or("VLM_TIMEOUT_SECS", "60").parse().unwrap_or(60);
//     let prompt = env_or("VLM_PROMPT", DEFAULT_VLM_PROMPT);

//     let png = prepare_image(&image_path)?;
//     log(&format!("image payload {} bytes", png.len()));
//     let data_uri = format!(
//         "data:image/png;base64,{}",
//         base64::engine::general_purpose::STANDARD.encode(&png)
//     );
//     let body = json!({
//         "model": model,
//         "temperature": 0,
//         "max_tokens": 1024,
//         "messages": [{ "role": "user", "content": [
//             { "type": "image_url", "image_url": { "url": data_uri } },
//             { "type": "text", "text": prompt }
//         ]}]
//     });

//     let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
//     let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(timeout)).build();
//     let mut req = agent.post(&url).set("Content-Type", "application/json");
//     if let Ok(key) = std::env::var("VLM_API_KEY") {
//         req = req.set("Authorization", &format!("Bearer {key}"));
//     }
//     let resp: Value = req
//         .send_json(body)
//         .map_err(|e| match e {
//             ureq::Error::Status(code, r) => format!(
//                 "VLM returned HTTP {code}: {}",
//                 r.into_string().unwrap_or_default().chars().take(300).collect::<String>()
//             ),
//             other => format!("VLM request failed: {other}"),
//         })?
//         .into_json()
//         .map_err(|e| format!("VLM response was not JSON: {e}"))?;
//     resp["choices"][0]["message"]["content"]
//         .as_str()
//         .map(str::to_string)
//         .ok_or_else(|| "VLM response missing choices[0].message.content".to_string())
// }

fn run_analyze_image(args: &Value) -> Result<String, String> {
    let started = std::time::Instant::now();
    
    // Step 1: Resolve image path
    let image_path = resolve_image_path(args)?;
    log(&format!("Step 1 - resolved image path: {} ({}ms)", 
                 image_path.display(), started.elapsed().as_millis()));
    
    // Step 2: Load VLM config
    let base_url = std::env::var("VLM_BASE_URL")
        .map_err(|_| "VLM_BASE_URL is not configured".to_string())?;
    let model = env_or("VLM_MODEL", "Qwen/Qwen3-VL-8B-Instruct");
    let timeout: u64 = env_or("VLM_TIMEOUT_SECS", "60").parse().unwrap_or(60);
    let prompt = env_or("VLM_PROMPT", DEFAULT_VLM_PROMPT);
    log(&format!("Step 2 - loaded config: model={} url={} ({}ms)", 
                 model, base_url, started.elapsed().as_millis()));
    
    // Step 3: Prepare image (read + downscale + encode)
    let prepare_start = std::time::Instant::now();
    let png = prepare_image(&image_path)?;
    log(&format!("Step 3 - image prepared: {} bytes ({}ms)", 
                 png.len(), prepare_start.elapsed().as_millis()));
    
    // Step 4: Base64 encode
    let encode_start = std::time::Instant::now();
    let data_uri = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&png)
    );
    log(&format!("Step 4 - base64 encoded ({}ms)", encode_start.elapsed().as_millis()));
    
    // Step 5: Build request body
    let body = json!({
        "model": model,
        "temperature": 0,
        "max_tokens": 240,
        "chat_template_kwargs": { "enable_thinking": false },
        "messages": [{ "role": "user", "content": [
            { "type": "image_url", "image_url": { "url": data_uri } },
            { "type": "text", "text": prompt }
        ]}]
    });
    log(&format!("Step 5 - built request body ({}ms)", started.elapsed().as_millis()));
    
    // Step 6: Send HTTP request to VLM
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    log(&format!("Step 6a - preparing HTTP request to {} ({}ms)", url, started.elapsed().as_millis()));
    
    // Build agent with connection pooling
    let agent_start = std::time::Instant::now();
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(timeout))
        .build();
    log(&format!("Step 6b - agent built ({}ms)", agent_start.elapsed().as_millis()));
    
    let mut req = agent.post(&url).set("Content-Type", "application/json");
    if let Ok(key) = std::env::var("VLM_API_KEY") {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    
    // Serialize body and measure
    let body_start = std::time::Instant::now();
    let body_size = serde_json::to_string(&body).map(|s| s.len()).unwrap_or(0);
    log(&format!("Step 6c - request body serialized to {} bytes ({}ms)", body_size, body_start.elapsed().as_millis()));
    
    log(&format!("Step 6d - sending VLM request ({}ms)", started.elapsed().as_millis()));
    let http_start = std::time::Instant::now();
    
    // Send and track connection vs response time
    let send_start = std::time::Instant::now();
    let response = req
        .send_json(body)
        .map_err(|e| match e {
            ureq::Error::Status(code, r) => format!(
                "VLM returned HTTP {code}: {}",
                r.into_string().unwrap_or_default().chars().take(300).collect::<String>()
            ),
            other => format!("VLM request failed: {other}"),
        })?;
    log(&format!("Step 6e - HTTP request sent, waiting for response ({}ms, status={})", send_start.elapsed().as_millis(), response.status()));
    
    let read_start = std::time::Instant::now();
    let resp: Value = response
        .into_json()
        .map_err(|e| format!("VLM response was not JSON: {e}"))?;
    log(&format!("Step 7 - VLM response parsed ({}ms total, {}ms read)", http_start.elapsed().as_millis(), read_start.elapsed().as_millis()));
    
    // Step 8: Parse VLM response
    let parse_start = std::time::Instant::now();
    let vlm_text = resp["choices"][0]["message"]["content"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "VLM response missing choices[0].message.content".to_string())?;
    log(&format!("Step 8 - parsed VLM response ({}ms)", parse_start.elapsed().as_millis()));

    let finish_reason = resp["choices"][0]["finish_reason"]
        .as_str()
        .unwrap_or("unknown");
    if vlm_text.trim().is_empty() {
        return Err(format!(
            "VLM returned empty content (finish_reason={})",
            finish_reason
        ));
    }
    log(&format!("Step 8b - content ok ({} chars, finish_reason={})",
                    vlm_text.len(), finish_reason));
    log(&format!("=== TOTAL: analyze_image completed in {}ms ===", started.elapsed().as_millis()));
    
    Ok(vlm_text)
}

// fn prepare_image(path: &Path) -> Result<Vec<u8>, String> {
//     let enabled = env_or("DOWNSCALE_ENABLED", "true") == "true";
//     if !enabled {
//         return std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()));
//     }
//     downscale(path)
// }


fn prepare_image(path: &Path) -> Result<Vec<u8>, String> {
    let started = std::time::Instant::now();
    let enabled = env_or("DOWNSCALE_ENABLED", "true") == "true";
    
    if !enabled {
        let read_start = std::time::Instant::now();
        let png = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        log(&format!("prepare_image: read full-size {} bytes ({}ms)", png.len(), read_start.elapsed().as_millis()));
        return Ok(png);
    }
    
    let result = downscale(path)?;
    log(&format!("prepare_image: total ({}ms, {} bytes)", started.elapsed().as_millis(), result.len()));
    Ok(result)
}

#[cfg(feature = "downscale")]
fn downscale(path: &Path) -> Result<Vec<u8>, String> {
    let started = std::time::Instant::now();
    let max: u32 = env_or("DOWNSCALE_MAX_EDGE", "1280").parse().unwrap_or(1280);
    
    // Step 1: Decode image
    let decode_start = std::time::Instant::now();
    let img = image::open(path).map_err(|e| format!("cannot decode {}: {e}", path.display()))?;
    log(&format!("downscale: decoded {}x{} ({}ms)", img.width(), img.height(), decode_start.elapsed().as_millis()));
    
    // Step 2: Check if downscale needed
    if img.width().max(img.height()) <= max {
        let read_start = std::time::Instant::now();
        let png = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        log(&format!("downscale: image already small, read as-is ({}ms)", read_start.elapsed().as_millis()));
        return Ok(png);
    }
    
    // Step 3: Resize
    let resize_start = std::time::Instant::now();
    let resized = img.resize(max, max, image::imageops::FilterType::Triangle);
    log(&format!("downscale: resized {}x{} -> {}x{} ({}ms)", 
                 img.width(), img.height(), resized.width(), resized.height(), resize_start.elapsed().as_millis()));
    
    // Step 4: Encode PNG
    let encode_start = std::time::Instant::now();
    let mut buf = std::io::Cursor::new(Vec::new());
    resized
        .write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| format!("re-encode failed: {e}"))?;
    log(&format!("downscale: encoded PNG ({}ms, {} bytes)", encode_start.elapsed().as_millis(), buf.get_ref().len()));
    
    log(&format!("downscale: TOTAL ({}ms)", started.elapsed().as_millis()));
    Ok(buf.into_inner())
}

#[cfg(not(feature = "downscale"))]
fn downscale(path: &Path) -> Result<Vec<u8>, String> {
    let started = std::time::Instant::now();
    log("built without downscale feature; sending full-size PNG");
    let result = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    log(&format!("downscale (feature disabled): read {} bytes ({}ms)", result.len(), started.elapsed().as_millis()));
    Ok(result)
}

// #[cfg(feature = "downscale")]
// fn downscale(path: &Path) -> Result<Vec<u8>, String> {
//     let max: u32 = env_or("DOWNSCALE_MAX_EDGE", "1280").parse().unwrap_or(1280);
//     let img = image::open(path).map_err(|e| format!("cannot decode {}: {e}", path.display()))?;
//     if img.width().max(img.height()) <= max {
//         return std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()));
//     }
//     let resized = img.resize(max, max, image::imageops::FilterType::Triangle);
//     log(&format!(
//         "downscaled {}x{} -> {}x{}",
//         img.width(), img.height(), resized.width(), resized.height()
//     ));
//     let mut buf = std::io::Cursor::new(Vec::new());
//     resized
//         .write_to(&mut buf, image::ImageFormat::Png)
//         .map_err(|e| format!("re-encode failed: {e}"))?;
//     Ok(buf.into_inner())
// }

// #[cfg(not(feature = "downscale"))]
// fn downscale(path: &Path) -> Result<Vec<u8>, String> {
//     log("built without downscale feature; sending full-size PNG");
//     std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
// }
