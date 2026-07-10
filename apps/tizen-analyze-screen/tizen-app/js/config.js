/* Gateway location + timeouts. Edit here (or regenerate at deploy time) —
 * no values are baked into main.js. */
var APP_CONFIG = {
    // The postprocess sidecar in front of ZeroClaw's gateway on the TV
    GATEWAY_URL: "http://127.0.0.1:8787/analyze-screen",
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
