# Evals

A deliberately small, in-repo harness for sanity-checking `jucode --headless`
end to end: each task is a prompt plus a `check.sh` that inspects the files
the agent produced in a throwaway work directory. It is a smoke suite, not a
benchmark.

## Layout

```
evals/
  run.sh              # runner: one temp dir per task, score with check.sh
  tasks/<name>/
    prompt.txt        # the user prompt handed to the agent
    fixture/          # optional: files copied into the work dir first
    check.sh          # exits 0 (pass) / non-zero (fail); runs in the work dir
```

## Running

```sh
cargo build --release
./evals/run.sh              # all tasks
./evals/run.sh create-file  # a single task
```

Requirements: a configured provider/API key (the same setup `jucode` uses
interactively). The runner invokes `jucode --headless --approval-mode
full-auto` so file writes are unattended; every task runs in a fresh temp
directory, never in your repo. Failed work directories are kept for
inspection (the agent's JSONL event log is at `.eval-agent.jsonl` inside),
passing ones are deleted.

Environment knobs:

- `AGENT_CMD` — full command to run instead of the local jucode build.
- `TIMEOUT_SECS` — per-task timeout (default 300).

## Comparing with Codex (or any other CLI agent)

The tasks are agent-agnostic: anything that takes a prompt and edits the
current directory can be scored with the same checks. For example:

```sh
AGENT_CMD='codex exec --full-auto' ./evals/run.sh
```

Run both agents, compare pass counts (and wall-clock/token cost from
jucode's `final_result` line in the log). Keep tasks small and deterministic
so a failure means "the agent could not do the thing", not "the check was
flaky".
