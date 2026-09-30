#!/usr/bin/env python3
"""A stand-in for `claude --print --input-format stream-json ...`: enough of
the stream-json protocol for the daemon's engine tests. Every start appends
its arguments to $FAKE_CLAUDE_LOG; turns are saved like Claude Code saves
them, under ~/.claude/projects/<munged cwd>/<session id>.jsonl."""
import json, os, re, sys

args = sys.argv[1:]
with open(os.environ["FAKE_CLAUDE_LOG"], "a") as log:
    log.write(json.dumps(args) + "\n")

def arg(flag):
    return args[args.index(flag) + 1] if flag in args else None

session = arg("--resume") or arg("--session-id")
mode = "bypassPermissions" if "--dangerously-skip-permissions" in args else (arg("--permission-mode") or "default")
cwd = os.getcwd()
saved = os.path.join(os.environ["HOME"], ".claude", "projects", re.sub(r"[^A-Za-z0-9]", "-", cwd))
os.makedirs(saved, exist_ok=True)

def out(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()

def save(role, text):
    with open(os.path.join(saved, session + ".jsonl"), "a") as f:
        f.write(json.dumps({"type": role, "message": {"content": text}}) + "\n")

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

while True:
    frame = read()
    if frame is None:
        break
    if frame["type"] == "control_request":
        request = frame["request"]
        response = {}
        if request["subtype"] == "set_permission_mode":
            mode = request["mode"]
            response = {"mode": mode}
        elif request["subtype"] == "list_models":
            response = {"models": [{"value": "sonnet", "resolvedModel": "claude-sonnet-4-5", "displayName": "Sonnet"}]}
        out({"type": "control_response", "response": {"subtype": "success", "request_id": frame["request_id"], "response": response}})
        continue
    if frame["type"] != "user":
        continue
    text = frame["message"]["content"][0]["text"]
    out({"type": "user", "isReplay": True, "message": frame["message"]})
    out({"type": "system", "subtype": "init", "session_id": session, "model": "claude-sonnet-4-5", "permissionMode": mode, "cwd": cwd, "slash_commands": ["compact"]})
    out({"type": "system", "subtype": "status", "status": "requesting"})
    save("user", text)
    if "tool" in text:
        out({"type": "control_request", "request_id": "perm-1", "request": {"subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "tool-1", "input": {"command": "echo hi"}}})
        while True:
            answer = read()
            if answer is None:
                sys.exit(0)
            if answer["type"] == "control_response":
                break
        allowed = answer["response"]["response"]["behavior"] == "allow"
        out({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "tool-1", "content": "hi" if allowed else "denied", "is_error": not allowed}]}, "tool_use_result": {"stdout": "hi"} if allowed else {}})
    reply = "ok: " + text
    out({"type": "stream_event", "event": {"type": "message_start", "message": {"usage": {"input_tokens": 5}}}})
    out({"type": "assistant", "uuid": "u-1", "message": {"content": [{"type": "text", "text": reply}]}})
    save("assistant", reply)
    out({"type": "result", "subtype": "success", "total_cost_usd": 0.01})
