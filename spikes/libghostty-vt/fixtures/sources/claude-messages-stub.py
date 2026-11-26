#!/usr/bin/env python3
"""Loopback Anthropic Messages endpoint for recording a real Claude Code TUI.

usage: python3 -I claude-messages-stub.py PORT_FILE

Binds an ephemeral port on 127.0.0.1, writes the port number to PORT_FILE, and
answers every POST /v1/messages with one fixed text reply, as a server-sent
event stream when the request asks for one and as a JSON message otherwise.
`/v1/messages/count_tokens` gets a fixed count. No credential is read or
checked, and nothing is sent anywhere but back to the caller.
"""

import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MODEL_ID = "claude-sonnet-4-5"
REPLY = "Hello from the loopback stub.\n\nThis reply has two paragraphs."


def message(stop_reason):
    return {
        "id": "msg_stub",
        "type": "message",
        "role": "assistant",
        "model": MODEL_ID,
        "content": [] if stop_reason is None else [{"type": "text", "text": REPLY}],
        "stop_reason": stop_reason,
        "stop_sequence": None,
        "usage": {"input_tokens": 1, "output_tokens": 1},
    }


def event(name, payload):
    return f"event: {name}\ndata: {json.dumps(payload)}\n\n".encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length)
        path = self.path.split("?")[0]
        if path == "/v1/messages/count_tokens":
            return self.send_json({"input_tokens": 1})
        if path != "/v1/messages":
            self.send_error(404)
            return
        try:
            wants_stream = json.loads(body).get("stream") is True
        except ValueError:
            wants_stream = False
        if not wants_stream:
            return self.send_json(message("end_turn"))
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        events = [
            ("message_start", {"type": "message_start", "message": message(None)}),
            ("content_block_start", {"type": "content_block_start", "index": 0,
                                     "content_block": {"type": "text", "text": ""}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                     "delta": {"type": "text_delta", "text": REPLY}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta",
                               "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                               "usage": {"output_tokens": 1}}),
            ("message_stop", {"type": "message_stop"}),
        ]
        for name, payload in events:
            self.wfile.write(event(name, payload))
            self.wfile.flush()
        self.close_connection = True

    def send_json(self, document):
        data = json.dumps(document).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    with open(sys.argv[1], "w", encoding="ascii") as handle:
        handle.write(str(server.server_address[1]))
    server.serve_forever()


if __name__ == "__main__":
    main()
