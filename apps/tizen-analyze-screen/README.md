# Analyze Screen — a real ZeroClaw agent loop on a Tizen TV

Press a button on the TV → the request goes to ZeroClaw as a natural-language
message → **ZeroClaw's LLM autonomously decides** to call a `screenshot` tool,
then an `analyze_image` tool (which sends the PNG to a remote VLM), then
composes a schema-conformant JSON answer → a thin pure-code layer validates it
(one retry nudge max) → the TV app renders it.

The tool sequence is **not hardcoded anywhere** — the LLM drives the chain.
The only permitted nudge is the single validation retry message.

## Architecture — the agent loop

```
┌────────────────────────────── Samsung Tizen TV ─────────────────────────────┐
│                                                                             │
│  ┌─────────────────┐ ENTER/click                                            │
│  │  Tizen web app  │────────────┐                                           │
│  │  (tizen-app/)   │            ▼                                           │
│  │  10-foot UI     │  POST http://127.0.0.1:8787/analyze-screen             │
│  │  loading/error/ │◄───────────┐  validated JSON (+ X-Timings-Ms)          │
│  │  result overlay │            │                                           │
│  └─────────────────┘  ┌─────────┴──────────────┐                            │
│                       │ analyze-screen-        │  step 6: parse + schema-   │
│                       │ postprocess            │  validate; ONE retry nudge │
│                       │ (postprocess/, :8787)  │  (same X-Session-Id);      │
│                       └─────────┬──────────────┘  else structured error     │
│                                 │ POST /webhook {"message": ...}            │
│                       ┌─────────▼──────────────┐                            │
│                       │  ZeroClaw gateway      │   the AGENT LOOP:          │
│                       │  (:42617, agents.main) │   msg → LLM → tool_call    │
│                       └─────────┬──────────────┘   → LLM → tool_call → LLM  │
│                                 │ native OpenAI tool calls   → final JSON   │
│              ┌──────────────────┼──────────────────┐                        │
│              │ MCP stdio        │                   ▼ chat/completions      │
│    ┌─────────▼──────────┐       │        ┌─────────────────────┐            │
│    │  tv-screen-tools   │       │        │  DGX LLM (the brain)│ (off-TV)   │
│    │ (zeroclaw/tools-   │       │        │  {{LLM_BASE_URL}}   │            │
│    │  mcp/, 2 tools)    │       │        │  native tool calls  │            │
│    ├────────────────────┤       │        └─────────────────────┘            │
│    │ tv__screenshot ────┼── runs ./tizenscreenshot → PNG                    │
│    │ tv__analyze_image ─┼── downscale→base64→VLM ──────┐                    │
│    └────────────────────┘                              │                    │
└────────────────────────────────────────────────────────┼────────────────────┘
                                                         ▼  HTTP (LAN)
                                          ┌──────────────────────────┐
                                          │ vLLM server (DGX)        │
                                          │ Qwen3-VL-8B, OpenAI-     │
                                          │ compatible /chat/…       │
                                          └──────────────────────────┘
```

One button press = 3 LLM turns + 2 tool executions + 1 VLM call:

1. Tizen app → postprocess sidecar → gateway `/webhook`.
2. **LLM turn 1** — sees the SOUL.md system prompt + native tool specs,
   decides to call `tv__screenshot`.
3. ZeroClaw runs the MCP tool → `./tizenscreenshot` → `{"image_path": ...}`.
4. **LLM turn 2** — decides to call `tv__analyze_image`. (ZeroClaw 0.8.2
   masks the literal path as `[media attachment]`, so the tool falls back to
   the most recent screenshot — see Troubleshooting.)
5. Tool downscales (max 1280px long edge, pure-Rust `image` crate,
   config-optional), base64-encodes, calls the VLM, returns its description.
6. **LLM turn 3** — composes the final answer as ONLY the schema JSON.
7. Postprocess layer parses + validates (pure code). Invalid → ONE retry
   message into the same session; still invalid → structured error object.
