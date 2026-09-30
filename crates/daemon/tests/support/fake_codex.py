#!/usr/bin/env python3
"""A stand-in for `codex app-server`: enough JSON-RPC for the daemon's engine
tests. Starts are logged to $FAKE_CODEX_LOG; threads are saved as rollouts
under ~/.codex/sessions so they can be listed and resumed."""
import json, os, sys, uuid

with open(os.environ["FAKE_CODEX_LOG"], "a") as log:
    log.write(json.dumps(sys.argv[1:]) + "\n")
day = os.path.join(os.environ["HOME"], ".codex", "sessions", "2026", "09", "30")
os.makedirs(day, exist_ok=True)
cwd = os.getcwd()
thread = None
turns = []

def out(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

def rollout(tid):
    return os.path.join(day, "rollout-2026-09-30T00-00-00-%s.jsonl" % tid)

def save():
    with open(rollout(thread), "w") as f:
        f.write(json.dumps({"type": "session_meta", "payload": {"id": thread, "cwd": cwd}}) + "\n")
        for turn in turns:
            f.write(json.dumps({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": turn[0]["content"][0]["text"]}]}}) + "\n")
            f.write(json.dumps({"type": "turn", "items": turn}) + "\n")

def opened(req):
    out({"id": req["id"], "result": {"thread": {"id": thread, "turns": [{"items": t} for t in turns]}, "model": "gpt-5", "modelProvider": "openai", "cwd": cwd}})

while True:
    msg = read()
    if msg is None:
        break
    method = msg.get("method")
    if method == "initialize":
        out({"id": msg["id"], "result": {"userAgent": "fake"}})
    elif method == "thread/start":
        thread = "th-" + uuid.uuid4().hex[:8]
        opened(msg)
    elif method == "thread/resume":
        thread = msg["params"]["threadId"]
        with open(rollout(thread)) as f:
            turns = [json.loads(l)["items"] for l in f if json.loads(l)["type"] == "turn"]
        opened(msg)
    elif method == "model/list":
        out({"id": msg["id"], "result": {"data": [{"model": "gpt-5", "supportedReasoningEfforts": [{"reasoningEffort": "low"}]}]}})
    elif method == "turn/start":
        text = msg["params"]["input"][0]["text"]
        out({"id": msg["id"], "result": {"turn": {"id": "t1"}}})
        out({"method": "turn/started", "params": {"turn": {"id": "t1"}}})
        items = [{"type": "userMessage", "content": [{"type": "text", "text": text}]}]
        if "tool" in text:
            out({"method": "item/started", "params": {"item": {"id": "c1", "type": "commandExecution", "command": "echo hi"}}})
            out({"id": 900, "method": "item/commandExecution/requestApproval", "params": {"itemId": "c1"}})
            while True:
                answer = read()
                if answer is None:
                    sys.exit(0)
                if answer.get("id") == 900:
                    break
            ok = answer["result"]["decision"].startswith("accept")
            command = {"id": "c1", "type": "commandExecution", "command": "echo hi", "aggregatedOutput": "hi\n" if ok else "", "exitCode": 0, "status": "completed" if ok else "declined"}
            out({"method": "item/completed", "params": {"item": command}})
            items.append(command)
        reply = "ok: " + text
        out({"method": "item/started", "params": {"item": {"id": "m1", "type": "agentMessage"}}})
        out({"method": "item/agentMessage/delta", "params": {"itemId": "m1", "delta": reply}})
        out({"method": "item/completed", "params": {"item": {"id": "m1", "type": "agentMessage", "text": reply}}})
        items.append({"type": "agentMessage", "text": reply})
        turns.append(items)
        save()
        out({"method": "turn/completed", "params": {"turn": {"id": "t1", "status": "completed"}}})
