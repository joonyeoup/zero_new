#!/usr/bin/env python3
"""Mock of the DGX LLM — plays the AGENT role deterministically.

Speaks BOTH tool protocols ZeroClaw may use, chosen per-request from what the
gateway actually sends:

  native (what the `vllm` provider family uses): tool specs arrive in the
  request body's `tools` array; tool calls go back as
  `message.tool_calls = [{id, type: "function", function: {name, arguments}}]`
  and results come back as `role: "tool"` messages tied to the call id.

  xml (ZeroClaw's prompt-embedded fallback dispatcher): tool specs are listed
  in the system prompt; calls are plain text `<tool_call>{...}</tool_call>`
  and results come back inside a user message as
  `<tool_result name="..." status="...">...</tool_result>`.

The mock walks the protocol like a competent model would:

  turn 1: no screenshot result in history      -> call the screenshot tool
  turn 2: screenshot result present            -> call analyze_image(image_path)
  turn 3: analyze_image result present         -> final schema-conformant JSON

Tool names are discovered from the request (native `tools` array or the
system prompt), so nothing is hardcoded to a config.

Modes (?mode= query param, POST /_mode, or MOCK_MODE env; query param wins):
  valid            -> final answer is clean JSON
  malformed_once   -> first final answer is fenced/prose junk; after the
                      retry nudge ("not valid JSON") the corrected JSON is
                      returned (exercises the single-retry path)
  always_malformed -> every final answer is junk (exercises structured error)

GET /_calls returns the ordered decision log so tests can assert the agent
actually chose screenshot before analyze_image.
"""

import argparse
import json
import os
import re
import time
import uuid

import uvicorn
from fastapi import FastAPI, Request

FINAL_ANSWER = {
    "screen_type": "streaming_app",
    "title": "Movie selection screen",
    "summary": (
        "A streaming app browse screen showing a grid of movie posters, "
        "with a sci-fi title currently focused."
    ),
    "detected_elements": [
        {
            "name": "poster_grid",
            "description": "Grid of 12 movie poster thumbnails",
            "confidence": 0.95,
        },
        {
            "name": "focused_title",
            "description": "Highlighted poster: 'Orbital Dawn' (2025)",
            "confidence": 0.88,
        },
    ],
    "suggested_actions": [
        "Press ENTER to open the focused title",
        "Scroll right to see more titles",
    ],
    "error": None,
}

MALFORMED_FINAL = (
    "Here's what I found on your screen!\n\n"
    "```json\n{\"screen_type\": \"streaming_app\", \"title\": \"Movie selection\", "
    "\"confidence\": \"very high\"}\n```\n\nHope that helps!"
)

RETRY_NUDGE_HINT = "not valid json"

app = FastAPI()
state = {"mode": os.environ.get("MOCK_MODE", "valid"), "calls": []}


def content_text(c) -> str:
    """Flatten OpenAI message content (string or content-part list) to text."""
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        return "\n".join(
            p.get("text", "") if isinstance(p, dict) else str(p) for p in c
        )
    return json.dumps(c)


def flatten(messages, role=None) -> str:
    return "\n".join(
        content_text(m.get("content", ""))
        for m in messages
        if role is None or m.get("role") == role
    )


def match_suffix(names, suffix: str) -> str:
    """Pick the advertised tool whose name ends with `suffix`. Prefers the
    MCP-prefixed form (`<server>__<tool>`) over a same-named built-in, the way
    the system prompt (SOUL.md) directs the agent to the TV tools."""
    for n in names:
        if n.endswith(f"__{suffix}"):
            return n
    for n in names:
        if n == suffix or n.endswith(suffix):
            return n
    return suffix


def native_tool_names(body) -> list:
    return [
        t.get("function", {}).get("name", "")
        for t in body.get("tools") or []
        if isinstance(t, dict)
    ]


def tool_results(messages) -> list:
    """Return [(tool_name, result_text)] in order, across both protocols."""
    results = []
    # Native: assistant tool_calls map ids -> names; role "tool" carries results.
    id_to_name = {}
    for m in messages:
        for tc in m.get("tool_calls") or []:
            id_to_name[tc.get("id", "")] = tc.get("function", {}).get("name", "")
        if m.get("role") == "tool":
            name = m.get("name") or id_to_name.get(m.get("tool_call_id", ""), "")
            results.append((name, content_text(m.get("content", ""))))
    # XML fallback: <tool_result name="..." ...>body</tool_result> in user text.
    for match in re.finditer(
        r'<tool_result name="([^"]+)"[^>]*>(.*?)</tool_result>',
        flatten(messages, "user"),
        re.DOTALL,
    ):
        results.append((match.group(1), match.group(2)))
    return results


