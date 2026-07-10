//! Per-stage latency logging: every pipeline stage emits a timestamped line
//! so demo latency can be measured stage by stage, and the collected timings
//! are exposed to the client in the `X-Timings-Ms` response header.

use std::time::{Instant, SystemTime};

pub struct StageLog {
    request_id: String,
    started: Instant,
    stage_started: Option<(String, Instant)>,
    timings_ms: Vec<(String, u128)>,
}

fn now_rfc3339() -> String {
    humantime::format_rfc3339_millis(SystemTime::now()).to_string()
}

impl StageLog {
    pub fn new(request_id: &str) -> Self {
        eprintln!("[{}] [{}] [request] start", now_rfc3339(), request_id);
        Self {
            request_id: request_id.to_string(),
            started: Instant::now(),
            stage_started: None,
            timings_ms: Vec::new(),
        }
    }

    pub fn stage(&mut self, name: &str) {
        self.end_stage();
        eprintln!("[{}] [{}] [{}] start", now_rfc3339(), self.request_id, name);
        self.stage_started = Some((name.to_string(), Instant::now()));
    }

    pub fn note(&self, msg: &str) {
        let stage = self.stage_started.as_ref().map(|(n, _)| n.as_str()).unwrap_or("request");
        eprintln!("[{}] [{}] [{}] {}", now_rfc3339(), self.request_id, stage, msg);
    }

    fn end_stage(&mut self) {
        if let Some((name, at)) = self.stage_started.take() {
            let ms = at.elapsed().as_millis();
            eprintln!("[{}] [{}] [{}] done in {} ms", now_rfc3339(), self.request_id, name, ms);
            self.timings_ms.push((name, ms));
        }
    }

    pub fn elapsed_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Close the current stage and return `{"stage": ms, ..., "total": ms}`.
    pub fn finish(mut self) -> String {
        self.end_stage();
        let total = self.started.elapsed().as_millis();
        eprintln!("[{}] [{}] [request] done in {} ms", now_rfc3339(), self.request_id, total);
        let mut parts: Vec<String> = self
            .timings_ms
            .iter()
            .map(|(n, ms)| format!("\"{n}\":{ms}"))
            .collect();
        parts.push(format!("\"total\":{total}"));
        format!("{{{}}}", parts.join(","))
    }
}
