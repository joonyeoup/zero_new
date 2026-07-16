#!/usr/bin/env bash
# Deploy the agentic analyze-screen stack to a Samsung Tizen TV over SSH (no sdb required).
#
# What lands on the TV:
#   tv-screen-tools            MCP stdio server (screenshot + analyze_image)
#   analyze-screen-postprocess validation sidecar the Tizen app calls (:8787)
#   ZeroClaw config sections   zeroclaw/config/config.toml (merge by hand)
#   SOUL.md                    agent system prompt -> <config>/agents/main/workspace/
#   AnalyzeScreen.wgt          the Tizen web app (copied, manual install via TV)
#
# Prereqs on this machine:
#   - SSH access to TV (Developer mode must be enabled)
#   - Both Rust binaries cross-compiled for the TV's architecture
#   - scp and ssh commands available
#
# Usage:
#   TV_IP=192.168.1.50 SSH_USER=root SSH_PASS=secinit ./deploy_tv_ssh.sh [steps...]
#   steps: connect binaries config app smoke all   (default: all)
#
# If SSH_PASS is not set, assumes key-based auth or will prompt for password.
set -euo pipefail

# ----------------------------------------------------------- placeholders
TV_IP="${TV_IP:-<TV_IP_HERE>}"                       # e.g. 192.168.1.50
TV_USER="${SSH_USER:-root}"                          # SSH user (default: root)
TV_PASS="${SSH_PASS:-}"                              # SSH password (optional, for sshpass)
TV_HOME="${TV_HOME:-/opt/usr/home/owner}"            # writable dir on the TV
ZEROCLAW_CONFIG_DIR="${ZEROCLAW_CONFIG_DIR:-$TV_HOME/.zeroclaw}"
TARGET_TRIPLE="${TARGET_TRIPLE:-armv7-unknown-linux-gnueabihf}"
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

# SSH command - uses sshpass if TV_PASS is set, otherwise plain ssh
if [[ -n "$TV_PASS" ]]; then
    SSH_CMD="sshpass -p '$TV_PASS' ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
    SCP_CMD="sshpass -p '$TV_PASS' scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
else
    SSH_CMD="ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
    SCP_CMD="scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
fi

step() { printf '\n\033[1;34m== %s ==\033[0m\n' "$*"; }

do_connect() {
    step "Testing SSH connection to $TV_USER@$TV_IP"
    if command -v sshpass >/dev/null 2>&1 && [[ -n "$TV_PASS" ]]; then
        sshpass -p "$TV_PASS" ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null "$TV_USER@$TV_IP" "echo 'SSH connection successful'"
    else
        ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null "$TV_USER@$TV_IP" "echo 'SSH connection successful'"
    fi
}

do_binaries() {
    step "Push tv-screen-tools (MCP) + analyze-screen-postprocess"
    for pair in "$TOOLS_BIN:tv-screen-tools" "$POSTPROCESS_BIN:analyze-screen-postprocess"; do
        src="${pair%%:*}"; dst="${pair##*:}"
        if [[ ! -f "$src" ]]; then
            echo "$dst not found at $src — cross-compile first" >&2
            echo "Build with: cargo build --release --target $TARGET_TRIPLE" >&2
            exit 1
        fi
        $SCP_CMD "$src" "$TV_USER@$TV_IP:$TV_HOME/$dst"
        $SSH_CMD "$TV_USER@$TV_IP" "chmod +x $TV_HOME/$dst"
        echo "Pushed $dst to $TV_HOME/$dst"
    done
}

do_config() {
    step "Push ZeroClaw config sections + agent system prompt"
    # Create config directory
    $SSH_CMD "$TV_USER@$TV_IP" "mkdir -p $ZEROCLAW_CONFIG_DIR/agents/main/workspace"
    
    # Push config template
    $SCP_CMD "$HERE/../zeroclaw/config/config.toml" "$TV_USER@$TV_IP:$TV_HOME/analyze-screen-config.toml"
    
    # Push SOUL.md
    $SCP_CMD "$HERE/../zeroclaw/workspace/SOUL.md" "$TV_USER@$TV_IP:$ZEROCLAW_CONFIG_DIR/agents/main/workspace/SOUL.md"
    
    echo "Config files pushed. NOW (one-time, via SSH):"
    echo "  1. SSH into TV: $SSH_CMD $TV_USER@$TV_IP"
    echo "  2. Edit $TV_HOME/analyze-screen-config.toml"
    echo "     (Fill LLM/VLM URLs, models, tizenscreenshot path + output path)"
    echo "  3. Merge its sections into $ZEROCLAW_CONFIG_DIR/config.toml"
    echo "     (agents/risk_profiles/runtime_profiles/mcp_bundles/mcp/gateway)"
    echo "  4. Restart the zeroclaw daemon/gateway on TV"
}

