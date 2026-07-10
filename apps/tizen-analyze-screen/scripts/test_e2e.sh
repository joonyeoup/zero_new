#!/usr/bin/env bash
# End-to-end test of the AGENTIC analyze-screen flow on the local machine,
# using the REAL ZeroClaw binary with an isolated config dir:
#
#   postprocess sidecar -> zeroclaw gateway (real agent loop, xml dispatcher)
#     agent LLM  = mock_llm_server.py  (returns <tool_call> tags)
#     tools      = tv-screen-tools MCP server (stdio)
#     screenshot = stub_tizenscreenshot.sh (copies sample PNG)
#     VLM        = mock_vlm_server.py
#
# Asserts: schema-valid final JSON, the tool-call ORDER the LLM chose
# (screenshot before analyze_image), that the VLM actually received an image,
# the system prompt reached the LLM, the single-retry path, and the
# structured-error path.
#
# Requires the zeroclaw binary: set ZEROCLAW_BIN, or have `zeroclaw` in PATH,
# or run from inside the zeroclaw source tree (../../target/release/zeroclaw).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(dirname "$HERE")"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/analyze-agentic-e2e.XXXXXX")"

LLM_PORT="${LLM_PORT:-8010}"
VLM_PORT="${VLM_PORT:-8008}"
GW_PORT="${GW_PORT:-42655}"
PP_PORT="${PP_PORT:-8790}"
LLM="http://127.0.0.1:${LLM_PORT}"
VLM="http://127.0.0.1:${VLM_PORT}"
PP="http://127.0.0.1:${PP_PORT}"

PIDS=()
cleanup() {
    for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
    rm -rf "$WORK"
}
trap cleanup EXIT

say()  { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }
pass() { printf '\033[1;32mPASS\033[0m %s\n' "$*"; }
fail() { printf '\033[1;31mFAIL\033[0m %s\n' "$*"; echo "--- gateway log tail:"; tail -30 "$WORK/gateway.log" 2>/dev/null; exit 1; }

wait_http() { # url, name, tries
    for _ in $(seq 1 "${3:-60}"); do
        curl -sf "$1" >/dev/null 2>&1 && return 0
        sleep 0.5
    done
    fail "$2 did not come up at $1"
}

find_zeroclaw() {
    if [ -n "${ZEROCLAW_BIN:-}" ]; then echo "$ZEROCLAW_BIN"; return; fi
    if command -v zeroclaw >/dev/null 2>&1; then command -v zeroclaw; return; fi
    for p in "$ROOT/../../target/release/zeroclaw" "$ROOT/../../target/debug/zeroclaw"; do
        [ -x "$p" ] && { echo "$p"; return; }
    done
    echo ""
}

# usage: assert_schema <file> <expect_error: yes|no>
assert_schema() {
    python3 - "$1" "$2" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
expect_error = sys.argv[2] == "yes"
errs = []
def need(k, t):
    if not isinstance(data.get(k), t): errs.append(f"{k} wrong/missing")
need("screen_type", str); need("title", str); need("summary", str)
if not isinstance(data.get("detected_elements"), list):
    errs.append("detected_elements not a list")
else:
    for i, el in enumerate(data["detected_elements"]):
        if not isinstance(el.get("name"), str): errs.append(f"el[{i}].name")
        if not isinstance(el.get("description"), str): errs.append(f"el[{i}].description")
        c = el.get("confidence")
        if not isinstance(c, (int, float)) or not 0 <= c <= 1: errs.append(f"el[{i}].confidence")
if not (isinstance(data.get("suggested_actions"), list)
        and all(isinstance(a, str) for a in data["suggested_actions"])):
    errs.append("suggested_actions")
err = data.get("error", "MISSING")
if expect_error:
    if not (isinstance(err, dict) and isinstance(err.get("code"), str)):
        errs.append("error object malformed")
elif err is not None:
    errs.append(f"unexpected error field: {err}")
if errs:
    print("schema violations:", "; ".join(errs)); sys.exit(1)
PY
}

