# Captain FSM Authoring Guide

A project factory is a finite-state machine of agent and Python states in
`<project>/.prime/machines/<name>/`. This guide is complete on its own: a
Captain with no Python REPL (antigravity, cursor, claude-cli) can author from
it. `rlm.factory.help()` and `prime-agent factory validate <dir>` remain the
source of truth; the loader is `crates/pa-factory/src/load.rs`.

## 1. FSM or crew thread?

Use an FSM when the work is **repeatable and has shape**: multiple stages,
verification gates (build, tests, review), fan-out and join, routing on a
verdict, or a bounded rework loop. Use a **crew thread** (`delegate` with
`provider`/`model`) for one-off, open-ended work: an investigation, a spike,
a question whose next step depends on judgment. If you would write the same
three delegate calls twice, write a machine.

## 2. `machine.toml` v1 reference

### Layout

```
.prime/machines/<name>/          # name: lowercase, ^[a-z0-9][a-z0-9-]{0,63}$, not a library name
  machine.toml                   # the graph (required)
  prompts/*.md                   # agent prompts; prompts/_context.md is prepended to every agent state
  nodes/*.py                     # Python states
  rubrics/*.toml                 # optional: [[rule]] id, check, default
  assets/<path>                  # optional: files an input reads as text ("assets/spec.md")
  modules/<alias>/               # copied by factory-add-module; never edit by hand
  modules.lock                   # module versions, written by factory-add-module
  captain.toml                   # ignored for project factories (they always count as writers)
```

Every table rejects unknown keys. Top level: `format = 1`, `name`,
`version` (int), `description`, optional `author`, then the tables below.

### `[run]` (all optional)

| key | value |
|---|---|
| `budget` | duration string (`"90s"`, `"30m"`, `"1h30m"`): pauses the run once when exceeded |
| `max_parallel` | 1..64 (default 8): instances running at once, run-wide |
| `max_children` | 1..1,000,000 (default 10,000): total admissions incl. foreach and retries; pauses once |
| `max_transitions` | 1..10,000 (default 10 per state): pauses once at the boundary |
| `failure_policy` | `fail_fast`, `continue`, or `escalate` (default) for every state |
| `repair_wait_ms` | ≥1: how long a child that owes a required output may take to supply it |
| `budget_tokens` | ≥1: token cap for the run |
| `budget_usd` | positive float: spend cap for the run |

### `[params]`

```toml
[params]
task = { type = "text" }                                   # required: no default
validate_command = { type = "text", default = "cargo test", description = "Checks to run" }
```

Prompts insert params with `{{task}}`; Python states receive them in the
inputs dict. `delegate` passes `params` (must match); `task` fills the first
required text param.

### `[roles]` and `[types]`

```toml
[roles]
fast   = { model = "antigravity/gemini-4-neon", thinking = "low" }
coder  = { model = "openai-codex/gpt-5.3-codex", thinking = "high" }
judge  = { model = "anthropic/claude-opus-5-5", thinking = "max" }

[types.verdict]                    # a record type, used as an output type by name
decision = "enum(approve, rework)"
findings = "list[text]"
```