do_app() {
    step "Package + copy Tizen web app"
    # Check if tizen command exists
    if ! command -v tizen >/dev/null 2>&1; then
        echo "Tizen CLI not found. Copying app source for manual packaging..." >&2
        echo "You can either:" >&2
        echo "  1. Install Tizen Studio for 'tizen package' command" >&2
        echo "  2. Manually package the app from $APP_DIR using Tizen IDE" >&2
        echo "  3. Skip app installation and test via curl only" >&2
        return 0
    fi
    
    # Package the app
    tizen package -t wgt -s "$CERT_PROFILE" -- "$APP_DIR" -o "$(dirname "$WGT_OUT")"
    mv "$(dirname "$WGT_OUT")"/*.wgt "$WGT_OUT" 2>/dev/null || true
    
    if [[ -f "$WGT_OUT" ]]; then
        # Copy the .wgt to TV (user needs to install manually via TV's package manager)
        $SCP_CMD "$WGT_OUT" "$TV_USER@$TV_IP:$TV_HOME/AnalyzeScreen.wgt"
        echo "App package copied to $TV_HOME/AnalyzeScreen.wgt"
        echo "Install via TV's package manager or use sdb if available:"
        echo "  sdb install $WGT_OUT"
    else
        echo "Failed to create .wgt package" >&2
    fi
}

do_smoke() {
    step "Smoke test on the TV"
    echo "-- 1. Test SSH access:"
    echo "   $SSH_CMD $TV_USER@$TV_IP 'uptime'"
    echo ""
    echo "-- 2. Verify binaries are executable:"
    echo "   $SSH_CMD $TV_USER@$TV_IP 'ls -la $TV_HOME/tv-screen-tools $TV_HOME/analyze-screen-postprocess'"
    echo ""
    echo "-- 3. Test screenshot binary (if tizenscreenshot exists):"
    echo "   $SSH_CMD $TV_USER@$TV_IP '$TV_HOME/tv-screen-tools --help 2>&1 || echo \"Binary exists but may need config\"'"
    echo ""
    echo "-- 4. Check if gateway is up (assuming zeroclaw is running):"
    echo "   $SSH_CMD $TV_USER@$TV_IP 'curl -s http://127.0.0.1:42617/health || echo \"Gateway not running\"'"
    echo ""
    echo "-- 5. Start postprocess sidecar:"
    echo "   $SSH_CMD $TV_USER@$TV_IP 'GATEWAY_URL=http://127.0.0.1:42617 POSTPROCESS_PORT=8787 $TV_HOME/analyze-screen-postprocess &'"
    echo ""
    echo "-- 6. Test full agent loop from TV shell:"
    echo "   $SSH_CMD $TV_USER@$TV_IP 'curl -s -X POST http://127.0.0.1:8787/analyze-screen -H \"Content-Type: application/json\" -d \"{\\\"message\\\": \\\"Analyze what is currently on my screen.\\\"}\"'"
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

echo ""
echo "=== Deployment Summary ==="
echo "Binaries deployed to: $TV_HOME/"
echo "Config template: $TV_HOME/analyze-screen-config.toml"
echo "Agent workspace: $ZEROCLAW_CONFIG_DIR/agents/main/workspace/SOUL.md"
echo ""
echo "Next steps:"
echo "1. SSH into TV and edit config with real LLM/VLM URLs"
echo "2. Merge config into $ZEROCLAW_CONFIG_DIR/config.toml"
echo "3. Restart zeroclaw daemon"
echo "4. Run smoke tests above"