post_analyze() { # outfile -> echoes http status
    curl -s -o "$1" -w '%{http_code}' -X POST "$PP/analyze-screen" \
         -H 'Content-Type: application/json' \
         -d '{"message": "Analyze what is currently on my screen."}' \
         --max-time 120
}

llm_mode() { curl -sf -X POST "$LLM/_mode" -d "{\"mode\":\"$1\"}" >/dev/null; }

# decisions made by the mock LLM, in order, e.g.
# "tool_call:screenshot tool_call:analyze_image final:valid"
llm_decisions() {
    curl -sf "$LLM/_calls" | python3 -c \
        'import json,sys; print(" ".join(c["decision"] for c in json.load(sys.stdin)["calls"]))'
}

# ---------------------------------------------------------------- binaries
say "locating/building binaries"
ZC="$(find_zeroclaw)"
[ -n "$ZC" ] || fail "zeroclaw binary not found — set ZEROCLAW_BIN or build the zeroclaw repo (cargo build --release --bin zeroclaw)"
echo "zeroclaw: $ZC ($("$ZC" --version 2>/dev/null | head -1 || true))"
cargo build --release --quiet --manifest-path "$ROOT/zeroclaw/tools-mcp/Cargo.toml"
cargo build --release --quiet --manifest-path "$ROOT/postprocess/Cargo.toml"
TOOLS_BIN="$ROOT/zeroclaw/tools-mcp/target/release/tv-screen-tools"
PP_BIN="$ROOT/postprocess/target/release/analyze-screen-postprocess"

# ------------------------------------------------------------------- mocks
say "starting mock LLM + mock VLM + stub screenshot"
python3 -c 'import fastapi, uvicorn' 2>/dev/null \
    || fail "the mocks need fastapi+uvicorn — pip install fastapi uvicorn (a venv on PATH works)"
python3 "$ROOT/mock/mock_llm_server.py" --port "$LLM_PORT" &
PIDS+=($!); disown 2>/dev/null || true
python3 "$ROOT/mock/mock_vlm_server.py" --port "$VLM_PORT" &
PIDS+=($!); disown 2>/dev/null || true
wait_http "$LLM/health" "mock LLM"
wait_http "$VLM/health" "mock VLM"
SHOT_OUT="$WORK/screenshot.png"

# --------------------------------------------------- isolated zeroclaw home
say "writing isolated ZeroClaw config ($WORK)"
mkdir -p "$WORK/config"
# Verified working shape for ZeroClaw 0.8.2 gateway chat (see STATUS.md):
# an [agents.<alias>] entry is REQUIRED, its risk_profile must resolve, MCP
# servers are granted only via mcp_bundles, and runtime tunables live on
# [runtime_profiles.<alias>]. The built-in `screenshot` tool must be
# excluded or it shadows the MCP tv__screenshot.
cat > "$WORK/config/config.toml" <<EOF
schema_version = 3

[providers.models.vllm.dgx]
uri = "${LLM}/v1"
model = "mock-agent"
wire_api = "chat_completions"
timeout_secs = 30
max_tokens = 2048

[agents.default]
model_provider = "vllm.dgx"
risk_profile = "default"
runtime_profile = "tv"
mcp_bundles = ["tv-tools"]

[runtime_profiles.tv]
agentic = true
max_tool_iterations = 6
max_history_messages = 16

[risk_profiles.default]
level = "full"
auto_approve = ["tv__screenshot", "tv__analyze_image"]
excluded_tools = ["screenshot"]

[mcp_bundles.tv-tools]
servers = ["tv"]

[mcp]
enabled = true

[[mcp.servers]]
name = "tv"
transport = "stdio"
command = "${TOOLS_BIN}"

[mcp.servers.env]
SCREENSHOT_MODE = "exec"
SCREENSHOT_BIN = "${ROOT}/mock/stub_tizenscreenshot.sh"
SCREENSHOT_OUTPUT = "${SHOT_OUT}"
SCREENSHOT_TIMEOUT_SECS = "10"
VLM_BASE_URL = "${VLM}/v1"
VLM_MODEL = "mock-vlm"
VLM_TIMEOUT_SECS = "60"
DOWNSCALE_ENABLED = "true"
DOWNSCALE_MAX_EDGE = "1280"

