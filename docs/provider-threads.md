# Provider threads

Run `pa thread providers` to check which executables are installed.
Run `pa thread connect codex`, `pa thread connect claude`, or `pa thread connect antigravity` to sign in with that executable.
Run `pa thread models antigravity` to list Antigravity models discovered from `agy` (or T3 Code's managed Antigravity binary).
Prime uses its own model configuration and credentials.

```sh
thread=$(pa thread new codex)
pa thread "$thread" chat
```

Type a message and press Enter to send it.
Use `/pin Keep the public API stable` to record a handoff note.
Use `/context` to inspect the text another provider will receive.
Use `/switch claude` or `/switch antigravity` to select a provider for the next turn.
Use `/model MODEL_ID` (or `/model default`) and `/thinking off|minimal|low|medium|high|xhigh|max` (or `/thinking default`) to set the model and reasoning depth for the thread.
The first turn in a fresh native session receives the pin, the first message, and a bounded selection of recent user and assistant text.
Later turns resume the native session directly.
Earlier messages stay available through `/history`.
Provider tools, permissions and credentials belong to the selected executable.
An interrupted request can already have changed files. Inspect `/info` and `/review` before `/recover`.

Ask “spin up a subthread” to create a child, or give it a task to start work in the background.
Codex and Claude get two MCP tools for the current thread: `spawn_subthread` and `read_subthread`.
Prime invokes the same operations through its Python tool.
The child inherits the conversation and worktree, and gets a separate provider session.

Use `/child codex` to create a child at the current point and `/open UUID` to enter it.
To work concurrently, run `pa thread UUID chat` in a second terminal.
The child starts with a copy of the conversation and an independent provider session.
Use `/children` to list direct children and `/info` to see the parent ID.

Use `/worktree create ../feature feature-branch` for an isolated checkout or `/worktree move ../existing` for an existing checkout of the same repository.
The next provider turn starts in the selected checkout with the handoff context.
Uncommitted files stay in their original checkout.
Run `/review` in a terminal to inspect the current worktree using Diffy’s diff engine.
Use the arrow keys to select a file, `n` and `p` to jump between hunks, `v` to switch layouts, `Tab` to switch between staged and unstaged files, and `s` to stage or unstage the selected file.
Press `q` to return to the same chat.
Use `/review gui` to open the desktop app.
Use `/review status`, `/review diff`, and `/review staged` to inspect checkout changes, and `/review stage "path with spaces"` or `/review unstage "path with spaces"` to update the index.
Diff and staged views accept file paths.

To continue a conversation created outside Prime, run `pa thread import claude /path/to/session.jsonl` or use `codex` or `prime` in place of `claude`.
Import uses the workspace recorded in the selected log.
Close the original client before continuing the native session.
Use `--context-only` to copy the visible history into an independent session in the current directory.
Codex histories with rollbacks are refused.

### Commands in the main terminal interface

Run `pa` in the worktree you want to review.
Type `/review` to open its diff and press `q` to return to the conversation.
Type `/subthread` to branch the current conversation and open the child.
In a durable `pa thread <id> chat`, use `/subthread <task>` to start a child in the background and `/children` to list its ID.

Use `/resume` in the main terminal (or `/history [text]`) or `pa thread history [text]` from the shell to browse local conversations by provider, newest first within each section.
The list includes T3 Code/Flock, Codex, Claude Code, Prime sessions, and durable pa threads.
Search by title, provider, date, or ID. In the viewer, use Tab to switch providers, the arrow keys to move through titles, Enter to continue the selected thread, `/` to filter the current section, and `q` to close.
In durable thread chat, `/threads [text]` opens the same list and `/history` prints the current transcript.

In the main terminal, open `/settings` and press `5` for Agents.
Press F6 or enter `/captain` to open the project dashboard. Press F6 or q to return to chat. Press f for the live factory, including in a new project. In the factory, press n to choose a machine, fill in its parameters, and press Enter in chat to send the launch request.

Open `/fsm` for the live factory page. Press `e` there, choose **Edit factory machines** in settings, or use `/fsm edit [machine]` to edit library definitions. For a directory machine such as dual-consensus, the editor opens machine.toml. Change the models under roles, then save and close the editor to validate. For a MACHINE.md definition, select a state by number to change its model. Use `edit` for prompts, transitions, guards, and run limits; `save` validates and imports the definition for future runs.
Select **Live factory runs** to inspect real factory runs. Enable **Factory enabled** and restart the client if the factory lane is disabled.
The same tab includes provider sign-in, review, history, subthreads, and durable thread management.
Inside a durable thread, use `/switch PROVIDER` for a handoff or `/worktree list` to inspect worktrees.

Selecting a Prime session resumes it directly. Selecting an external thread imports its saved user and assistant text into a native Prime conversation. Later selections reopen that Prime continuation. Messages sent there stay in Prime.

### Project Captains

Use `n` in the dashboard to set an objective for the current checkout. A project can start in a folder with no commits. Linked worktrees share a Captain. Each project has a persistent dot pattern and color. The selection arrow follows the keyboard, while the dot after the project name identifies the current checkout. You can open a project by name with `/captain NAME`.

Press `m` with a project selected to message its lead, or press Enter to read its conversation. Use `e` to change the objective, plan, lead rules, shared crew rules, name, or parallel worker limit. The default permits two crew turns at once per project. New assignments can queue while both workers are busy.

Press `a` to adopt a saved Prime conversation or an existing root provider thread in the same project. Open an external Flock conversation through `/resume` first. Press `d` to delegate. An isolated writer starts in a linked worktree at a recorded commit. A shared reader sees the current checkout and receives a read-only instruction. Use `Tab` to select crew, `m` to answer, `v` to review the selected checkout, and `u` to release a thread. The question and answer remain in the thread.

Crew results and open decisions update the digest while the dashboard is closed. Press `c` to prepare a dual-consensus run, or `f` to open the project's factory. Its journal records the run and child IDs. If the kernel exits, the Captain flags the interrupted run for inspection. An interrupted writer is reconciled against its saved transcript before its result is accepted. The Captain never automatically repeats an uncertain assignment.

From a shell use `pa thread captain status` for JSON or `pa thread captain act OP` with a JSON object on stdin. The installed Captain skill makes these operations available from ordinary Prime prompts.
