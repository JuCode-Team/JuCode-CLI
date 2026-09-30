#!/usr/bin/env python3
"""A stand-in ACP agent for the daemon's engine tests. Prints its arguments
and $FAKE_ACP_GREETING into the first reply."""
import json, os, sys

def out(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

while True:
    msg = read()
    if msg is None:
        break
    method = msg.get("method")
    if method == "initialize":
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif method == "session/new":
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"sessionId": "agent-1"}})
    elif method == "session/prompt":
        text = msg["params"]["prompt"][0]["text"]
        if "tool" in text:
            out({"jsonrpc": "2.0", "id": 50, "method": "session/request_permission", "params": {"toolCall": {"title": "Run ls", "kind": "execute"}, "options": [{"optionId": "ok", "kind": "allow_once"}, {"optionId": "no", "kind": "reject_once"}]}})
            while True:
                answer = read()
                if answer is None:
                    sys.exit(0)
                if answer.get("id") == 50:
                    break
            status = "completed" if answer["result"]["outcome"].get("optionId") == "ok" else "failed"
            out({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "agent-1", "update": {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "ls", "kind": "execute", "status": status}}})
        reply = "ok: %s %s %s" % (text, " ".join(sys.argv[1:]), os.environ.get("FAKE_ACP_GREETING", ""))
        out({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "agent-1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": reply.strip()}}}})
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"stopReason": "end_turn"}})