`thinking`: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`. Put
different model families on reviewers so they do not share blind spots.
Models come **only** from the Configured models list (section 3).

### Port types

`text`, `int`, `float`, `bool`, `json`, `file`, `enum(a, b)` (lowercase
words, no duplicates), `list[T]` (square brackets; items cannot be
optional), or a `[types.x]` record name. Append `?` for an optional
output (`"text?"`); an input reading an optional output must be optional too.

### `[states.<id>]`

| key | meaning |
|---|---|
| `entry = true` | an entry state (at least one). All its inputs must be optional (`?`) |
| `agent = { prompt, role?, model?, thinking?, name?, rubric? }` | an agent state |
| `kind = "python"` + `run = "nodes/<file>.py:<fn>"` | a Python state (no `agent`) |
| `inputs = { port = "<state>.<output>" }` | bind upstream outputs; suffix `?` for optional (`"review.verdict?"`); `"assets/<path>"` reads a file |
| `outputs = { port = "<type>" }` | typed outputs the state must produce |
| `next = "s"` or `next = ["a", "b"]` | unconditional successor; a list fans out in parallel |
| `route = { on = "<output>[.<field>]", <value> = "<state>", ... }` | switch on one enum or bool output; every value needs an arm (bool arms: `true`, `false`) |
| `join = ["a", "b"]` | wait for every member; each member names this state in its `next` |
| `foreach = { over = "<input>", max = 1..256 }` | one child per list item of that input (agent states only) |
| `max_entries` | default **1**: any state a loop re-enters needs more |
| `retries` | 0..10, same rendered prompt |
| `budget` | duration from admission to settle; exceeding fails without retry |
| `failure_policy` | overrides `[run].failure_policy` |
| `resident = true` | stays alive after the run; no outputs, next, route, or retries |

Rules the validator enforces: **a `route` arm is one state name, never a
list.** To route into a fan-out, route to a small pass state whose `next`
is the list. A required input cannot read its own state (make a self-input
optional). Any condition other than one enum/bool switch is a Python state
that returns an enum. Prompts use `{{name}}` for inputs and params; the
loader appends an Outputs section with each port's type, so prompts do not
restate the output format.

### Python states

```python
# nodes/gate.py
def check(inputs):                 # inputs: declared inputs + params, raw values (optional+missing = None)
    passed = inputs["log"].strip().endswith("ok")
    return {"passed": passed, "summary": inputs["log"][-2000:]}   # EXACTLY the declared output keys
