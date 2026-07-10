/* Analyze Screen — Tizen TV web app (ES5 for older TV browsers).
 *
 * Flow: button / remote ENTER -> POST to sidecar -> loading overlay ->
 * render validated JSON (or structured error). Every stage is logged with a
 * timestamp so per-stage latency is measurable from the web inspector.
 */
(function () {
    "use strict";

    var KEY = { ENTER: 13, RETURN: 10009, LEFT: 37, UP: 38, RIGHT: 39, DOWN: 40 };

    var els = {
        analyzeBtn: document.getElementById("analyze-btn"),
        statusLine: document.getElementById("status-line"),
        overlay: document.getElementById("overlay"),
        loading: document.getElementById("state-loading"),
        loadingText: document.getElementById("loading-text"),
        error: document.getElementById("state-error"),
        errorCode: document.getElementById("error-code"),
        errorMessage: document.getElementById("error-message"),
        result: document.getElementById("state-result"),
        resultType: document.getElementById("result-type"),
        resultTitle: document.getElementById("result-title"),
        resultSummary: document.getElementById("result-summary"),
        resultElements: document.getElementById("result-elements"),
        resultActions: document.getElementById("result-actions"),
        resultTimings: document.getElementById("result-timings"),
        closeBtn: document.getElementById("close-btn")
    };

    var busy = false;

    function log(stage, msg) {
        console.log("[" + new Date().toISOString() + "] [" + stage + "] " + msg);
    }

    /* ---------- overlay state machine ---------- */

    function showOnly(stateEl, withClose) {
        els.overlay.classList.remove("hidden");
        [els.loading, els.error, els.result].forEach(function (el) {
            el.classList.add("hidden");
        });
        stateEl.classList.remove("hidden");
        els.closeBtn.classList.toggle("hidden", !withClose);
        if (withClose) { els.closeBtn.focus(); }
    }

    function hideOverlay() {
        els.overlay.classList.add("hidden");
        els.analyzeBtn.focus();
    }

    function showLoading(text) {
        els.loadingText.textContent = text;
        showOnly(els.loading, false);
    }

    function showError(code, message) {
        els.errorCode.textContent = code || "UNKNOWN";
        els.errorMessage.textContent = message || "No details available.";
        showOnly(els.error, true);
    }

    function showResult(data, timings) {
        els.resultType.textContent = data.screen_type || "unknown";
        els.resultTitle.textContent = data.title || "(no title)";
        els.resultSummary.textContent = data.summary || "";

        els.resultElements.innerHTML = "";
        (data.detected_elements || []).forEach(function (el) {
            var li = document.createElement("li");
            var conf = typeof el.confidence === "number"
                ? Math.round(el.confidence * 100) + "%" : "";
            li.innerHTML =
                '<span class="el-conf">' + conf + '</span>' +
                '<span class="el-name"></span> — <span class="el-desc"></span>';
            li.querySelector(".el-name").textContent = el.name || "?";
            li.querySelector(".el-desc").textContent = el.description || "";
            els.resultElements.appendChild(li);
        });
        if (!els.resultElements.children.length) {
            els.resultElements.innerHTML = "<li>Nothing notable detected</li>";
        }

        els.resultActions.innerHTML = "";
        (data.suggested_actions || []).forEach(function (a) {
            var li = document.createElement("li");
            li.textContent = a;
            els.resultActions.appendChild(li);
        });

        els.resultTimings.textContent = timings ? "latency ms: " + timings : "";
        showOnly(els.result, true);
    }

    /* ---------- the pipeline call ---------- */

    function analyze() {
        if (busy) { return; }
        busy = true;
        els.analyzeBtn.disabled = true;
        var t0 = Date.now();
        log("button_press", "analyze requested");
        showLoading(APP_CONFIG.PROGRESS_STAGES[0][0]);

        // Agent loops take longer than a fixed pipeline: rotate honest
        // progress hints and show elapsed time so the wait feels alive.
        var labelTimer = setInterval(function () {
            var elapsed = (Date.now() - t0) / 1000;
            var label = APP_CONFIG.PROGRESS_STAGES[0][0];
            APP_CONFIG.PROGRESS_STAGES.forEach(function (stage) {
                if (elapsed >= stage[1]) { label = stage[0]; }
            });
            els.loadingText.textContent =
                label + "  (" + Math.floor(elapsed) + "s)";
        }, 1000);

        var controller = window.AbortController ? new AbortController() : null;
        var abortTimer = controller && setTimeout(function () {
            controller.abort();
        }, APP_CONFIG.REQUEST_TIMEOUT_MS);

        fetch(APP_CONFIG.GATEWAY_URL, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: "{}",
            signal: controller ? controller.signal : undefined
        }).then(function (resp) {
            log("gateway_response", "HTTP " + resp.status + " after " + (Date.now() - t0) + " ms");
            var timings = resp.headers.get("X-Timings-Ms");
            return resp.json().then(function (data) {
                return { data: data, timings: timings };
            });
        }).then(function (r) {
            log("render", "rendering result");
            if (r.data && r.data.error) {
                showError(r.data.error.code, r.data.error.message);
            } else if (r.data && r.data.title !== undefined) {
                showResult(r.data, r.timings);
            } else {
                showError("BAD_RESPONSE", "Gateway returned an unexpected payload.");
            }
        }).catch(function (err) {
            log("error", String(err));
            var aborted = err && (err.name === "AbortError");
            showError(
                aborted ? "TIMEOUT" : "NETWORK",
                aborted
                    ? "The analysis took longer than " + APP_CONFIG.REQUEST_TIMEOUT_MS / 1000 + "s."
                    : "Could not reach the ZeroClaw gateway. Is the sidecar running? (" + err + ")"
            );
        }).then(function () {
            clearInterval(labelTimer);
            if (abortTimer) { clearTimeout(abortTimer); }
            busy = false;
            els.analyzeBtn.disabled = false;
            log("done", "total " + (Date.now() - t0) + " ms");
        });
    }

    /* ---------- input handling ---------- */

    els.analyzeBtn.addEventListener("click", analyze);
    els.closeBtn.addEventListener("click", hideOverlay);

    document.addEventListener("keydown", function (e) {
        var overlayOpen = !els.overlay.classList.contains("hidden");
        switch (e.keyCode) {
            case KEY.ENTER:
                // ENTER activates whatever is focused; if focus got lost,
                // route it to the sensible default for the current state.
                if (document.activeElement !== els.analyzeBtn &&
                    document.activeElement !== els.closeBtn) {
                    if (overlayOpen && !els.closeBtn.classList.contains("hidden")) {
                        hideOverlay();
                    } else if (!overlayOpen) {
                        analyze();
                    }
                    e.preventDefault();
                }
                break;
            case KEY.RETURN: // Samsung remote "back"
                if (overlayOpen) { hideOverlay(); e.preventDefault(); }
                break;
            case KEY.LEFT:
            case KEY.RIGHT:
            case KEY.UP:
            case KEY.DOWN:
                // Single-button-per-state UI: arrows just restore focus.
                (overlayOpen && !els.closeBtn.classList.contains("hidden")
                    ? els.closeBtn : els.analyzeBtn).focus();
                e.preventDefault();
                break;
        }
    });

    els.analyzeBtn.focus();
    log("app_start", "ready, gateway=" + APP_CONFIG.GATEWAY_URL);
})();