def find_image_path(results) -> str:
    for _, text in results:
        # Tolerate JSON-escaped quotes: results may arrive wrapped in an MCP
        # envelope where the inner JSON is a string ({\"image_path\":\"...\"}).
        m = re.search(r'image_path\\?"?\s*:\s*\\?"?([^"\\]+)', text)
        if m:
            return m.group(1).strip()
        m = re.search(r"\[IMAGE:([^\]]+)\]", text)  # canonicalized media marker
        if m:
            return m.group(1).strip()
    return None  # runtime masked the path ("[media attachment]")


def decide(body) -> tuple:
    """Return (decision_label, tool_call_or_None, final_text_or_None)."""
    mode = state["request_mode"]
    messages = body.get("messages", [])
    names = native_tool_names(body)
    if not names:  # xml protocol: discover names from the system prompt
        names = re.findall(r"[A-Za-z0-9_-]*(?:screenshot|analyze_image)",
                           flatten(messages, "system"))
    shot_tool = match_suffix(names, "screenshot")
    analyze_tool = match_suffix(names, "analyze_image")

    results = tool_results(messages)
    done = {name for name, _ in results}
    retry_nudged = RETRY_NUDGE_HINT in flatten(messages, "user").lower()

    if shot_tool not in done:
        return "tool_call:screenshot", (shot_tool, {}), None
    if analyze_tool not in done:
        path = find_image_path(results)
        # A masked path means the runtime hid it; the tool's own fallback
        # (most recent screenshot) covers the no-argument call.
        args = {"image_path": path} if path else {}
        return "tool_call:analyze_image", (analyze_tool, args), None

    if mode == "always_malformed":
        return "final:malformed", None, MALFORMED_FINAL
    if mode == "malformed_once" and not retry_nudged:
        return "final:malformed", None, MALFORMED_FINAL
    if retry_nudged:
        return "final:corrected", None, json.dumps(FINAL_ANSWER)
    return "final:valid", None, json.dumps(FINAL_ANSWER)


@app.post("/v1/chat/completions")
async def chat_completions(request: Request):
    body = await request.json()
    state["request_mode"] = request.query_params.get("mode", state["mode"])
    native = bool(body.get("tools"))
    decision, call, final = decide(body)
    state["calls"].append(
        {
            "t": time.time(),
            "decision": decision,
            "mode": state["request_mode"],
            "native": native,
            "n_messages": len(body.get("messages", [])),
        }
    )
    state["last_system"] = flatten(body.get("messages", []), "system")
    print(
        f"[mock-llm] {time.strftime('%H:%M:%S')} mode={state['request_mode']} "
        f"protocol={'native' if native else 'xml'} -> {decision}"
    )

    if call and native:
        name, args = call
        message = {
            "role": "assistant",
            "content": None,
            "tool_calls": [
                {
                    "id": f"call_{uuid.uuid4().hex[:8]}",
                    "type": "function",
                    "function": {"name": name, "arguments": json.dumps(args)},
                }
            ],
        }
        finish = "tool_calls"
    elif call:  # xml protocol: the tool call is plain text
        name, args = call
        message = {
            "role": "assistant",
            "content": "<tool_call>"
            + json.dumps({"name": name, "arguments": args})
            + "</tool_call>",
        }
        finish = "stop"
    else:
        message = {"role": "assistant", "content": final}
        finish = "stop"

    return {
        "id": f"chatcmpl-{uuid.uuid4().hex[:12]}",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": body.get("model", "mock-agent"),
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    }


@app.post("/_mode")
async def set_mode(request: Request):
    body = await request.json()
    state["mode"] = body.get("mode", "valid")
    state["calls"] = []
    return {"mode": state["mode"]}


@app.get("/_calls")
async def get_calls():
    return {"mode": state["mode"], "calls": state["calls"]}


@app.get("/_last_system")
async def get_last_system():
    return {"system": state.get("last_system", "")}


@app.get("/health")
async def health():
    return {"status": "ok"}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8010)
    parser.add_argument("--host", default="127.0.0.1")
    args = parser.parse_args()
    uvicorn.run(app, host=args.host, port=args.port, log_level="warning")