```

Each call runs in a fresh isolated subprocess in the run's working directory:
no `rlm`, no agents, prints go to stderr. Return a dict whose keys are
exactly the declared outputs, each matching its type (a state with no
outputs returns `{}`). An exception, a non-dict, or wrong keys fails the
attempt (then `retries`, then `failure_policy`). Keep functions pure (they
re-run on retry and resume), import stdlib at module top, and never commit
`__pycache__/` or reference it. Default time limit: state `budget`, else run
`budget`, else 60 s.

## 3. Multi-provider workflow rules

**Models.** Pick every `[roles]` model from the **Configured models** line in
your Captain context. If it is absent, check `pa model list`,
`pa thread models antigravity`, and `pa thread models cursor`. Never name a
provider the user has not configured; the run fails at spawn.

**Output contract, native vs CLI-backed.** Write prompts that work for both:
state what to produce, not how to bind it.

- Native RLM children (`openai-codex/*`, API-key `anthropic/*`, …) bind
  outputs with `await rlm.factory.emit(port=value, ...)`; a wrong type
  raises `FactoryEmitError` naming the field, fixed in the same turn.
- CLI-backed children (`antigravity/*`, `cursor/*`, `claude-cli/*`) have no
  REPL. The runtime tells them to end with a fenced `factory-outputs` JSON
  block (or write the same object to the per-attempt `factory-outputs.json`
  path it gives). That binds directly; no repair loop.
- Never write "call rlm.factory.emit" or "print a json block" in a prompt;
  the generated Outputs section handles each family.

**Changing models.** Models are policy, not content: `factory-set-model`
with `all: true`, `role`, or `state` (plus `thinking`) writes the project's
model overlay. The factory stays approved, live runs are untouched, new runs
use it. `factory-clear-model-override` reverts to `machine.toml`.

**Structural edits.** Edit any time, even during a live run: each run uses a
frozen copy of its approved files. An edit makes the factory a draft; tell
the user under Needs you. With an explicit user pre-approval,
`factory-preapprove`, edit, `factory-show` (new digest), `factory-approve`
in the same turn.

**Shared modules and specialists.** `factory-modules` lists `validate` (runs
`validate_command`, routes on `passed`), `sanitize-diff` (drops binary hunks
and secret-like lines, caps size), `review-triad` (Opus + Codex + Neon
reviewers → `verdict` approve/rework). `factory-add-module` with `name`,
`module`, `alias`, `wire`, `after` adds them as `<alias>-<state>`. Project
specialists: `specialist-write` then `factory-use-specialist` (`states`).

**Token efficiency.**

- Deterministic Python gates (`validate`, `sanitize-diff`, a diff collector)
  run **before** frontier reviewers, so broken code never costs review tokens.
- Pass compact ports (`summary`, `changed_files`, `findings`) rather than
  logs; trim logs in Python (the validate module keeps error lines + tail).
- Tier models: fast/cheap for triage, routing, formatting; strong coding
  models for implement; cross-family frontier models for review/consensus.
- Bound everything: small `max_entries` on loops, `[run].budget`,
  `budget_tokens`/`budget_usd`. For a human choice use the Captain's
  `escalate` op, not prose.

## 4. Archetypes

Each blueprint is a valid `machine.toml` (prompts and nodes omitted; every
`prompts/*.md` and `nodes/*.py` named must exist). Replace role models with
configured ones.

### 4.1 Delivery pipeline

plan → implement → validate (loop back on fail) → collect diff → sanitize →
three-family review → verdict (loop back on rework) → finalize. In practice
add `validate`, `sanitize-diff`, and `review-triad` with `factory-add-module`;
this inline form shows the wiring.

```toml
format = 1
name = "delivery"
version = 1
description = "Plan, implement, gate, review, finalize."

[params]
task = { type = "text" }
validate_command = { type = "text", default = "cargo test --locked" }

[run]
budget = "2h"
max_parallel = 4
budget_usd = 25.0

[roles]
planner = { model = "anthropic/claude-opus-5-5", thinking = "high" }
coder = { model = "openai-codex/gpt-5.3-codex", thinking = "high" }
rev-a = { model = "anthropic/claude-opus-5-5", thinking = "high" }
rev-b = { model = "openai-codex/gpt-5.3-codex", thinking = "high" }
rev-c = { model = "antigravity/gemini-4-neon", thinking = "high" }

[states.plan]
entry = true
agent = { role = "planner", prompt = "prompts/plan.md" }
outputs = { plan = "text" }
next = "implement"

[states.implement]
agent = { role = "coder", prompt = "prompts/implement.md" }
inputs = { plan = "plan.plan", log = "validate.log?", findings = "verdict.findings?" }
outputs = { summary = "text" }
max_entries = 5
next = "validate"

[states.validate]
kind = "python"
run = "nodes/validate.py:run"
outputs = { passed = "bool", log = "text" }
max_entries = 5
route = { on = "passed", true = "collect-diff", false = "implement" }

[states.collect-diff]
kind = "python"
run = "nodes/diff.py:collect"
outputs = { diff = "text", changed_files = "list[text]" }
max_entries = 4
next = "sanitize"

[states.sanitize]
kind = "python"
run = "nodes/diff.py:sanitize"
inputs = { diff = "collect-diff.diff" }
outputs = { diff = "text" }
max_entries = 4
next = ["review-a", "review-b", "review-c"]

[states.review-a]
agent = { role = "rev-a", prompt = "prompts/review-correctness.md" }
inputs = { diff = "sanitize.diff" }
outputs = { approve = "bool", findings = "text" }
max_entries = 4
next = "verdict"

[states.review-b]
agent = { role = "rev-b", prompt = "prompts/review-bugs.md" }
inputs = { diff = "sanitize.diff" }
outputs = { approve = "bool", findings = "text" }
max_entries = 4
next = "verdict"

[states.review-c]
agent = { role = "rev-c", prompt = "prompts/review-robustness.md" }
inputs = { diff = "sanitize.diff" }
outputs = { approve = "bool", findings = "text" }
max_entries = 4
next = "verdict"

[states.verdict]
kind = "python"
run = "nodes/verdict.py:merge"
join = ["review-a", "review-b", "review-c"]
inputs = { a = "review-a.approve", af = "review-a.findings", b = "review-b.approve", bf = "review-b.findings", c = "review-c.approve", cf = "review-c.findings" }
outputs = { verdict = "enum(approve, rework)", findings = "text" }
max_entries = 4
route = { on = "verdict", approve = "finalize", rework = "implement" }

[states.finalize]
agent = { role = "planner", prompt = "prompts/finalize.md" }
inputs = { plan = "plan.plan", files = "collect-diff.changed_files" }
outputs = { report = "text" }
```

### 4.2 Smart delegation / model router

A cheap model classifies; only hard work reaches a frontier model. Note the
`split` pass state: a route arm cannot be a list.

```toml
format = 1
name = "router"
version = 1
description = "Triage, then route to the cheapest capable tier."

[params]
task = { type = "text" }

[run]
budget_tokens = 2000000

[roles]
fast = { model = "antigravity/gemini-4-neon", thinking = "low" }
deep = { model = "anthropic/claude-opus-5-5", thinking = "max" }
coder = { model = "openai-codex/gpt-5.3-codex", thinking = "high" }

[types.triage]
tier = "enum(fast, deep, multi)"
parts = "list[text]"

[states.triage]
entry = true
agent = { role = "fast", prompt = "prompts/triage.md" }
outputs = { triage = "triage" }
route = { on = "triage.tier", fast = "quick-fix", deep = "deep-solve", multi = "split" }

[states.quick-fix]
agent = { role = "fast", prompt = "prompts/quick-fix.md" }
outputs = { summary = "text" }
next = "validate"

[states.deep-solve]
agent = { role = "deep", prompt = "prompts/deep-solve.md" }
outputs = { summary = "text" }
next = "validate"

[states.split]
kind = "python"
run = "nodes/split.py:parts"
inputs = { triage = "triage.triage" }
outputs = { parts = "list[text]" }
next = "worker"

[states.worker]
agent = { role = "coder", prompt = "prompts/worker.md" }
inputs = { part = "split.parts" }
outputs = { summary = "text" }
foreach = { over = "part", max = 8 }
next = "synthesize"

[states.synthesize]
agent = { role = "fast", prompt = "prompts/synthesize.md" }
inputs = { summaries = "worker.summary" }
outputs = { summary = "text" }
next = "validate"

[states.validate]
kind = "python"
run = "nodes/validate.py:run"
inputs = { quick = "quick-fix.summary?", deep = "deep-solve.summary?", multi = "synthesize.summary?" }
outputs = { passed = "bool", log = "text" }
```

### 4.3 Cross-family consensus / debate

The same question to three families; Python measures agreement; disagreement
goes to a tiebreaker (or the Captain escalates the result to the user).

```toml
format = 1
name = "consensus"
version = 1
description = "Three families answer; agree finalizes, disagree goes to a tiebreaker."

[params]
question = { type = "text" }

[roles]
opus = { model = "anthropic/claude-opus-5-5", thinking = "high" }
codex = { model = "openai-codex/gpt-5.3-codex", thinking = "high" }
neon = { model = "antigravity/gemini-4-neon", thinking = "high" }

[types.vote]
choice = "enum(yes, no)"
reason = "text"

[states.start]
entry = true
kind = "python"
run = "nodes/pass.py:start"
next = ["ask-opus", "ask-codex", "ask-neon"]

[states.ask-opus]
agent = { role = "opus", prompt = "prompts/ask.md" }
outputs = { vote = "vote" }
next = "tally"

[states.ask-codex]
agent = { role = "codex", prompt = "prompts/ask.md", name = "ask-codex" }
outputs = { vote = "vote" }
next = "tally"

[states.ask-neon]
agent = { role = "neon", prompt = "prompts/ask.md", name = "ask-neon" }
outputs = { vote = "vote" }
next = "tally"

[states.tally]
kind = "python"
run = "nodes/tally.py:tally"
join = ["ask-opus", "ask-codex", "ask-neon"]
inputs = { a = "ask-opus.vote", b = "ask-codex.vote", c = "ask-neon.vote" }
outputs = { outcome = "enum(agree, disagree)", summary = "text" }
route = { on = "outcome", agree = "finalize", disagree = "tiebreak" }

[states.tiebreak]
agent = { role = "opus", prompt = "prompts/tiebreak.md", thinking = "max" }
inputs = { summary = "tally.summary" }
outputs = { decision = "text" }
next = "finalize"

[states.finalize]
agent = { role = "neon", prompt = "prompts/finalize.md" }
inputs = { summary = "tally.summary", decision = "tiebreak.decision?" }
outputs = { answer = "text" }
```

`nodes/pass.py`: `def start(inputs): return {}`.

### 4.4 Scientific / experiment verification

```toml
format = 1
name = "experiment-check"
version = 1
description = "Implement, run experiments, audit in parallel, report."

[params]
task = { type = "text" }
experiment_command = { type = "text", default = "python -m experiments.run" }

[run]
budget = "3h"

[roles]
coder = { model = "openai-codex/gpt-5.3-codex", thinking = "high" }
numerics = { model = "anthropic/claude-opus-5-5", thinking = "high" }
leakage = { model = "openai-codex/gpt-5.3-codex", thinking = "high" }
robust = { model = "antigravity/gemini-4-argon", thinking = "high" }

[states.implement]
entry = true
agent = { role = "coder", prompt = "prompts/implement.md" }
inputs = { log = "run-experiments.log?", findings = "verdict.findings?" }
outputs = { summary = "text" }
max_entries = 4
next = "run-experiments"

[states.run-experiments]
kind = "python"
run = "nodes/experiments.py:run"
budget = "1h"
outputs = { passed = "bool", log = "text", metrics = "json" }
max_entries = 4
route = { on = "passed", true = "audits", false = "implement" }

[states.audits]
kind = "python"
run = "nodes/pass.py:start"
max_entries = 4
next = ["audit-numerics", "audit-leakage", "audit-robustness"]

[states.audit-numerics]
agent = { role = "numerics", prompt = "prompts/audit-numerics.md" }
inputs = { metrics = "run-experiments.metrics", summary = "implement.summary" }
outputs = { approve = "bool", findings = "text" }
max_entries = 4
next = "verdict"

[states.audit-leakage]
agent = { role = "leakage", prompt = "prompts/audit-leakage.md" }
inputs = { metrics = "run-experiments.metrics", summary = "implement.summary" }
outputs = { approve = "bool", findings = "text" }
max_entries = 4
next = "verdict"

[states.audit-robustness]
agent = { role = "robust", prompt = "prompts/audit-robustness.md" }
inputs = { metrics = "run-experiments.metrics", summary = "implement.summary" }
outputs = { approve = "bool", findings = "text" }
max_entries = 4
next = "verdict"

[states.verdict]
kind = "python"
run = "nodes/verdict.py:merge"
join = ["audit-numerics", "audit-leakage", "audit-robustness"]
inputs = { a = "audit-numerics.approve", af = "audit-numerics.findings", b = "audit-leakage.approve", bf = "audit-leakage.findings", c = "audit-robustness.approve", cf = "audit-robustness.findings" }
outputs = { verdict = "enum(approve, rework)", findings = "text" }
max_entries = 4
route = { on = "verdict", approve = "report", rework = "implement" }

[states.report]
agent = { role = "numerics", prompt = "prompts/report.md" }
inputs = { metrics = "run-experiments.metrics", findings = "verdict.findings" }
outputs = { report = "text" }
```

### 4.5 Read-only audit / sweep

No writes, so a `captain.toml` with `access = "read"` fits a library copy;
project factories still count as writers.

```toml
format = 1
name = "audit-sweep"
version = 1
description = "Parallel read-only audits merged into one deduplicated findings list."

[params]
scope = { type = "text", default = "the current branch's changes" }

[roles]
security = { model = "anthropic/claude-opus-5-5", thinking = "high" }
perf = { model = "antigravity/gemini-4-neon", thinking = "medium" }
correct = { model = "openai-codex/gpt-5.3-codex", thinking = "high" }

[states.start]
entry = true
kind = "python"
run = "nodes/pass.py:start"
next = ["security", "performance", "correctness"]

[states.security]
agent = { role = "security", prompt = "prompts/security.md" }
outputs = { findings = "json" }
next = "merge"

[states.performance]
agent = { role = "perf", prompt = "prompts/performance.md" }
outputs = { findings = "json" }
next = "merge"

[states.correctness]
agent = { role = "correct", prompt = "prompts/correctness.md" }
outputs = { findings = "json" }
next = "merge"

[states.merge]
kind = "python"
run = "nodes/merge.py:dedupe"
join = ["security", "performance", "correctness"]
inputs = { s = "security.findings", p = "performance.findings", c = "correctness.findings" }
outputs = { findings = "json", summary = "text" }
```

### 4.6 Staged multi-file refactor / migration

```toml
format = 1
name = "staged-refactor"
version = 1
description = "Plan shards, rewrite them in parallel, verify all, review."

[params]
task = { type = "text" }
validate_command = { type = "text", default = "cargo check --workspace" }

[run]
max_parallel = 6

[roles]
planner = { model = "anthropic/claude-opus-5-5", thinking = "high" }
writer = { model = "openai-codex/gpt-5.3-codex", thinking = "medium" }
reviewer = { model = "antigravity/gemini-4-argon", thinking = "high" }

[states.plan-shards]
entry = true
agent = { role = "planner", prompt = "prompts/plan-shards.md" }
inputs = { log = "verify-all.log?" }
outputs = { shards = "list[text]" }
max_entries = 3
next = "write-shard"

[states.write-shard]
agent = { role = "writer", prompt = "prompts/write-shard.md" }
inputs = { shard = "plan-shards.shards" }
outputs = { summary = "text" }
foreach = { over = "shard", max = 16 }
max_entries = 3
next = "verify-all"

[states.verify-all]
kind = "python"
run = "nodes/validate.py:run"
outputs = { passed = "bool", log = "text" }
max_entries = 3
route = { on = "passed", true = "review", false = "plan-shards" }

[states.review]
agent = { role = "reviewer", prompt = "prompts/review.md" }
inputs = { summary = "write-shard.summary" }
outputs = { approve = "bool", findings = "text" }
```

## 5. Validation loop and pitfalls

1. Write files, then `pa thread captain act factory-validate` with `name`;
   fix every diagnostic and repeat until valid. `factory-show` gives the
   graph, digest, and state.
2. Never run a project factory yourself: `rlm.factory.run` refuses
   `.prime/machines/<name>/`. Start an approved one with `delegate` +
   `machine`. A design turn never runs it.
3. Pitfalls the validator catches: a `route` arm that is a list (use a pass
   state); a join member missing `next = "<join>"`; a route value without an
   arm; required inputs on an entry state; a required self-input; `list(...)`
   instead of `list[...]`; unknown keys anywhere; a prompt placeholder with
   no input or param; `foreach` on a Python state.
4. Pitfalls it cannot catch: `max_entries` left at 1 on a loop target (the
   loop blocks after one pass); unbounded outputs (keep ports short; the
   capture cap is 65,536 characters); a writer prompt without a recovery
   posture for retries (reconcile with `git status`/`git diff` first); a
   model from an unconfigured provider.
