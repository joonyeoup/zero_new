# Wiring `analyze-screen-tool` into ZeroClaw

Four edits outside the new crate. All of them are additive.

Place the crate at:

```
apps/tizen-analyze-screen/analyze-screen-tool/
├── Cargo.toml
└── src/lib.rs
```

---

## 1. Workspace members — root `Cargo.toml`

Add to the `members` array (line 2):

```toml
"apps/tizen-analyze-screen/analyze-screen-tool",
```

## 2. Feature flag — `crates/zeroclaw-runtime/Cargo.toml`

The tool is Samsung-TV-specific and shells out to `tzcapturesample`. Gating it
keeps it out of every other gateway build.

```toml
[features]
tizen-analyze-screen = ["dep:analyze-screen-tool"]

[dependencies]
analyze-screen-tool = { path = "../../apps/tizen-analyze-screen/analyze-screen-tool", optional = true }
```

A cargo feature rather than a config flag (the `gemini_cli.enabled` pattern)
because a config flag would still compile the `image` crate and an HTTP client
into every build. If you'd rather have runtime togglability, add
`[analyze_screen] enabled = false` to `zeroclaw-config` and use the
`root_config.gemini_cli.enabled` shape instead — both work.

## 3. Registration — `crates/zeroclaw-runtime/src/tools/mod.rs`

Next to the existing re-export at line ~116:

```rust
#[cfg(feature = "tizen-analyze-screen")]
pub use analyze_screen_tool::AnalyzeScreenTool;
```

Then in the same block as the vision tools (~line 1147), right after
`ScreenshotTool`:

```rust
// Vision tools are always available
tool_arcs.push(Arc::new(ScreenshotTool::new(security.clone())));
tool_arcs.push(Arc::new(RateLimitedTool::new(
    PathGuardedTool::new(ImageInfoTool::new(security.clone()), security.clone()),
    security.clone(),
)));

// TV screen analysis (Tizen only)
#[cfg(feature = "tizen-analyze-screen")]
tool_arcs.push(Arc::new(AnalyzeScreenTool::new(security.clone())));
```

Consider wrapping it in `RateLimitedTool` as the CLI-delegation tools do — each
call costs a capture plus a VLM round trip, so an agent in a loop could get
expensive. `PathGuardedTool` is not applicable: the tool takes no path
arguments.

## 4. Config — `analyze-screen-config.toml`

The MCP server is gone, so this whole block is now dead:

```toml
[[mcp.servers]]
name = "tv"
transport = "stdio"
command = "/opt/usr/home/owner/tv-screen-tools"

[mcp.servers.env]
SCREENSHOT_BIN = "/root/tzcapturesample"
...
```

Those variables were the gateway's instructions for its stdio child. With the
tool compiled in, they must be in the **gateway's own environment** instead.

The risk profile also changes — the tool is now `analyze_screen`, not
`tv__analyze_screen` (the `tv__` prefix was the MCP server's namespace):

```toml
[risk_profiles.default]
level = "full"
auto_approve = ["analyze_screen"]
excluded_tools = ["screenshot"]   # built-in still shadows nothing useful on TV
```

Keep `allowed_tools` restricted. That fix was worth ~11,500 input tokens per
turn and it applies just as much to the native tool as it did to the MCP one.

---

## Build

```bash
cargo build --release \
  --target armv7-unknown-linux-musleabihf \
  -p zeroclaw-gateway \
  --features tizen-analyze-screen
```

Verify the tool is present in the binary before deploying:

```bash
strings target/armv7-unknown-linux-musleabihf/release/zeroclaw-gateway \
  | grep analyze_screen
```

Nothing back means the feature didn't reach the compiler — the same silent
failure mode that cost hours with `--features serve` on the MCP binary.

## Run

```bash
cd /root
SCREENSHOT_BIN=/root/tzcapturesample \
SCREENSHOT_OUTPUT=/root/capture.png \
SCREENSHOT_TIMEOUT_SECS=10 \
VLM_BASE_URL=http://172.28.138.103:8080/v1 \
VLM_MODEL=Qwen3.6-35B-A3B-UD-Q4_K_XL.gguf \
VLM_TIMEOUT_SECS=60 \
DOWNSCALE_ENABLED=true DOWNSCALE_MAX_EDGE=1280 \
./zeroclaw-gateway &
```

The `cd /root` is now belt-and-braces rather than load-bearing: the tool pins
the capture child's `current_dir` to `SCREENSHOT_OUTPUT`'s parent, so the
"every process must start from /root" rule no longer applies to it.

Then the sidecar, with **no** `TOOLS_HTTP_URL` — this is the agentic path:

```bash
/opt/usr/home/owner/analyze-screen-postprocess &
time curl -i -X POST localhost:8787/analyze-screen
```

## What this changes about the architecture

Three processes become two. `tv-screen-tools` is no longer deployed at all:

```
before:  sidecar → gateway → agent → MCP stdio child → capture + VLM
after:   sidecar → gateway → agent → analyze_screen (in-process) → capture + VLM
```

The sidecar's `AnalyzeBackend` trait is untouched — `Webhook::fetch` still
POSTs to `/webhook` and doesn't care what's behind it. `Direct` still points at
the standalone HTTP server if you keep that binary around for comparison.

Expect the agentic path to get modestly faster: no process spawn, no JSON-RPC
serialization, no stdio round trip. The agent loop overhead (~7s of the ~10s)
is unchanged, so don't expect it to approach the direct path's ~3s — the cost
was never the transport.

---

## Seams to check before it compiles

1. **`reqwest` in the workspace** — `grep -n "reqwest" crates/zeroclaw-tools/Cargo.toml`.
   Match the version and features; must be rustls for musl.
2. **`workspace = true` deps** — confirm `async-trait`, `base64`, `image`, and
   `tokio` are all in the root `[workspace.dependencies]`. If not, pin versions
   directly.
3. **`ToolKind` variant** — `ScreenshotTool` uses `Plugin`. Check
   `crates/zeroclaw-api/src/attribution.rs` for something better-fitting.
4. **`SecurityPolicy` fields** — the test module assumes `autonomy`,
   `workspace_dir`, and a `Default` impl, copied from `screenshot.rs`'s tests.
5. **`spec()`** — assumed to be a provided method on `Tool` with a default
   body. It's called in `screenshot.rs`'s tests but absent from its impl, so
   this should hold.
