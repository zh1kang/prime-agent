# A project Captain for pa

Design recommendation, 21 November 2026.

Give the project a persistent Captain and let it use the existing factory for individual jobs.
Expose both through F6, inside pa's own interface.
A Captain owns an objective across conversations.
A factory runs a defined procedure with a start, transitions, and an end.

## What Capy ships

| Question | Documented behavior | Consequence for pa |
| --- | --- | --- |
| Who owns the project? | A Captain keeps a plan, its own rules, shared crew rules, and a digest. A user can promote an existing thread with its history. [Captains](https://docs.capy.ai/captains) | A Captain needs a stable identity across sessions. The current chat can be its first conversation. |
| Who belongs to the crew? | A Captain can adopt and release existing root threads in the same project. Each thread belongs to at most one crew and can spawn its own subagents. [Add crew](https://docs.capy.ai/api-reference/captains/add-crew) | Keep crew membership separate from thread ancestry. Releasing a thread must preserve its transcript and worktree. |
| How does the lead stay current? | A crew thread reports after a turn or when it needs help. The Captain maintains a digest and brings decisions back to the user. [Digest](https://docs.capy.ai/captains#read-the-digest) | Persist report IDs, open decisions, and the last report processed. Link each digest item to its thread or run. |
| What starts more work? | Messages, completions, CI results, review comments, and configured watches can wake a thread. Some events can be kept as context without starting a turn. [Background work](https://docs.capy.ai/async-work) | Start with local reports and user replies. Coalesce routine updates. Reserve model turns for decisions that change the plan. |
| How are edits isolated? | Subagents can use a shared machine for reading or a fresh one for writing. Capy distinguishes independent parallel work from dependent changes stacked in order. [Subagents](https://docs.capy.ai/subagents) | Use the existing worktree support for independent writers. Give a dependent writer the accepted base revision of its predecessor. |
| What context does a worker receive? | A Capy subagent starts from an explicit assignment. The parent receives a summary and can inspect details. [Subagent isolation](https://docs.capy.ai/subagents#subagents-work-in-isolation) | Keep the current subthread fork for continuing a conversation. Give a new crew assignment a brief, constraints, acceptance checks, and links to relevant context. |

These sources establish Capy's public behavior.
They do not establish how Capy stores events internally or how reliably its service recovers from a crash.

## Where pa already has the pieces

- The factory can fan out, join results, enforce budgets, and retry or escalate a failed state. Its run registry lives in the Python kernel. A kernel restart loses the registry while supervisor-owned children can continue. A saved run ID alone cannot resume that machine. [Factory executor](../prime-agent-runtime/src/rlm/factory.py)
- The durable thread store already records a thread ID, parent, checkout, provider, transcript, pending request, and job status. Its child tools clone the parent's context and restrict reads to direct children. A Captain adopting an existing root thread needs an explicit membership check. [Thread store](../crates/pa-core/src/thread.rs) and [child tools](../crates/pa-cli/src/thread_tools.rs)
- Prime's daemon already journals session inbox messages and replays unread messages on open. This includes factory progress and child lifecycle notices. The Captain needs a bridge from participating sessions and provider threads to its project record. [Session inbox](../crates/pa-core/src/session_engine/inbox.rs)
- The workspace binding distinguishes the main checkout, linked worktree, branch, and base revision. The Captain should keep a stable project ID across worktree moves and support a new folder before its first commit. [Workspace binding](../crates/pa-core/src/workspace/binding.rs)
- The personal dual-consensus machine already gives Opus and Astra distinct proposal, challenge, reconciliation, and voting steps. Its Python tally requires both votes to accept the same candidate. A Captain can use that machine for a disputed plan and present unresolved disagreement to the user.

## First release

1. Open the project from F6 or the proposed `/captain` shortcut. Create or resume a lead conversation, name the objective, and return to the same Captain from a linked checkout. Keep ordinary chat and direct factory launches available.
2. Give the Captain a durable project record for its objective, rules, crew membership, assignments, and open decisions. Keep each transcript in its existing session store. Store typed references so a native Prime session and a durable provider thread can both be resolved after restart. Use the adopted Prime continuation when the source was imported through `/resume`.
3. Give the model the same operations the user can perform in the UI. It can adopt or release a thread, delegate an assignment, request a factory run, and read reports. Record the assignment and its stable dispatch ID before starting a worker. Link the resulting worker ID and workspace to the assignment. If a launch was interrupted, reconcile that ID before retrying.
4. Show **Needs you**, **In progress**, and **Recent results** in F6. Open a crew thread to continue it, a run to inspect its FSM, or a changed worktree to enter the existing review screen. Keep `/fsm edit` focused on machine definitions.
5. Let one lead serialize decisions. Offer the existing high-effort Opus/Astra pair for plan approval, conflicting reports, or an explicit request for consensus. Both models can deliberate. One coordinator records and dispatches the accepted decision once.

A machine that loops forever would inherit the kernel's lifetime and make adopting an unrelated thread awkward.
An independent second execution engine would have to own scheduling, provider sessions, review, and cancellation again.
The proposed split adds project ownership at the point where these existing components meet.
Cloud machines, Slack, and automatic CI wakeups can follow once local recovery works.

## Acceptance test

Start in an empty folder, create a Captain, and return after closing the terminal.
Adopt an imported conversation and release it again with its complete history.
In a Git project, give two writers separate worktrees and open each diff from the Captain.
Kill the daemon between starting one worker and recording its response, then deliver the same completion twice.
The recovered Captain must show one assignment, one worker, and one result or an explicit unresolved state.
Restart a kernel during a factory run and surface the lost run plus any surviving children for reconciliation.
An uncertain dispatch must never start another writer automatically.
Finally, block one worker, answer its question once from the digest, and observe exactly one continuation in the correct worktree.

## Captain, crew threads, and factory runs

The Captain owns the objective and decides who does each piece of work.
A crew thread takes open-ended work and answers in conversation.
A factory run executes a known procedure with verification gates, such as build, review, validate, or a two-model decision.
Both start through one call: `delegate`, with `machine` and `params` for a run.
Both use the same request ID idempotency, the same project `parallel` limit (one slot per run), the same writer worktrees and base revisions, and the same crew rules.
Every agent step of a run gets the crew rules as a preamble and never the Captain's own rules.

A Captain-started run is hosted by its job worker, not by a chat.
The worker starts `pa thread factory-host`, a print-mode session whose only work is one kernel cell.
That cell runs the normal executor, writes the live graph to the run's control folder, and applies answers.
The executor journal (`runs.jsonl`) reports progress to the Captain as before.
Runs started from chat with `/fsm` or the F6 factory view keep their chat kernel.

| State | Meaning | Set by |
| --- | --- | --- |
| queued | Job and crew row recorded, worker not started | `delegate` |
| starting | Worker took the job and is launching the host | job worker |
| running | Executor started the run | journal, read by the watcher |
| paused | Run waits for review; shown under Needs you | journal, read by the watcher |
| done, failed, stopped | Host wrote its result and exited | job worker, or recovery after the worker died |
| lost | Host exited without a result | job worker, or recovery after the worker died |

A run starts at most once.
The worker launches the host only on the queued to running job transition.
The host claims its control folder before it calls the executor and refuses a claimed or abandoned folder.
Recovery never relaunches a run.
It takes the host lock before it decides, so a slow host cannot start after the run was marked lost.
The end is reported once under the fixed report ID `<member>/end`.
An answer is keyed to the pause number, so a second answer to the same pause is refused.
Answer text goes to the retried step as a reviewer note through `rlm.factory.resume(run_id, note)`.
A pause with no failed step, such as a budget pause, has nothing to retry, so the note does not reach a step; the host records `notes_applied` in `resume-N.json`.

Permission default: the Captain may start a read-only machine on its own and must ask with NEEDS YOU: before a machine that writes code.
A machine is read-only when its folder has `captain.toml` with `access = "read"`; unknown machines count as writers.
A Captain-started writer run needs one of two things:

- `approved: true` in a turn that answers a user message (job purpose `lead` or `brief`), or
- a grant for that machine, which the user gives in chat and the Captain records with `grant(machine)`.

The backend reads the turn type from the lead's running job, not from anything the Captain passes, so crew output cannot approve a writer run or create a grant.
The Captain can grant or revoke only in a turn that answers a user message, and each change is recorded in `edits` with field `grants`.
Crew turns and factory hosts run with `PA_CAPTAIN_ACTOR=crew:<project>`; from them, `grant`, `factory-approve`, `factory-delete`, and every other change is refused, and only `factory-validate` and `factory-show` work.
These checks stop mistakes and crew output on the normal path.
They are not a sandbox: the Captain and crew threads have a shell, so they could bypass any local check on purpose, for example by calling the command without that variable. A Prime crew turn that runs in the shared daemon gets its tool environment from the daemon, not from this variable.

## Project factories

A Captain can write a factory for its project at `<project root>/.prime/machines/<name>/`, in the machine directory format (`machine.toml`).
The Captain reads `rlm.factory.help()` for the format and runs `factory-validate` until the machine is valid.
The catalog of that project lists these machines with source `project`.
A project factory whose name matches a library machine is invalid; rename it.
A run passes the directory path to `rlm.factory.run`.

The Captain writes these files, so the user approves one exact content version.
The digest is SHA-256 over the sorted relative paths and the bytes of every file in the folder, except `__pycache__`.
A symlink whose target leaves the folder makes the factory invalid, as in the machine loader.
A `machine.toml` that names a path under `__pycache__` is invalid, because the approval does not cover those files.
Each check copies the folder once into a temporary snapshot; the digest, `pa factory validate`, and the compiled graph all come from that one snapshot.
The approved digest is stored in the `factory_approvals` table of the Captain hub database.

| State | Meaning | Runs |
| --- | --- | --- |
| missing | no `<name>/machine.toml` | no |
| invalid | does not validate, a symlink leaves the folder, the name does not match the folder, or a library machine has the name | no |
| draft | valid and never approved | no |
| approved | valid and its digest equals the approved digest | yes |
| changed | valid and its digest differs from the approved digest | no |

Any edit after approval, by the Captain or anyone, makes the factory a draft again; a delegation of it is refused with a NEEDS YOU message.
A project factory always writes code: its `captain.toml` is ignored, because the Captain could write `access = "read"` itself.
Grants by machine name do not apply to project factories; the approval of a digest is the permission for that version.
Python nodes run code, and the approval covers them.
At run start the worker copies the approved files into the run's control folder, checks that the copy has the approved digest, and runs the copy, so a later edit cannot change a run that started.

Approval follows the grant rules: the user approves from the review page, or the Captain calls `factory-approve` with the digest in a turn that answers a user message.
A turn started by a crew event or a design request cannot approve.
Each lead turn records the factory digests it starts with, and a Captain approval is refused unless the digest is still the one the turn started with, so the Captain cannot change a factory and approve the change in one turn.
On the review page, `a` sends the digest from the user's last explicit load (`F` or `r`); when a refresh brings a different digest, the page shows "Changed - press r to review again" and `a` waits for `r`.
Only the user deletes a project factory, and never while a run of it is queued or running.

Operations: `factory-validate(name)` returns the state and errors, `factory-show(name)` adds the compiled graph and `live`, `factory-approve(name, digest)`, `factory-delete(name)`, and `factory-design(text?)`.
`factory-design` queues a Captain turn with purpose `design`: the Captain writes a factory from the objective, plan, and rules, validates it, and reports.
A design turn cannot start a run.
The runtime refuses `rlm.factory.run` on any machine directory inside a `<dir>/.prime/machines/` tree ("Project factories run through the Captain after approval."), so no chat or design turn can run a draft directly; Captain runs use the approved copy in the run's control folder, outside that tree.
A name shared by a library machine and a project factory folder is refused at delegation with both paths named.
Onboarding offers "Design a factory for this project" first in the factory step; the design request then runs after the first turn that writes the plan.

## The Captain in pa's interface

F6 and `/captain` open the Captain panel inside pa, drawn by pa-tui like the factory page.
The panel reads `pa thread captain status` and runs every action through `pa thread captain act OP`; the Python backend stays the source of truth.
Text steps (a brief, a message, an answer, a field edit, a run parameter) use pa's own prompt editor with a header line above it.
Lists (models, thinking levels, machines, worker limit, threads to add) use the panel's picker.

| Key | Action |
| --- | --- |
| ↑↓, Tab, ←→ | move, focus the crew, change project |
| Enter, o | open the Captain or the selected crew thread in the normal chat |
| m | message the Captain or a crew thread; on a paused run, answer it |
| n, p | new Captain, or promote this chat |
| e, a, d, u | edit a field, add a thread, assign work, release a thread |
| f, c, v, x | factory, consensus, git review, rules and plan |
| F | project factories: review a draft, or request a design |
| s, R, D | stop, resume, delete |

Opening a chat resumes its native session in the same view, without a terminal handoff.
The draft of the chat being left is stashed and returns when that chat opens again.
A chat of a Captain project shows a breadcrumb above the prompt, such as `captain › parser-fix · F7 switch chat · Shift+F7 captain`.
F7 or `/crew` opens the chat switcher; Shift+F7 or `/captain chat` opens the Captain's chat.
A project factory that is a draft or changed since approval shows under the project as needs you.
`F` lists the project factories and "Design a factory for this project".
The review page draws the compiled machine with the factory page diagram, without live state, and shows the state, the short digest, and any validation errors.
Its keys are a approve, e edit `machine.toml` in `$EDITOR`, m send a change request to the Captain, D delete (with a confirm), and Esc back.

A provider CLI thread (claude, codex, antigravity) has no native session, so it opens read-and-reply inside the panel; replies go through the Captain's `reply` job.

### Crew threads are pa chats

A Prime durable thread gets a native session file when it is created, so pa can open the lead and new Prime crew threads before their first turn.
The panel's assign form offers pa models, and the backend starts the crew thread with provider prime and that model.
The provider choice maps to a pa model: Claude to `anthropic/claude-opus-5-5` (through the claude CLI provider), Codex to an `openai-codex` model, and Antigravity to an `antigravity/*` model.
Adopted provider CLI threads keep working through their CLIs.

### One turn per session

A Captain job runs a Prime turn in its own process, which holds the session's runtime lease.
When an open pa chat already holds the lease, the job sends its turn through that chat's daemon (`--mode acp`) instead.
The open chat then shows the Captain's turn live, and a message typed in the chat queues behind it.
Opening a chat while a job holds its lease shows that the Captain is running a turn there.
A message typed in the Captain's own chat is a Captain turn: pa queues it as a `lead` job, so it runs through the same route, counts as a user message for approvals and grants, and gets the Captain's context.
The transcript shows only what the user typed, or a short line for other turns (`Brief: …`, `Crew update: 2 reports`).
The Captain's instructions, rules, plan, digest, crew reports, and the `act` command are hidden context: in its own process the turn adds them to the system prompt, and in the daemon route they ride the prompt's `_meta` and the daemon writes them as a `display: false` custom row before the user message.
A crew turn works the same way: the transcript shows the assigned task, and the crew rules are hidden context.
In both routes the Captain's instructions give it the command `env … PA_CAPTAIN_ACTOR=<project> pa thread captain act`, so its `act` calls carry the Captain scope; a model that runs `act` without that command loses the scope, which the permission note above already allows for.

### Stop, resume, and delete

| State | New turns | Leaves by |
| --- | --- | --- |
| active | yes | stop to stopping, delete to deleting |
| stopping | no | paused, when no job worker runs |
| paused | no | resume to active, delete to deleting |
| deleting | no | removed, when no job worker runs |

Stop cancels queued jobs, sends SIGTERM to the process group of each running job worker (its provider turn ends with it), and writes `stop` into each Captain-started run's control folder.
A paused project queues no digest wakeups, and the job worker cancels any job that raced the stop.
A stopped turn keeps its thread and native session; `pa thread ID abandon` clears its unfinished-turn marker.
Delete stops work the same way, then removes the project, crew, jobs, reports, and edits.
The lead thread, crew threads, transcripts, and worktrees stay.
Each action is safe to repeat, and `reconcile` finishes a stop or delete that a restart interrupted.
