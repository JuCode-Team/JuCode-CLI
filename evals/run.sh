#!/usr/bin/env bash
# Minimal eval harness: runs each task under evals/tasks/ against
# `jucode --headless` in a throwaway work directory and scores the result
# with the task's check.sh. See evals/README.md.
set -u

EVALS_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$EVALS_DIR/.." && pwd)"

# Agent under test. Override to compare other agents, e.g.:
#   AGENT_CMD='codex exec --full-auto' ./evals/run.sh
if [ -z "${AGENT_CMD:-}" ]; then
    if [ -x "$REPO_ROOT/target/release/jucode" ]; then
        AGENT_CMD="$REPO_ROOT/target/release/jucode --headless --approval-mode full-auto"
    elif [ -x "$REPO_ROOT/target/debug/jucode" ]; then
        AGENT_CMD="$REPO_ROOT/target/debug/jucode --headless --approval-mode full-auto"
    else
        AGENT_CMD="jucode --headless --approval-mode full-auto"
    fi
fi
TIMEOUT_SECS="${TIMEOUT_SECS:-300}"

only_task="${1:-}"
pass=0
fail=0
results=""

for task_dir in "$EVALS_DIR"/tasks/*/; do
    task="$(basename "$task_dir")"
    if [ -n "$only_task" ] && [ "$task" != "$only_task" ]; then
        continue
    fi
    workdir="$(mktemp -d "${TMPDIR:-/tmp}/jucode-eval-$task-XXXXXX")"
    if [ -d "$task_dir/fixture" ]; then
        cp -R "$task_dir/fixture/." "$workdir/"
    fi
    prompt="$(cat "$task_dir/prompt.txt")"
    log="$workdir/.eval-agent.jsonl"

    echo "== $task (workdir: $workdir)"
    # shellcheck disable=SC2086 — AGENT_CMD is intentionally word-split.
    (cd "$workdir" && timeout "$TIMEOUT_SECS" $AGENT_CMD "$prompt" >"$log" 2>&1)
    agent_status=$?
    if [ "$agent_status" -eq 124 ]; then
        echo "   agent timed out after ${TIMEOUT_SECS}s"
    fi

    if (cd "$workdir" && bash "$task_dir/check.sh"); then
        echo "   PASS"
        results="$results\n  PASS $task"
        pass=$((pass + 1))
        rm -rf "$workdir"
    else
        echo "   FAIL (agent exit $agent_status; inspect $workdir, log: $log)"
        results="$results\n  FAIL $task"
        fail=$((fail + 1))
    fi
done

echo
echo "results:"
printf '%b\n' "$results"
echo "passed $pass, failed $fail"
[ "$fail" -eq 0 ]