8. Tizen app renders the result overlay.

## Repo layout

| Path | What |
|---|---|
| `zeroclaw/config/config.toml` | ZeroClaw 0.8.2 config sections (provider, agent, profiles, MCP) — every placeholder lives here |
| `zeroclaw/tools-mcp/` | Rust MCP stdio server: `screenshot` + `analyze_image` tools |
| `zeroclaw/workspace/SOUL.md` | Agent system prompt (embeds the JSON schema verbatim) |
| `postprocess/` | Rust validation sidecar: `POST /analyze-screen` → gateway → validate → retry-once |
| `tizen-app/` | Tizen 6.0+ web app (10-foot UI, remote-key handling) |
| `mock/` | Mock agent-LLM (native + XML tool protocols), mock VLM, stub screenshot binary |
| `scripts/test_e2e.sh` | Full local e2e against the REAL zeroclaw binary + mocks |
| `scripts/deploy_tv.sh` | sdb push / wgt install / config push |
| `STATUS.md` | Living goal / done / todo document for this POC |

## Config placeholders

| Placeholder | Where | Meaning |
|---|---|---|
| `{{LLM_BASE_URL}}` | `[providers.models.vllm.dgx].uri` | DGX OpenAI-compatible endpoint (the agent's brain) |
| `{{LLM_MODEL_NAME}}` | `[providers.models.vllm.dgx].model` | Orchestrator model name |
| `{{VLM_BASE_URL}}` | `[mcp.servers.env].VLM_BASE_URL` | vLLM Qwen3-VL endpoint (called by the tool, not the loop) |
| `{{VLM_MODEL_NAME}}` | `[mcp.servers.env].VLM_MODEL` | VLM model name |
| `{{PATH_TO_BINARY}}` | `[mcp.servers.env].SCREENSHOT_BIN` | tizenscreenshot location on the TV |
| `{{SCREENSHOT_OUTPUT_PATH}}` | `[mcp.servers.env].SCREENSHOT_OUTPUT` | where it writes the PNG |
| `{{ZEROCLAW_PORT}}` | `[gateway].port` | gateway port (default 42617) |
| `{{ZEROCLAW_CONFIG_PATH}}` | — | the TV's ZeroClaw config to merge into |
| `TV_IP` | `deploy_tv.sh` env | the TV's LAN address |

Timeouts (all configurable): screenshot 10s (`SCREENSHOT_TIMEOUT_SECS`), VLM
60s (`VLM_TIMEOUT_SECS`), each LLM turn 30s (provider `timeout_secs`), total
150s (postprocess `TOTAL_TIMEOUT_SECS`; the Tizen app waits 155s).

## Running the e2e locally

```sh
# 1. build zeroclaw at the repo root (once):   cargo build --release --bin zeroclaw
# 2. the mocks need fastapi+uvicorn:           python3 -m venv v && v/bin/pip install fastapi uvicorn
# 3. run (venv on PATH so python3 finds the deps):
PATH="$PWD/v/bin:$PATH" bash scripts/test_e2e.sh
```

The suite boots the REAL zeroclaw gateway with an isolated config, the real
MCP tool server and postprocess sidecar, and mock LLM/VLM/screenshot. It
asserts: schema-valid JSON; the **LLM-chosen** tool order (screenshot before
analyze_image); the VLM actually received an image; SOUL.md reached the LLM;
the malformed→retry recovery; and the always-malformed→structured-422 path.

## Latency measurement

Every stage is logged with timestamps:

- **Tizen app** — button press / response / render (console).
- **postprocess** — `[req-N] [agent_turn|validate|retry_turn|…] done in X ms`
  on stderr, plus an `X-Timings-Ms` response header the app can read.
- **tv-screen-tools** — per-tool start/finish ms on stderr (visible in the
  gateway log), including VLM payload size.
- **ZeroClaw trace** — `<config>/data/state/runtime-trace.jsonl` records
  each LLM turn and `tool_call_start`/`tool_call_result` with arguments:
  this is where you SHOW the agent's tool-selection decisions in the demo.

Expected budget: LLM turn + screenshot + LLM turn + VLM (dominant, tens of
seconds on Qwen3-VL-8B) + LLM turn. The mock-based e2e completes in ~1s,
which isolates infrastructure overhead from model latency.

## Troubleshooting

- **Gateway 500 "LLM request failed" with `no configured [agents.<alias>]
  entry` in the trace** — ZeroClaw 0.8.2 gateway chat needs the full shape:
  `[agents.main]` + resolvable `risk_profile` + `runtime_profile` +
  `mcp_bundles` (see `zeroclaw/config/config.toml`; MCP-server omission is
  not a grant).
- **Agent captures the wrong thing / prose "Screenshot saved to" result** —
  ZeroClaw's *built-in* `screenshot` tool shadowed `tv__screenshot`; keep
  `excluded_tools = ["screenshot"]` in the risk profile.
- **`analyze_image` gets `[media attachment]` instead of a path** — 0.8.2
  promotes image paths in tool results to media markers and masks them for
  non-vision LLMs. Expected; the tool falls back to the most recent
  screenshot. Don't "fix" this by hardcoding the chain.
- **LLM never calls tools** — the `vllm` provider family always uses native
  OpenAI tool calling; serve the DGX model with vLLM's
  `--enable-auto-tool-choice --tool-call-parser <model-family>` (for many
  Qwen builds: `hermes`). ZeroClaw's XML text fallback is not reachable for
  this family in 0.8.2; if your endpoint cannot do native tool calls, switch
  the provider family or upgrade ZeroClaw.
- **System prompt ignored** — SOUL.md must be at
  `<config dir>/agents/<alias>/workspace/SOUL.md` (per-agent workspace, not
  the config root).
- **Tizen CORS / mixed content** — the app calls `http://127.0.0.1:8787`;
  `config.xml` carries `allow-navigation` for localhost and the internet
  privilege; the sidecar answers CORS preflights (`OPTIONS`, `*`).
- **Binary exec permissions** — `chmod +x` after `sdb push`; if the TV
  blocks subprocess spawn entirely, set `SCREENSHOT_MODE=watch` (the tool
  then waits for the PNG to appear instead of exec'ing).
- **vLLM rejects the image body** — raise vLLM's request-size limits or
  keep `DOWNSCALE_ENABLED=true` (a 1080p PNG easily exceeds default limits;
  1280px long edge keeps payloads in the low hundreds of KB).
- **Agent loop times out** — raise the postprocess `TOTAL_TIMEOUT_SECS` and
  the app's `REQUEST_TIMEOUT_MS` together; check per-stage logs to see
  which leg (LLM turn vs VLM) is eating the budget;
  `max_tool_iterations = 6` caps runaway loops.

## Final-answer JSON schema

```json
{
  "screen_type": "string",
  "title": "string",
  "summary": "string",
  "detected_elements": [
    { "name": "string", "description": "string", "confidence": 0.0 }
  ],
  "suggested_actions": ["string"],
  "error": null
}
```

`error` is `null` on success or `{ "code": "...", "message": "..." }`
(codes: `GATEWAY_FAILED`, `TIMEOUT`, `AGENT_OUTPUT_INVALID`, `NOT_FOUND`).

## Cross-compiling for the TV

Both Rust binaries are dependency-light (no OpenSSL — `ureq`/`tiny_http`).
For a typical ARM Tizen target:

```sh
rustup target add armv7-unknown-linux-gnueabi
cargo build --release --target armv7-unknown-linux-gnueabi \
  --manifest-path zeroclaw/tools-mcp/Cargo.toml
cargo build --release --target armv7-unknown-linux-gnueabi \
  --manifest-path postprocess/Cargo.toml
```

(Adjust `TARGET_TRIPLE` for your TV's chipset; a `*-musl` triple avoids
glibc-version mismatches on older firmware.)