[gateway]
host = "127.0.0.1"
port = ${GW_PORT}
require_pairing = false

[memory]
backend = "none"
EOF
# Agent system prompt: the per-agent workspace is <config>/agents/<alias>/workspace.
mkdir -p "$WORK/config/agents/default/workspace"
cp "$ROOT/zeroclaw/workspace/SOUL.md" "$WORK/config/agents/default/workspace/SOUL.md"
export SCREENSHOT_OUTPUT="$SHOT_OUT"   # for the stub when run standalone

say "starting real zeroclaw gateway"
ZEROCLAW_CONFIG_DIR="$WORK/config" ZEROCLAW_DATA_DIR="$WORK/data" \
    "$ZC" gateway > "$WORK/gateway.log" 2>&1 &
PIDS+=($!); disown 2>/dev/null || true
wait_http "http://127.0.0.1:${GW_PORT}/health" "zeroclaw gateway"

say "starting postprocess sidecar"
POSTPROCESS_PORT="$PP_PORT" GATEWAY_URL="http://127.0.0.1:${GW_PORT}" \
    TOTAL_TIMEOUT_SECS=150 "$PP_BIN" > "$WORK/postprocess.log" 2>&1 &
PIDS+=($!); disown 2>/dev/null || true
wait_http "$PP/health" "postprocess"
pass "full stack up (real zeroclaw + mocks)"

# ------------------------------------------------------------------- tests
say "test 1: agent loop happy path"
llm_mode valid
STATUS="$(post_analyze "$WORK/r1.json")"
[ "$STATUS" = "200" ] || fail "expected HTTP 200, got $STATUS: $(cat "$WORK/r1.json")"
assert_schema "$WORK/r1.json" no || fail "schema"
pass "schema-valid JSON returned"

DECISIONS="$(llm_decisions)"
echo "LLM decisions: $DECISIONS"
case "$DECISIONS" in
    "tool_call:screenshot tool_call:analyze_image final:valid")
        pass "LLM drove the tool chain in order (screenshot -> analyze_image -> final)" ;;
    *) fail "unexpected tool-call sequence: $DECISIONS" ;;
esac

curl -sf "$VLM/_calls" | grep -q '"has_image": *true' \
    && pass "VLM received an actual image payload" \
    || fail "VLM never saw an image"

curl -sf "$LLM/_last_system" | grep -q "screen-analysis assistant" \
    && pass "SOUL.md system prompt reached the agent LLM" \
    || fail "system prompt did not contain SOUL.md content"

say "test 2: malformed final answer -> single retry nudge in same session"
llm_mode malformed_once
STATUS="$(post_analyze "$WORK/r2.json")"
[ "$STATUS" = "200" ] || fail "expected HTTP 200 after retry, got $STATUS: $(cat "$WORK/r2.json")"
assert_schema "$WORK/r2.json" no || fail "schema"
DECISIONS="$(llm_decisions)"
echo "LLM decisions: $DECISIONS"
grep -q "final:malformed" <<<"$DECISIONS" || fail "malformed answer never produced"
grep -q "final:corrected" <<<"$DECISIONS" || fail "retry nudge never reached the same conversation"
grep -q "retry_turn" "$WORK/postprocess.log" || fail "postprocess retry stage missing"
pass "one retry nudge recovered a valid answer"

say "test 3: unrepairable output -> structured error object"
llm_mode always_malformed
STATUS="$(post_analyze "$WORK/r3.json")"
[ "$STATUS" = "422" ] || fail "expected HTTP 422, got $STATUS: $(cat "$WORK/r3.json")"
assert_schema "$WORK/r3.json" yes || fail "schema"
grep -q "AGENT_OUTPUT_INVALID" "$WORK/r3.json" || fail "wrong error code"
pass "structured error returned after failed retry"

say "all agentic e2e tests passed"
echo "-- per-stage latency (postprocess):"
grep "done in" "$WORK/postprocess.log" | tail -6
echo "-- tool executions (MCP server, via gateway log):"
grep -a "tv-screen-tools" "$WORK/gateway.log" | tail -6 || true
