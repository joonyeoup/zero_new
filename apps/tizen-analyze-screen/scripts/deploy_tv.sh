#!/usr/bin/env bash
# Deploy the agentic analyze-screen stack to a Samsung Tizen TV over sdb.
#
# What lands on the TV:
#   tv-screen-tools            MCP stdio server (screenshot + analyze_image)
#   analyze-screen-postprocess validation sidecar the Tizen app calls (:8787)
#   ZeroClaw config sections   zeroclaw/config/config.toml (merge by hand)
#   SOUL.md                    agent system prompt -> <config>/agents/main/workspace/
#   AnalyzeScreen.wgt          the Tizen web app
#
# Prereqs on this machine:
#   - Tizen Studio CLI (`tizen`, `sdb`) in PATH
#   - A TV in developer mode with this machine's IP whitelisted
#     (Apps panel -> enter 12345 on the remote -> Developer mode ON + host IP)
#   - Both Rust binaries cross-compiled for the TV's architecture (see README
#     "Cross-compiling"); point TOOLS_BIN / POSTPROCESS_BIN at them.
#
# Usage:
#   TV_IP=192.168.1.50 ./deploy_tv.sh [steps...]
#   steps: connect binaries config app smoke all   (default: all)
set -euo pipefail

# ----------------------------------------------------------- placeholders
TV_IP="${TV_IP:-<TV_IP_HERE>}"                       # e.g. 192.168.1.50
TV_HOME="${TV_HOME:-/opt/usr/home/owner}"            # writable dir on the TV
ZEROCLAW_CONFIG_DIR="${ZEROCLAW_CONFIG_DIR:-$TV_HOME/.zeroclaw}"  # {{ZEROCLAW_CONFIG_PATH}} dir
TARGET_TRIPLE="${TARGET_TRIPLE:-armv7-unknown-linux-gnueabi}"
TOOLS_BIN="${TOOLS_BIN:-../zeroclaw/tools-mcp/target/$TARGET_TRIPLE/release/tv-screen-tools}"
POSTPROCESS_BIN="${POSTPROCESS_BIN:-../postprocess/target/$TARGET_TRIPLE/release/analyze-screen-postprocess}"
CERT_PROFILE="${CERT_PROFILE:-tvcert}"               # tizen security-profile name
HERE="$(cd "$(dirname "$0")" && pwd)"
APP_DIR="$HERE/../tizen-app"
WGT_OUT="${WGT_OUT:-/tmp/AnalyzeScreen.wgt}"

if [[ "$TV_IP" == "<TV_IP_HERE>" ]]; then
    echo "Set TV_IP first, e.g.: TV_IP=192.168.1.50 $0" >&2
    exit 1
fi

step() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }

do_connect() {
    step "sdb connect $TV_IP"
    sdb connect "$TV_IP:26101"
    sdb devices
}

do_binaries() {
    step "push tv-screen-tools (MCP) + analyze-screen-postprocess"
    for pair in "$TOOLS_BIN:tv-screen-tools" "$POSTPROCESS_BIN:analyze-screen-postprocess"; do
        src="${pair%%:*}"; dst="${pair##*:}"
        [[ -f "$src" ]] || {
            echo "$dst not found at $src — cross-compile first (see README)" >&2
            exit 1
        }
        sdb push "$src" "$TV_HOME/$dst"
        sdb shell "chmod +x $TV_HOME/$dst"
    done
}

do_config() {
    step "push ZeroClaw config sections + agent system prompt"
    sdb push "$HERE/../zeroclaw/config/config.toml" "$TV_HOME/analyze-screen-config.toml"
    sdb shell "mkdir -p $ZEROCLAW_CONFIG_DIR/agents/main/workspace"
    sdb push "$HERE/../zeroclaw/workspace/SOUL.md" "$ZEROCLAW_CONFIG_DIR/agents/main/workspace/SOUL.md"
    echo "NOW (one-time, by hand over sdb shell):"
    echo "  1. Fill every REPLACE_ME in $TV_HOME/analyze-screen-config.toml"
    echo "     (LLM/VLM URLs+models, tizenscreenshot path + output path)."
    echo "  2. Merge its sections into $ZEROCLAW_CONFIG_DIR/config.toml"
    echo "     (agents/risk_profiles/runtime_profiles/mcp_bundles/mcp/gateway)."
    echo "  3. Restart the zeroclaw daemon/gateway."
}

do_app() {
    step "package + install Tizen web app"
    # Signing requires a Samsung certificate profile created in Tizen Studio
    # (Certificate Manager) that includes your TV's DUID.
    tizen package -t wgt -s "$CERT_PROFILE" -- "$APP_DIR" -o "$(dirname "$WGT_OUT")"
    mv "$(dirname "$WGT_OUT")"/*.wgt "$WGT_OUT" 2>/dev/null || true
    tizen install -n "$WGT_OUT" -t "$(sdb devices | awk 'NR==2{print $1}')"
    echo "Launch from the TV's Apps rail, or: tizen run -p AnLzScrn01"
}

do_smoke() {
    step "smoke test on the TV"
    echo "-- 1. can the TV spawn subprocesses? (decides SCREENSHOT_MODE exec vs watch)"
    echo "   sdb shell '<PATH_TO_BINARY>' && sdb shell 'ls -l <SCREENSHOT_OUTPUT_PATH>'"
    echo "   If this fails with a permission error, set SCREENSHOT_MODE=watch in"
    echo "   the [mcp.servers.env] block."
    echo "-- 2. is the gateway up with the agent configured?"
    echo "   sdb shell 'curl -s http://127.0.0.1:42617/health'"
    echo "-- 3. start the postprocess sidecar:"
    echo "   sdb shell 'GATEWAY_URL=http://127.0.0.1:42617 POSTPROCESS_PORT=8787 \\"
    echo "              $TV_HOME/analyze-screen-postprocess &'"
    echo "-- 4. one full agent loop from the TV shell (expect schema JSON back):"
    echo "   sdb shell 'curl -s -X POST http://127.0.0.1:8787/analyze-screen \\"
    echo "              -H \"Content-Type: application/json\" \\"
    echo "              -d \"{\\\"message\\\": \\\"Analyze what is currently on my screen.\\\"}\"'"
}

steps=("${@:-all}")
for s in "${steps[@]}"; do
    case "$s" in
        connect)  do_connect ;;
        binaries) do_binaries ;;
        config)   do_config ;;
        app)      do_app ;;
        smoke)    do_smoke ;;
        all)      do_connect; do_binaries; do_config; do_app; do_smoke ;;
        *)        echo "unknown step: $s (connect|binaries|config|app|smoke|all)" >&2; exit 1 ;;
    esac
done
