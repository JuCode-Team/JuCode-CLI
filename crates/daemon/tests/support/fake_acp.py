#!/usr/bin/env python3
"""A stand-in ACP agent for the daemon's engine tests. Prints its arguments
and $FAKE_ACP_GREETING into the first reply. Keeps its conversation in
$FAKE_ACP_STATE (when set) and replays it on session/load."""
import json, os, sys

STATE = os.environ.get("FAKE_ACP_STATE")
CONFIG = [{"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "fake/one",
           "options": [{"value": "fake/one", "name": "One"}, {"value": "fake/two", "name": "Two"}]}]

def out(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

def update(update):
    out({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "agent-1", "update": update}})

def history():
    if STATE and os.path.exists(STATE):
        with open(STATE) as f:
            return json.load(f)
    return []

def remember(user, agent):
    if STATE:
        turns = history() + [[user, agent]]
        with open(STATE, "w") as f:
            json.dump(turns, f)

while True:
    msg = read()
    if msg is None:
        break
    method = msg.get("method")
    if method == "initialize":
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": True},
                                                          "agentInfo": {"name": "fake", "version": "0.1.0"}}})
    elif method == "session/new":
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"sessionId": "agent-1", "configOptions": CONFIG}})
    elif method == "session/load":
        for user, agent in history():
            update({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": user}})
            update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": agent}})
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"configOptions": CONFIG}})
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
            update({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "ls", "kind": "execute", "status": status})
        reply = ("ok: %s %s %s" % (text, " ".join(sys.argv[1:]), os.environ.get("FAKE_ACP_GREETING", ""))).strip()
        update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": reply}})
        update({"sessionUpdate": "usage_update", "used": 100 * (len(history()) + 1), "size": 1000})
        remember(text, reply)
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"stopReason": "end_turn"}})
