/* Gateway location + timeouts. Edit here (or regenerate at deploy time) —
 * no values are baked into main.js. */
var APP_CONFIG = {
    // The postprocess sidecar in front of ZeroClaw's gateway on the TV
    GATEWAY_URL: "http://127.0.0.1:8787/analyze-screen",
    // The PNG the agent captured, served by that same sidecar. Loaded as an
    // <img> off the analysis path — it never blocks or delays the agent loop.
    SCREENSHOT_URL: "http://127.0.0.1:8787/screenshot",
    // One stat() on the sidecar: tells us when a fresh capture exists so the
    // image can be preloaded while the vision model is still thinking.
    SCREENSHOT_INFO_URL: "http://127.0.0.1:8787/screenshot-info",
    // How often to ask (only while the loading overlay is up). Loopback +
    // one stat per poll; set to 0 to disable the preload entirely.
    SCREENSHOT_POLL_MS: 1000,
    // Client-side cap; must be >= the sidecar's TOTAL_TIMEOUT_SECS (150s)
    REQUEST_TIMEOUT_MS: 155000,
    // Rotating status hints while the agent loop runs (label, at_seconds)
    PROGRESS_STAGES: [
        ["Sending to the ZeroClaw agent…", 0],
        ["Agent is deciding which tools to use…", 2],
        ["Capturing the screen…", 5],
        ["Vision model is reading the screen…", 12],
        ["Agent is composing the answer…", 45]
    ]
};
