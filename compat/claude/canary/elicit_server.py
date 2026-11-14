#!/usr/bin/env python3
"""The MCP server of the Claude canary's `mcp_elicitation` scenario.

A stdio JSON-RPC 2.0 server (one JSON message per line, as the MCP stdio
transport specifies) with one tool, `ask`. Calling `ask` sends the client an
`elicitation/create` request with a one-field form and answers the tool call
with the action the client returned. The capture never answers the form: it
reads the `blocked` record Claude emits while the form is open and stops the
session.

Standard library only. The protocol shape follows the MCP specification
revisions listed in SUPPORTED_VERSIONS; it is verified against Claude at the
canary's first real run (docs/runbooks/claude-canary.md).
"""

import json
import sys

# MCP revisions whose elicitation request this server sends, newest first.
# A client asking for another revision gets the newest one, as the
# specification's version negotiation prescribes.
SUPPORTED_VERSIONS = ("2025-11-25", "2025-06-18")

SERVER_INFO = {"name": "canary", "version": "1.0.0"}

TOOL = {
    "name": "ask",
    "description": "Ask the user one question through an MCP elicitation form.",
    "inputSchema": {"type": "object", "properties": {}},
}

ELICITATION_ID = "canary-elicitation"

ELICITATION_PARAMS = {
    "message": "Canary question: type any answer.",
    "requestedSchema": {
        "type": "object",
        "properties": {"answer": {"type": "string", "title": "Answer"}},
        "required": ["answer"],
    },
}

# JSON-RPC error code of an unknown method.
METHOD_NOT_FOUND = -32601


def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def result(request_id, value):
    send({"jsonrpc": "2.0", "id": request_id, "result": value})


def tool_text(request_id, text, is_error):
    result(
        request_id,
        {"content": [{"type": "text", "text": text}], "isError": is_error},
    )


def main():
    client_elicits = False
    # The tools/call request waiting for the elicitation answer, if any.
    pending_call = None
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if not isinstance(message, dict):
            continue
        method = message.get("method")
        request_id = message.get("id")
        if method is None:
            # A response: only the elicitation request expects one.
            if request_id == ELICITATION_ID and pending_call is not None:
                answer = message.get("result") or {}
                action = answer.get("action", "error")
                tool_text(pending_call, "elicitation " + str(action), False)
                pending_call = None
            continue
        if method == "initialize":
            params = message.get("params") or {}
            requested = params.get("protocolVersion")
            version = requested if requested in SUPPORTED_VERSIONS else SUPPORTED_VERSIONS[0]
            capabilities = params.get("capabilities") or {}
            client_elicits = "elicitation" in capabilities
            result(
                request_id,
                {
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": SERVER_INFO,
                },
            )
        elif method == "tools/list":
            result(request_id, {"tools": [TOOL]})
        elif method == "tools/call":
            name = (message.get("params") or {}).get("name")
            if name != TOOL["name"]:
                tool_text(request_id, "unknown tool " + str(name), True)
            elif not client_elicits:
                # The specification forbids an elicitation request to a
                # client that did not declare the capability.
                tool_text(request_id, "the client declares no elicitation capability", True)
            else:
                pending_call = request_id
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": ELICITATION_ID,
                        "method": "elicitation/create",
                        "params": ELICITATION_PARAMS,
                    }
                )
        elif method == "ping":
            result(request_id, {})
        elif request_id is not None:
            send(
                {
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": METHOD_NOT_FOUND, "message": "method not found"},
                }
            )


if __name__ == "__main__":
    main()
