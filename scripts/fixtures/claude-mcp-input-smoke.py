#!/usr/bin/env python3
"""Minimal local stdio MCP fixture for the real Claude elicitation smoke."""

import json
from pathlib import Path
import sys


def send(message):
    print(json.dumps(message), flush=True)


for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": request["params"]["protocolVersion"],
            "capabilities": {"tools": {}}, "serverInfo": {"name": "wmcterm-input-smoke", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "ask", "description": "Ask for local test configuration",
            "inputSchema": {"type": "object", "properties": {}}}]}
    elif method == "tools/call":
        send({"jsonrpc": "2.0", "id": "local-elicitation", "method": "elicitation/create", "params": {
            "mode": "form", "message": "Configure the local fixture", "requestedSchema": {
                "type": "object", "required": ["count", "enabled", "label"], "properties": {
                    "count": {"type": "integer", "title": "Count"},
                    "enabled": {"type": "boolean", "title": "Enabled"},
                    "label": {"type": "string", "title": "Label"},
                }}}})
        for response_line in sys.stdin:
            response = json.loads(response_line)
            if response.get("id") == "local-elicitation":
                Path(sys.argv[1]).write_text(json.dumps(response))
                result = {"content": [{"type": "text", "text": json.dumps(response)}]}
                break
        else:
            raise SystemExit("Claude closed MCP before answering elicitation")
    elif method == "ping":
        result = {}
    elif method is None or method.startswith("notifications/"):
        continue
    else:
        send({"jsonrpc": "2.0", "id": request.get("id"), "error": {"code": -32601, "message": "Not implemented"}})
        continue
    send({"jsonrpc": "2.0", "id": request["id"], "result": result})
