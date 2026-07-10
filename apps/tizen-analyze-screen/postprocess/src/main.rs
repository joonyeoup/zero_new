//! analyze-screen-postprocess — thin validation proxy in front of ZeroClaw.
//!
//!   Tizen app -> POST /analyze-screen (here) -> POST {gateway}/webhook
//!                                                (ZeroClaw agent loop runs)
//!             <- validated schema JSON        <- {"response": "<agent text>"}
//!
//! Step 6 of the flow: pure-code parse + schema validation of the agent's
//! final answer. If invalid, ONE follow-up message is sent to the same agent
//! conversation (same X-Session-Id) asking for corrected JSON; if that is
//! still invalid, a structured error object is returned. No tool sequencing
//! happens here — the agent's LLM owns the tool chain.
//!
//! Config via env (nothing hardcoded):
//!   POSTPROCESS_PORT     default 8787
//!   GATEWAY_URL          default http://127.0.0.1:42617
//!   GATEWAY_TOKEN        optional bearer for paired gateways
//!   AGENT_ALIAS          optional ?agent= override
//!   DEFAULT_MESSAGE      default "Analyze what is currently on my screen."
//!   TOTAL_TIMEOUT_SECS   default 150 (whole request budget)

mod stagelog;
mod validate;

use serde_json::Value;
use stagelog::StageLog;
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Response, Server};

static REQ_COUNTER: AtomicU64 = AtomicU64::new(1);

const RETRY_NUDGE: &str = "Your response was not valid JSON per the schema. Respond again with \
ONLY the corrected JSON object matching the schema — no markdown fences, no prose.";

struct Cfg {
    port: u16,
    gateway_url: String,
    token: Option<String>,
    agent: Option<String>,
    default_message: String,
    total_timeout_secs: u64,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn load_cfg() -> Cfg {
    Cfg {
        port: env_or("POSTPROCESS_PORT", "8787").parse().unwrap_or(8787),
        gateway_url: env_or("GATEWAY_URL", "http://127.0.0.1:42617"),
        token: std::env::var("GATEWAY_TOKEN").ok(),
        agent: std::env::var("AGENT_ALIAS").ok(),
        default_message: env_or("DEFAULT_MESSAGE", "Analyze what is currently on my screen."),
        total_timeout_secs: env_or("TOTAL_TIMEOUT_SECS", "150").parse().unwrap_or(150),
    }
}

fn main() {
    let cfg = load_cfg();
    let addr = format!("127.0.0.1:{}", cfg.port);
    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "analyze-screen-postprocess on http://{addr} -> gateway {} (agent: {})",
        cfg.gateway_url,
        cfg.agent.as_deref().unwrap_or("<default>")
    );
    let cfg = std::sync::Arc::new(cfg);
    for request in server.incoming_requests() {
        let cfg = std::sync::Arc::clone(&cfg);
        std::thread::spawn(move || handle(request, &cfg));
    }
}

fn cors_headers() -> Vec<Header> {
    [
        ("Access-Control-Allow-Origin", "*"),
        ("Access-Control-Allow-Methods", "POST, GET, OPTIONS"),
        ("Access-Control-Allow-Headers", "Content-Type"),
        ("Access-Control-Expose-Headers", "X-Timings-Ms"),
    ]
    .iter()
    .map(|(k, v)| Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap())
    .collect()
}

fn json_response(status: u16, body: &Value, extra: Vec<Header>) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut resp = Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap());
    for h in cors_headers().into_iter().chain(extra) {
        resp = resp.with_header(h);
    }
    resp
}

