#!/usr/bin/env python3
"""Mock of the Qwen3-VL vLLM endpoint. The `analyze_image` MCP tool calls
this with a base64 image; it returns a canned prose description of a TV
screen (the VLM in this architecture describes — the agent LLM structures).

GET /_calls reports whether requests actually carried an image payload so
tests can assert the tool sent one.
"""

import argparse
import json
import time
import uuid

import uvicorn
from fastapi import FastAPI, Request

CANNED_DESCRIPTION = (
    "The screen shows a streaming app's browse view. A grid of twelve movie "
    "poster thumbnails fills most of the screen, arranged in two rows. The "
    "poster in the second column of the top row is enlarged and highlighted, "
    "showing the title 'Orbital Dawn' with a 2025 release badge. A navigation "
    "bar on the left edge lists Home, Search, Movies, and Settings icons. In "
    "the top right corner a user avatar and the current time 21:42 are shown."
)

app = FastAPI()
state = {"calls": []}


@app.post("/v1/chat/completions")
async def chat_completions(request: Request):
    body = await request.json()
    flat = json.dumps(body.get("messages", []))
    has_image = '"image_url"' in flat
    state["calls"].append({"t": time.time(), "has_image": has_image})
    print(f"[mock-vlm] {time.strftime('%H:%M:%S')} image={has_image}")
    return {
        "id": f"chatcmpl-{uuid.uuid4().hex[:12]}",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": body.get("model", "mock-vlm"),
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": CANNED_DESCRIPTION},
                "finish_reason": "stop",
            }
        ],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    }


@app.get("/_calls")
async def get_calls():
    return state


@app.get("/health")
async def health():
    return {"status": "ok"}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8008)
    parser.add_argument("--host", default="127.0.0.1")
    args = parser.parse_args()
    uvicorn.run(app, host=args.host, port=args.port, log_level="warning")
