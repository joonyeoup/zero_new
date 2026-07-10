# Analyze Screen — an agentic screen-analysis demo on a Tizen TV, powered by ZeroClaw

## What this is

You press **"Analyze this screen"** on a Samsung TV. The TV app sends one
plain-English message — *"Analyze what is currently on my screen."* — to the
ZeroClaw agent runtime installed on the TV. From there, **an LLM decides what
to do**: it chooses to call a `screenshot` tool, then chooses to send that
image to a vision model via an `analyze_image` tool, then writes its final
answer as a strict JSON object. A small piece of plain code checks that JSON
against a schema (and nudges the agent once if it got the format wrong), and
the TV renders the result as an overlay.

**The entire point of this POC** is that the tool sequence is *not* written
in code anywhere. There is no `captureScreen(); callVlm(); formatJson();`
pipeline. The LLM reads the tool descriptions, reasons about the request, and
drives the chain itself. You can prove this in the logs: ZeroClaw's trace
records every LLM turn and every tool decision with timestamps.

```
button press
   │
   ▼
Tizen web app ──POST──▶ postprocess sidecar ──POST /webhook──▶ ZeroClaw gateway
(tizen-app/)            (postprocess/, :8787)                  (agent loop, :42617)
   ▲                        │ validates JSON,                      │
   │                        │ 1 retry max                          ▼
   └──── result overlay ◀───┘                            LLM on DGX decides:
                                                          1. call tv__screenshot ──▶ ./tizenscreenshot
                                                          2. call tv__analyze_image ─▶ VLM (Qwen3-VL)
                                                          3. answer with schema JSON
```

One button press = **3 LLM turns + 2 tool executions + 1 VLM call**, all
chosen by the model.

## The moving parts