fn handle(mut request: tiny_http::Request, cfg: &Cfg) {
    let method = request.method().clone();
    let url = request.url().split('?').next().unwrap_or("").to_string();
    let result = match (&method, url.as_str()) {
        (Method::Options, _) => {
            let mut resp = Response::empty(204);
            for h in cors_headers() {
                resp = resp.with_header(h);
            }
            request.respond(resp)
        }
        (Method::Get, "/health") => {
            request.respond(json_response(200, &serde_json::json!({"status": "ok"}), vec![]))
        }
        (Method::Post, "/analyze-screen") => {
            let mut body = String::new();
            let _ = request.as_reader().take(64 * 1024).read_to_string(&mut body);
            let message = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| cfg.default_message.clone());

            let id = format!("req-{}", REQ_COUNTER.fetch_add(1, Ordering::Relaxed));
            let mut log = StageLog::new(&id);
            let (status, value) = run(cfg, &message, &id, &mut log);
            let timings = log.finish();
            let th = Header::from_bytes(&b"X-Timings-Ms"[..], timings.as_bytes()).unwrap();
            request.respond(json_response(status, &value, vec![th]))
        }
        _ => request.respond(json_response(
            404,
            &validate::error_object("NOT_FOUND", &format!("no route {method} {url}")),
            vec![],
        )),
    };
    if let Err(e) = result {
        eprintln!("failed to send response: {e}");
    }
}

/// One button press: agent turn -> validate -> (one retry nudge) -> result.
fn run(cfg: &Cfg, message: &str, req_id: &str, log: &mut StageLog) -> (u16, Value) {
    // Session id ties the retry to the SAME agent conversation (the gateway
    // honors the X-Session-Id header for /webhook history).
    let session_id = format!(
        "tv-{}-{req_id}",
        SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
    );

    log.stage("agent_turn");
    log.note(&format!("message: {message:?} session: {session_id}"));
    let text = match webhook(cfg, message, &session_id, log) {
        Ok(t) => t,
        Err(e) => return (502, validate::error_object("GATEWAY_FAILED", &e)),
    };

    log.stage("validate");
    match lenient_validate(&text, log) {
        Ok(v) => return (200, v),
        Err(errs) => log.note(&format!("invalid: {}", errs.join("; "))),
    }

    if log.elapsed_secs() >= cfg.total_timeout_secs {
        return (504, validate::error_object("TIMEOUT", "budget exhausted before retry"));
    }

    // The one permitted nudge: same conversation, ask for corrected JSON.
    log.stage("retry_turn");
    let text = match webhook(cfg, RETRY_NUDGE, &session_id, log) {
        Ok(t) => t,
        Err(e) => return (502, validate::error_object("GATEWAY_FAILED", &e)),
    };
    log.stage("revalidate");
    match lenient_validate(&text, log) {
        Ok(v) => {
            log.note("recovered via retry nudge");
            (200, v)
        }
        Err(errs) => (
            422,
            validate::error_object(
                "AGENT_OUTPUT_INVALID",
                &format!("agent's answer failed validation after one retry: {}", errs.join("; ")),
            ),
        ),
    }
}

/// Pure-code parse: strict first, then fence-stripping/first-object
/// extraction (still no LLM involved — the only LLM nudge is the retry turn).
fn lenient_validate(text: &str, log: &StageLog) -> Result<Value, Vec<String>> {
    match validate::parse_and_validate(text) {
        Ok(v) => Ok(v),
        Err(first) => match validate::extract_json_candidate(text) {
            Some(candidate) => validate::parse_and_validate(&candidate).map(|v| {
                log.note("recovered via fence/JSON extraction");
                v
            }),
            None => Err(first),
        },
    }
}

fn webhook(cfg: &Cfg, message: &str, session_id: &str, log: &StageLog) -> Result<String, String> {
    let mut url = format!("{}/webhook", cfg.gateway_url.trim_end_matches('/'));
    if let Some(agent) = &cfg.agent {
        url = format!("{url}?agent={agent}");
    }
    log.note(&format!("POST {url}"));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(cfg.total_timeout_secs))
        .build();
    let mut req = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .set("X-Session-Id", session_id);
    if let Some(token) = &cfg.token {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    let resp: Value = req
        .send_json(serde_json::json!({ "message": message }))
        .map_err(|e| match e {
            ureq::Error::Status(code, r) => format!(
                "gateway returned HTTP {code}: {}",
                r.into_string().unwrap_or_default().chars().take(300).collect::<String>()
            ),
            other => format!("gateway request failed: {other}"),
        })?
        .into_json()
        .map_err(|e| format!("gateway response was not JSON: {e}"))?;
    resp["response"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("gateway response missing 'response' field: {resp}"))
}
