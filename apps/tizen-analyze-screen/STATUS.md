# STATUS — Agentic "Analyze Screen" on Tizen TV via ZeroClaw

_Last updated: 2026-07-08 (session 3 — agentic refactor debugging)._

## Goal

A true **agent-loop** demo on a Samsung Tizen TV: the Tizen app POSTs
"Analyze what is currently on my screen." to ZeroClaw's gateway, and
ZeroClaw's **LLM autonomously decides** to (a) call a `screenshot` tool,
(b) call an `analyze_image` tool (which sends the PNG to a VLM), and
(c) emit a final schema-conformant JSON answer. A thin pure-code
postprocess layer validates the JSON (one retry nudge max), and the TV app
renders it. **No hardcoded tool sequence anywhere** — the LLM drives.

Architecture: Tizen app → postprocess sidecar (`:8787`, validation only) →
ZeroClaw gateway `/webhook` (real agent loop) → MCP stdio server
`tv-screen-tools` (`screenshot` + `analyze_image` tools) → VLM.

## What is done

- **`zeroclaw/tools-mcp/`** — Rust MCP stdio server exposing `screenshot`
  (exec/watch modes, 10s timeout) and `analyze_image` (downscale → base64 →
  VLM chat/completions). All config via env vars. Builds clean.
- **`postprocess/`** — Rust sidecar: `POST /analyze-screen` → gateway
  `/webhook` (with `X-Session-Id`), lenient parse + schema validation, ONE
  retry nudge in the same session, structured error object, per-stage
  timing logs + `X-Timings-Ms` header.
- **`mock/`** — `mock_llm_server.py` (plays the agent deterministically;
  now speaks BOTH native OpenAI tool-calling and ZeroClaw's XML fallback),
  `mock_vlm_server.py`, `stub_tizenscreenshot.sh`.
- **`tizen-app/`** — 10-foot UI web app (unchanged from earlier session).
- **Debugged the real ZeroClaw 0.8.2 gateway agent loop end-to-end** (this
  session). Key findings, all now fixed in the local debug stack:
  1. Gateway chat **requires `[agents.<alias>]`** + resolvable
     `risk_profile` (`[risk_profiles.default]`) — a bare provider entry is
     not enough. MCP servers must be granted via `mcp_bundles` (omission is
     not a grant). Tunables (`max_tool_iterations`…) live on
     `[runtime_profiles.<alias>]`, NOT a top-level `[agent]` block.
  2. The `vllm` provider family **always uses native OpenAI tool-calling**
     (`native_tools = false` is only honored by the Groq factory), so tool
     specs go in the request body, not the system prompt. Mock updated to
     speak native protocol (and keep the XML fallback).
  3. The per-agent system prompt (SOUL.md) must be at
     `<config>/agents/<alias>/workspace/SOUL.md`.
  4. ZeroClaw has a **built-in `screenshot` tool** that shadows the MCP
     `tv__screenshot` → excluded via
     `risk_profiles.default.excluded_tools = ["screenshot"]`.
  5. **Media-path masking (0.8.2)**: any real image path in a tool result is
     promoted to `[IMAGE:...]` and replaced with `[media attachment]` before
     a non-vision LLM sees it → the orchestrator LLM can never pass a
     literal screenshot path to `analyze_image`.

## What needs to be done

All local work is DONE as of 2026-07-08 — `scripts/test_e2e.sh` passes all
assertions against the real ZeroClaw 0.8.2 gateway:

- [x] `analyze_image` `image_path` optional with last-screenshot fallback
      (works around 0.8.2 media-path masking); mock omits the masked arg.
- [x] Malformed→retry (same `X-Session-Id`) recovers; always-malformed→422
      structured error — both verified against the real gateway.
- [x] `scripts/test_e2e.sh` rewritten to the working config shape; all
      tests pass (`PATH=<venv>/bin:$PATH bash scripts/test_e2e.sh`).
- [x] `zeroclaw/config/config.toml` TV template updated + annotated.
- [x] `README.md` rewritten for the agentic architecture.
- [x] `scripts/deploy_tv.sh` rewritten (tools-mcp + postprocess binaries,
      SOUL.md to `agents/main/workspace/`, smoke steps).

Remaining (needs the user / real hardware):

- [ ] Commit the working tree in the inner repo
      (`apps/tizen-analyze-screen/.git`, branch `main`) when the user is
      ready.
- [ ] On-TV validation: run `deploy_tv.sh`, confirm subprocess spawn
      (else `SCREENSHOT_MODE=watch`), fill real LLM/VLM URLs.
- [ ] Confirm the DGX vLLM serve flags include `--enable-auto-tool-choice`
      + a `--tool-call-parser` matching the model (native tool calling is
      required — see finding 2).
- [ ] Cross-compile both Rust binaries for the TV's ARM triple.

## How to run the e2e locally

```sh
# needs: zeroclaw built at repo root (cargo build --release --bin zeroclaw),
# python3 with fastapi+uvicorn (venv is fine — put its bin/ on PATH)
bash scripts/test_e2e.sh
```

## Key facts / decisions

- ZeroClaw 0.8.2, config `schema_version = 3`; gateway `/webhook` honors
  `X-Session-Id` for conversation continuity (used by the retry nudge).
- MCP tools surface to the agent as `tv__screenshot` / `tv__analyze_image`.
- The DGX LLM endpoint is used through the `vllm` provider family
  (`[providers.models.vllm.dgx]`, `wire_api = "chat_completions"`) → the
  real DGX endpoint **must support native OpenAI tool calling** (vLLM:
  `--enable-auto-tool-choice`). The XML dispatcher fallback is NOT reachable
  on this path in 0.8.2 (see finding 2).
- TV facts still unknown: exact `tizenscreenshot` output path; whether the
  TV allows subprocess spawn (sidecar/tools support both `exec` and `watch`
  screenshot modes for this reason); real VLM/LLM URLs are placeholders.