| Piece | Where | What it actually does |
|---|---|---|
| **Tizen web app** | `tizen-app/` | The 10-foot UI. One button, remote-control ENTER handling, rotating "what the agent is doing" status lines, a large-font result overlay. Calls `http://127.0.0.1:8787/analyze-screen`. |
| **Postprocess sidecar** | `postprocess/` | ~230 lines of Rust, no LLM. Forwards the message to ZeroClaw's gateway, then parses + schema-validates the agent's final text. If invalid: sends exactly ONE follow-up message into the *same* conversation ("your response was not valid JSON…") and validates once more. Still invalid → structured error object. Also logs per-stage latency. |
| **ZeroClaw gateway** | already on the TV | The agent runtime (v0.8.2). Receives the message, runs the agent loop against the DGX LLM, executes tools, returns the final text. We only *configure* it — no ZeroClaw code is modified. |
| **MCP tool server** | `zeroclaw/tools-mcp/` | A small Rust binary ZeroClaw spawns over stdio. Registers the two custom tools: `screenshot` (runs the existing `tizenscreenshot` binary, 10s timeout, or *watches* for the PNG if the TV forbids spawning subprocesses) and `analyze_image` (downscales to ≤1280px long edge with a pure-Rust image crate, base64-encodes, calls the VLM's OpenAI-style endpoint, returns its text). Everything configured via env vars — nothing hardcoded. |
| **Agent system prompt** | `zeroclaw/workspace/SOUL.md` | Tells the agent it's a TV screen-analysis assistant, that it has these two tools and must use them when asked about the screen, and that its final answer must be ONLY a JSON object matching the schema (embedded verbatim). |
| **ZeroClaw config** | `zeroclaw/config/config.toml` | Every placeholder (LLM/VLM URLs, model names, binary paths, ports) lives here. Annotated with all the 0.8.2 gotchas we hit (see change log). Merge into the TV's `~/.zeroclaw/config.toml`. |
| **Mocks** | `mock/` | Let the whole thing run on a laptop with no TV and no DGX: a mock *agent* LLM that plays the loop deterministically (and can misbehave on demand to test the retry path), a mock VLM with a canned description, and a stub `tizenscreenshot` that copies a sample PNG. |
| **E2E test** | `scripts/test_e2e.sh` | Boots the REAL `zeroclaw` gateway (isolated config dir) + the real tool server + the real postprocess sidecar + all three mocks, then asserts the things that matter (below). |
| **Deploy script** | `scripts/deploy_tv.sh` | `sdb connect` → push both Rust binaries → push config + SOUL.md → package/install the `.wgt` → printed smoke-test steps. |
| **STATUS.md** | `STATUS.md` | Living document: goal, what's done, what's left. Update it as work progresses. |

## What the e2e test proves

`bash scripts/test_e2e.sh` (all green as of 2026-07-09):

1. **Happy path** — the response is schema-valid JSON, **and** the mock LLM's
   decision log shows it chose `screenshot` *before* `analyze_image` *before*
   the final answer (the order was the model's choice, not code), **and** the
   VLM actually received an image payload, **and** the SOUL.md system prompt
   reached the LLM.
2. **Malformed answer** — the mock deliberately returns fenced/prose junk
   once; the postprocess layer sends its single retry nudge into the same
   session and recovers a valid answer.
3. **Unrepairable answer** — the mock returns junk every time; the client
   gets HTTP 422 with a structured `AGENT_OUTPUT_INVALID` error object.

Requirements: the `zeroclaw` binary built at the repo root
(`cargo build --release --bin zeroclaw`) and Python with `fastapi`+`uvicorn`
for the mocks (a venv works — put its `bin/` on `PATH`).

```sh
python3 -m venv v && v/bin/pip install fastapi uvicorn
PATH="$PWD/v/bin:$PATH" bash scripts/test_e2e.sh
```

## Configuration reference

All placeholders (no secrets or URLs are hardcoded anywhere):

| Placeholder | Where | Meaning |
|---|---|---|
| `{{LLM_BASE_URL}}` | `[providers.models.vllm.dgx].uri` | DGX OpenAI-compatible endpoint — the agent's brain |
| `{{LLM_MODEL_NAME}}` | `[providers.models.vllm.dgx].model` | orchestrator model |
| `{{VLM_BASE_URL}}` | `[mcp.servers.env].VLM_BASE_URL` | vLLM Qwen3-VL endpoint — called *by the tool*, never by the loop |
| `{{VLM_MODEL_NAME}}` | `[mcp.servers.env].VLM_MODEL` | VLM model |
| `{{PATH_TO_BINARY}}` | `[mcp.servers.env].SCREENSHOT_BIN` | `tizenscreenshot` location on the TV |
| `{{SCREENSHOT_OUTPUT_PATH}}` | `[mcp.servers.env].SCREENSHOT_OUTPUT` | where it writes its PNG |
| `{{ZEROCLAW_PORT}}` | `[gateway].port` | gateway port (default 42617) |
| `TV_IP` | `deploy_tv.sh` env | the TV's LAN address |

Timeouts (all configurable): screenshot **10s**, VLM **60s**, each LLM turn
**30s**, total **150s** (postprocess `TOTAL_TIMEOUT_SECS`; the app waits 155s).
Downscaling is optional via `DOWNSCALE_ENABLED` in case the resize crate
misbehaves on the TV.

## Watching the agent think (latency + decisions)

Every stage logs with timestamps, which is what makes this demo-able:

- **ZeroClaw trace** (`<config>/data/state/runtime-trace.jsonl`) — each LLM
  turn plus `tool_call_start` / `tool_call_result` with the exact arguments
  the model chose. *This is where you show the agent's decisions.*
- **postprocess stderr** — `[req-N] [agent_turn|validate|retry_turn] done in
  X ms`, and the same numbers go to the app in an `X-Timings-Ms` header.
- **tv-screen-tools stderr** — per-tool start/finish in ms, VLM payload size.
- **Tizen app console** — button press, response, render.

Expected real-world budget: LLM turn + screenshot + LLM turn + **VLM call
(dominant — tens of seconds for Qwen3-VL-8B)** + LLM turn. The mocked e2e
finishes in ~1s, which tells you the infrastructure overhead is negligible.

## Final-answer contract

```json
{
  "screen_type": "string",       // "live_tv" | "streaming_app" | "menu" | "game" | ...
  "title": "string",
  "summary": "string",
  "detected_elements": [
    { "name": "string", "description": "string", "confidence": 0.0 }
  ],
  "suggested_actions": ["string"],
  "error": null                  // or { "code": "...", "message": "..." }
}
```

Error codes produced by the postprocess layer: `GATEWAY_FAILED`, `TIMEOUT`,
`AGENT_OUTPUT_INVALID`, `NOT_FOUND`.

---

## Change log — what changed and why

### v1 — deterministic pipeline (2026-07-08, commit `539191f`)

The first working version was **not** agentic: a Rust sidecar ran
screenshot → VLM → validate as fixed code steps.

- **Why it was built that way:** ZeroClaw 0.8.2's gateway routes are
  hard-coded (no way to add an HTTP endpoint), its WASM plugins can't
  register routes either, and at the time we believed the local model
  couldn't drive tool calls through ZeroClaw at all.
- **Why it was replaced:** it worked, but it demonstrated nothing about
  agentic reasoning — the exact thing this POC exists to show. The tool
  order lived in code, which the project brief explicitly forbids.

### v2 — the agentic refactor (2026-07-08 → 09, current working tree)

Every change below exists to make the *LLM* the thing that drives the flow.

| Change | Purpose |
|---|---|
| **Deleted** `zeroclaw/sidecar/` (the pipeline), `zeroclaw/config-fragment.toml`, `server/` | The hardcoded chain violated the core requirement. Nothing may sequence tools except the model. |
| **Added** `zeroclaw/tools-mcp/` — MCP stdio server with `screenshot` + `analyze_image` | ZeroClaw 0.8.2's real extension mechanism for custom tools is MCP servers (`[[mcp.servers]]`), not config-defined "traits". This registers our two tools so the agent can *choose* them. |
| **Added** `postprocess/` — validation sidecar | The brief's step 6: pure-code schema validation with exactly one retry nudge (the only permitted intervention), plus per-stage latency logs. Kept outside ZeroClaw because 0.8.2 has no response-middleware hook. |
| **Added** `mock/mock_llm_server.py` — a mock that plays the *agent* role | Lets the full loop run and be CI-tested on a laptop: turn 1 returns a screenshot tool call, turn 2 an analyze_image call, turn 3 the final JSON — with flags to return malformed JSON once (retry test) or always (error test). |
| **Moved** `server/mock_vlm_server.py` → `mock/`, added `stub_tizenscreenshot.sh`, moved sample PNG to `mock/assets/` | All fakes in one place; the stub simulates the TV binary by copying a bundled PNG to the output path. |
| **Added** `zeroclaw/workspace/SOUL.md` | The agent's system prompt: role, tool guidance, and the JSON schema embedded verbatim, per the brief. |
| **Rewrote** `scripts/test_e2e.sh` | Now boots the REAL `zeroclaw` gateway with an isolated config dir — the previous version tested our own sidecar, which proved nothing about ZeroClaw's agent loop. Asserts the tool ORDER the LLM chose, not just the output. |
| **Updated** `tizen-app/js/{config,main}.js` | Agent loops take longer than a fixed pipeline; the app now shows rotating progressive status ("Agent is deciding which tools to use…") and a client timeout ≥ the sidecar's budget. |

### v2.1 — making it actually pass against real ZeroClaw (2026-07-09)

The refactor above was written but had never passed e2e — the session that
built it was cut off. Getting green required five fixes, each rooted in how
ZeroClaw 0.8.2 *actually* behaves (verified by reading its source):

| # | Change | Purpose / root cause |
|---|---|---|
| 1 | **Config restructured** to `[agents.default]` + `[risk_profiles.default]` + `[runtime_profiles.tv]` + `[mcp_bundles.tv-tools]` (in `test_e2e.sh` and the TV template) | Gateway chat **rejects** any request unless an `[agents.<alias>]` entry exists, its `risk_profile` resolves, and MCP servers are granted through `mcp_bundles` — "omission is not a grant". Loop tunables live on runtime profiles; the `[agent]` block v2 used simply doesn't exist. |
| 2 | **Mock LLM taught native OpenAI tool-calling** (tool specs from the request body, `tool_calls` in the response) alongside the XML fallback | The `vllm` provider family **always** uses native tool calling — the `native_tools = false` override is only honored by the Groq factory in 0.8.2. Tool specs never appear in the system prompt on this path. Consequence for production: the DGX endpoint must be served with vLLM's `--enable-auto-tool-choice` + a `--tool-call-parser` matching the model. |
| 3 | **SOUL.md moved** to `<config>/agents/<alias>/workspace/SOUL.md` | That's the per-agent workspace 0.8.2 actually reads; the root-level locations v2 used were silently ignored (`[File not found: SOUL.md]` in the built prompt). |
| 4 | **`excluded_tools = ["screenshot"]`** added to the risk profile | ZeroClaw ships a *built-in* `screenshot` tool that shadowed our MCP `tv__screenshot` — during testing it captured the *laptop's* screen and returned prose instead of `{"image_path": ...}`, silently breaking the chain. |
| 5 | **`analyze_image.image_path` made optional**, falling back to the most recent screenshot (in-process, then `SCREENSHOT_OUTPUT`); mock omits the argument when masked | 0.8.2's media pipeline rewrites any real image path in a tool result to `[IMAGE:...]` and shows a non-vision LLM only `[media attachment]` — the orchestrator can *never* see the literal path. The fallback keeps the handoff working while the LLM still chooses the tools and their order. This is an accommodation of a runtime limitation, not a hardcoded sequence. |

Follow-on repairs in the same session:

| Change | Purpose |
|---|---|
| Mock's path extraction made escape-tolerant (`\"image_path\":\"…\"`), returns `None` when masked | Tool results arrive wrapped in an MCP JSON envelope with escaped quotes; the old regex missed them. |
| `scripts/deploy_tv.sh` rewritten | It still deployed v1 files that no longer exist (`analyze-screen-sidecar`, `sidecar.toml`). Now pushes `tv-screen-tools` + `analyze-screen-postprocess`, puts SOUL.md in the per-agent workspace, and prints agent-loop smoke tests. |
| `zeroclaw/config/config.toml` rewritten + annotated | The TV template carried the broken v2 shape; it now carries the verified shape with every gotcha documented inline. |
| `test_e2e.sh` preflight for `fastapi`/`uvicorn` | The mocks died with a bare `ModuleNotFoundError` before; now the script fails with an actionable message. |
| `STATUS.md` added; this README rewritten | The README described the deleted v1 architecture. STATUS.md is the living goal/done/todo document. |

---

## Honest limits & open items

- **Native tool calling is load-bearing.** If the DGX endpoint can't do
  OpenAI-style tool calls, the loop won't run on the `vllm` provider family
  in 0.8.2 (the XML text fallback isn't reachable there). Fix on the serving
  side (vLLM flags above) — not by forcing the chain in code.
- **Only tested against mocks + the real gateway on a Mac.** The real TV
  still needs: `deploy_tv.sh`, a subprocess-spawn check (decides
  `SCREENSHOT_MODE=exec` vs `watch`), real LLM/VLM URLs, and ARM
  cross-compilation of both Rust binaries (`rustup target add …`, see
  `deploy_tv.sh`'s `TARGET_TRIPLE`).
- **POC security posture:** loopback-only gateway with pairing disabled, and
  `risk_profiles.level = "full"` so tools run without approval prompts.
  Re-enable pairing + a `GATEWAY_TOKEN` before exposing anything.
- **The path-masking accommodation** (change 5) means the *image handoff*
  between the two tools is resolved by the tool server, not spelled out by
  the LLM, on 0.8.2. A ZeroClaw version that lets MCP tools opt out of media
  canonicalization would remove the need for it.

## Troubleshooting quick table

| Symptom | Cause → fix |
|---|---|
| 500 `LLM request failed`, trace says `no configured [agents.<alias>] entry` | Config missing the agent/profile/bundle shape → copy `zeroclaw/config/config.toml` structure |
| Tool result is prose "Screenshot saved to…" | Built-in `screenshot` shadowing → keep `excluded_tools = ["screenshot"]` |
| `analyze_image` gets `[media attachment]` | Expected on 0.8.2 → tool falls back to latest screenshot automatically |
| LLM answers without calling tools | DGX serving lacks native tool calls → vLLM `--enable-auto-tool-choice --tool-call-parser …` |
| System prompt ignored | SOUL.md in the wrong place → `<config>/agents/<alias>/workspace/SOUL.md` |
| Tizen fetch blocked | `config.xml` needs internet privilege + `allow-navigation` for localhost (already set) |
| vLLM 413/400 on the image | Keep `DOWNSCALE_ENABLED=true` (1280px long edge) or raise vLLM body limits |
| Total timeout | Raise postprocess `TOTAL_TIMEOUT_SECS` + app `REQUEST_TIMEOUT_MS` together; check per-stage logs for the slow leg |
