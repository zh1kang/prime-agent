//! The workspace pages: history, the machine editor, and git review render at 110 and 64
//! columns, and their keys produce the calls pa-cli answers.

use super::*;
use crate::theme::{ColorMode, Theme};
use crate::width::line_width;

fn theme() -> Theme {
    Theme::builtin("prime", ColorMode::TrueColor)
}

fn kb() -> KeybindingsManager {
    KeybindingsManager::new()
}

fn text(lines: &[Line]) -> String {
    lines
        .iter()
        .map(|line| line.iter().map(|s| s.content.as_str()).collect::<String>())
        .map(|row| row.trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render at `width` and check that no line overflows it.
fn screen(page: &mut WorkspaceView, width: usize) -> String {
    page.set_viewport_rows(24);
    let lines = page.render(&theme(), width, &kb());
    assert_eq!(lines.len(), 24);
    for line in &lines {
        assert!(
            line_width(line) <= width,
            "{} > {width}: {}",
            line_width(line),
            text(std::slice::from_ref(line))
        );
    }
    text(&lines)
}

fn press(page: &mut WorkspaceView, keys: &[&str]) -> WorkspaceAction {
    let mut last = WorkspaceAction::None;
    for key in keys {
        last = page.handle_key(key, &kb());
    }
    last
}

fn request_of(action: &WorkspaceAction) -> (WorkspaceRequest, Vec<String>, Value) {
    match action {
        WorkspaceAction::Call {
            request,
            args,
            input,
        } => (*request, args.clone(), input.clone()),
        other => panic!("expected a call, got {other:?}"),
    }
}

fn history() -> WorkspaceView {
    let (mut page, first) = WorkspaceView::history("");
    assert_eq!(request_of(&first).1, ["history", "--json"]);
    page.apply(
        WorkspaceRequest::HistoryList,
        Ok(json!({"groups": [
            {"name": "T3/Flock", "rows": [{"title": "Fix the parser", "search": "t3 fix the parser", "target": ["t3", "a", "x"]}]},
            {"name": "Claude", "rows": [
                {"title": "Review grammar", "search": "claude review grammar", "target": ["claude", "b", "/c/b.jsonl"]},
                {"title": "Plan release", "search": "claude plan release", "target": ["claude", "c", "/c/c.jsonl"]}
            ]}
        ]})),
    );
    page
}

#[test]
fn history_renders_and_resumes_the_selected_conversation() {
    let mut page = history();
    let wide = screen(&mut page, 110);
    assert!(wide.contains("history  3 conversations"), "{wide}");
    assert!(
        wide.contains(">  Search threads"),
        "the settings search field:\n{wide}"
    );
    assert!(
        wide.contains("1 T3/Flock 1") && wide.contains("2 Claude 2"),
        "{wide}"
    );
    assert!(wide.contains("› Fix the parser"), "{wide}");
    assert!(wide.contains("Type to search · Tab/1-9 provider"), "{wide}");
    let narrow = screen(&mut page, 64);
    assert!(narrow.contains("Tab provider · Enter resume"), "{narrow}");

    press(&mut page, &["tab", "down"]);
    assert!(screen(&mut page, 110).contains("› Plan release"));
    let (request, args, input) = request_of(&press(&mut page, &["enter"]));
    assert_eq!(
        (request, args),
        (
            WorkspaceRequest::HistoryOpen,
            vec!["history".into(), "--open".into()]
        )
    );
    assert_eq!(
        input,
        json!({"target": ["claude", "c", "/c/c.jsonl"], "title": "Plan release"})
    );
    let opened = page.apply(
        WorkspaceRequest::HistoryOpen,
        Ok(json!({"path": "/s/import.jsonl"})),
    );
    assert_eq!(opened, WorkspaceAction::Resume("/s/import.jsonl".into()));
}

#[test]
fn history_search_filters_the_open_provider() {
    let mut page = history();
    press(&mut page, &["2", "g", "r", "a", "m"]);
    let shown = screen(&mut page, 110);
    assert!(
        shown.contains("> gram") && shown.contains("Review grammar"),
        "{shown}"
    );
    assert!(!shown.contains("Plan release"), "{shown}");
    page.paste("zz");
    assert!(screen(&mut page, 110).contains("No matching threads"));
    // Esc clears the search first, then closes.
    assert_eq!(press(&mut page, &["escape"]), WorkspaceAction::None);
    assert!(screen(&mut page, 110).contains("Plan release"));
    assert_eq!(press(&mut page, &["escape"]), WorkspaceAction::Close);
}

fn machines() -> WorkspaceView {
    let (mut page, first) = WorkspaceView::machines("");
    assert_eq!(request_of(&first).1, ["fsm-edit", "list"]);
    page.apply(
        WorkspaceRequest::Machines,
        Ok(json!({"machines": [
            {"name": "code-review", "source": "builtin", "kind": "md", "editable": false},
            {"name": "my-build", "source": "user", "kind": "toml", "editable": true}
        ]})),
    );
    page
}

fn draft() -> Value {
    json!({"draft": "/m/.pa-edit-1", "file": "/m/.pa-edit-1/MACHINE.md", "name": "code-review",
           "target": "my-code-review", "kind": "md",
           "states": [{"id": "review", "model": "inherit", "editable": true},
                      {"id": "publish", "model": "see definition", "editable": false}],
           "graph": {"states": [{"id": "review", "kind": "agent"}, {"id": "publish", "kind": "agent"}],
                     "transitions": [{"from": "review", "to": "publish"}]}})
}

fn compose_of(action: &WorkspaceAction) -> (String, String) {
    match action {
        WorkspaceAction::Compose { label, initial } => (label.clone(), initial.clone()),
        other => panic!("expected a prompt editor step, got {other:?}"),
    }
}

#[test]
fn machine_editor_copies_a_library_machine_and_edits_a_state_model() {
    let mut page = machines();
    let wide = screen(&mut page, 110);
    assert!(wide.contains("machines  2 installed"), "{wide}");
    assert!(
        wide.contains("› code-review") && wide.contains("copy to edit"),
        "{wide}"
    );
    screen(&mut page, 64);

    // A machine the user does not own asks for the copy's name in pa's prompt editor.
    let (label, initial) = compose_of(&press(&mut page, &["enter"]));
    assert_eq!(
        (label.as_str(), initial.as_str()),
        (
            "name for your editable copy of code-review",
            "my-code-review"
        )
    );
    let (request, _, input) = request_of(&page.compose_done(Some("my-code-revix".into())));
    assert_eq!(request, WorkspaceRequest::Draft);
    assert_eq!(
        input,
        json!({"name": "code-review", "target": "my-code-revix"})
    );

    page.apply(WorkspaceRequest::Draft, Ok(draft()));
    let shown = screen(&mut page, 110);
    assert!(
        shown.contains("my-code-review  copy of code-review"),
        "{shown}"
    );
    assert!(
        shown.contains("› review") && shown.contains("inherit"),
        "{shown}"
    );
    assert!(
        shown.contains("e edit file · g graph · s validate and save · r reload · E $EDITOR"),
        "{shown}"
    );
    assert!(screen(&mut page, 64).contains("e edit · g graph"));

    let (label, _) = compose_of(&press(&mut page, &["m"]));
    assert!(label.starts_with("model for review"), "{label}");
    let (request, _, input) = request_of(&page.compose_done(Some("anthropic/x".into())));
    assert_eq!(request, WorkspaceRequest::SetModel);
    assert_eq!(
        input,
        json!({"draft": "/m/.pa-edit-1", "state": "review", "model": "anthropic/x"})
    );
    // A cancelled text step changes nothing.
    page.apply(WorkspaceRequest::SetModel, Ok(draft()));
    press(&mut page, &["m"]);
    assert_eq!(page.compose_done(None), WorkspaceAction::None);

    press(&mut page, &["down", "m"]);
    assert!(screen(&mut page, 110).contains("referenced agent"));
    assert_eq!(
        press(&mut page, &["E"]),
        WorkspaceAction::EditFile("/m/.pa-edit-1/MACHINE.md".into())
    );
    assert_eq!(request_of(&page.file_edited()).0, WorkspaceRequest::States);
    assert_eq!(
        request_of(&press(&mut page, &["s"])).0,
        WorkspaceRequest::Save
    );
    let relist = page.apply(
        WorkspaceRequest::Save,
        Ok(json!({"saved": "my-code-review"})),
    );
    assert_eq!(request_of(&relist).1, ["fsm-edit", "list"]);
    assert!(screen(&mut page, 110).contains("Saved my-code-review for future runs."));
}

#[test]
fn the_definition_file_is_edited_in_the_prompt_editor() {
    let mut page = machines();
    press(&mut page, &["down"]);
    page.apply(WorkspaceRequest::Draft, Ok(draft()));
    let (request, args, input) = request_of(&press(&mut page, &["e"]));
    assert_eq!(
        (request, args[1].as_str()),
        (WorkspaceRequest::Read, "read")
    );
    assert_eq!(input, json!({"draft": "/m/.pa-edit-1"}));
    let (label, initial) = compose_of(&page.apply(
        WorkspaceRequest::Read,
        Ok(json!({"file": "/m/.pa-edit-1/MACHINE.md", "text": "---\nname: my-code-review\n---\n"})),
    ));
    assert_eq!(label, "my-code-review · MACHINE.md");
    assert_eq!(initial, "---\nname: my-code-review\n---\n");
    let (request, _, input) = request_of(&page.compose_done(Some("new text".into())));
    assert_eq!(request, WorkspaceRequest::Write);
    assert_eq!(input, json!({"draft": "/m/.pa-edit-1", "text": "new text"}));
    page.apply(WorkspaceRequest::Write, Ok(draft()));
    assert!(screen(&mut page, 110).contains("Draft updated. Press s to validate and save."));
}

#[test]
fn the_draft_graph_view_draws_the_compiled_states() {
    let mut page = machines();
    page.apply(WorkspaceRequest::Draft, Ok(draft()));
    press(&mut page, &["g"]);
    let shown = screen(&mut page, 110);
    assert!(
        shown.contains("review") && shown.contains("publish") && shown.contains("g states"),
        "{shown}"
    );
    assert!(
        shown.contains('┌') || shown.contains('╭'),
        "boxes:\n{shown}"
    );
    screen(&mut page, 64);
    let mut broken = draft();
    broken["graph"] = Value::Null;
    broken["graph_error"] = json!("states.x: unknown kind");
    page.apply(WorkspaceRequest::States, Ok(broken));
    assert!(screen(&mut page, 110).contains("No graph: states.x: unknown kind"));
}

#[test]
fn machine_editor_opens_a_named_machine_and_discards_its_draft_on_escape() {
    let (mut page, _) = WorkspaceView::machines("my-build");
    let first = page.apply(
        WorkspaceRequest::Machines,
        Ok(json!({"machines": [{"name": "my-build", "source": "user", "kind": "toml", "editable": true}]})),
    );
    assert_eq!(request_of(&first).2, json!({"name": "my-build"}));
    page.apply(WorkspaceRequest::Draft, Ok(json!({"draft": "/m/.pa-edit-2", "file": "/m/.pa-edit-2/my-build/machine.toml",
        "name": "my-build", "target": "my-build", "kind": "toml", "states": [{"id": "build", "model": "python", "editable": false}]})));
    press(&mut page, &["m"]);
    assert!(screen(&mut page, 110).contains("Press e to edit machine.toml and change a model."));
    let (request, _, input) = request_of(&press(&mut page, &["escape"]));
    assert_eq!(
        (request, input),
        (WorkspaceRequest::Discard, json!({"draft": "/m/.pa-edit-2"}))
    );
}

#[test]
fn a_project_factory_edit_saves_and_returns_to_the_captain() {
    let (mut page, first) = WorkspaceView::project_machine("/work/alpha", "proj-build");
    let (request, _, input) = request_of(&first);
    assert_eq!(request, WorkspaceRequest::Draft);
    assert_eq!(
        input,
        json!({"name": "proj-build", "project": "/work/alpha"})
    );
    assert!(page.returns_to_captain());
    page.apply(WorkspaceRequest::Draft, Ok(json!({"draft": "/m/.pa-edit-3",
        "file": "/m/.pa-edit-3/proj-build/machine.toml", "name": "proj-build", "target": "proj-build",
        "kind": "toml", "project": "/work/alpha", "states": [{"id": "build", "model": "python", "editable": false}]})));
    let shown = screen(&mut page, 110);
    assert!(
        shown.contains("project factory") && shown.contains("saving needs approval again"),
        "{shown}"
    );
    assert!(
        !shown.contains("$EDITOR"),
        "no outside editor for a project factory:\n{shown}"
    );
    assert_eq!(press(&mut page, &["E"]), WorkspaceAction::None);
    assert_eq!(
        request_of(&press(&mut page, &["s"])).0,
        WorkspaceRequest::Save
    );
    assert_eq!(
        page.apply(WorkspaceRequest::Save, Ok(json!({"saved": "proj-build"}))),
        WorkspaceAction::Close
    );
    assert_eq!(
        page.take_notice().as_deref(),
        Some("Saved proj-build. This version needs your approval before it runs.")
    );
}

fn review(member: Option<(String, String)>) -> WorkspaceView {
    let (mut page, first) = WorkspaceView::review(member);
    let (_, _, input) = request_of(&first);
    assert!(input == json!({"op": "files"}) || input["request"] == json!({"op": "files"}));
    let diff = page.apply(
        WorkspaceRequest::Files,
        Ok(json!({"root": "/work/alpha", "staged": false,
                  "files": [{"path": "src/parser.rs", "status": "M"}, {"path": "notes.md", "status": "?"}]})),
    );
    assert_eq!(request_of(&diff).0, WorkspaceRequest::Diff);
    page.apply(
        WorkspaceRequest::Diff,
        Ok(json!({"rows": [
            {"kind": "file", "text": ""},
            {"kind": "hunk", "text": "@@ -1,2 +1,2 @@ fn parse"},
            {"kind": "context", "old": {"n": 1, "sign": " ", "text": "fn parse() {"}, "new": {"n": 1, "sign": " ", "text": "fn parse() {"}},
            {"kind": "modified", "old": {"n": 2, "sign": "-", "text": "    old()"}, "new": {"n": 2, "sign": "+", "text": "    new()"}}
        ]})),
    );
    page
}

#[test]
fn review_renders_split_wide_and_one_column_narrow() {
    let mut page = review(None);
    let wide = screen(&mut page, 110);
    assert!(wide.contains("review  2 unstaged files"), "{wide}");
    assert!(wide.contains("› M src/parser.rs"), "{wide}");
    assert!(wide.contains("@@ -1,2 +1,2 @@ fn parse"), "{wide}");
    assert!(
        wide.contains("2 -     old()") && wide.contains("2 +     new()"),
        "{wide}"
    );
    assert!(wide.contains("v layout · s stage"), "{wide}");
    let narrow = screen(&mut page, 64);
    assert!(
        narrow.contains("2 +     new()") && !narrow.contains("2 -     old()"),
        "{narrow}"
    );
}

#[test]
fn review_keys_stage_toggle_and_page_through_files() {
    let mut page = review(None);
    let (request, args, input) = request_of(&press(&mut page, &["s"]));
    assert_eq!(
        (request, args),
        (
            WorkspaceRequest::Stage,
            vec!["review".into(), "json".into()]
        )
    );
    assert_eq!(input, json!({"op": "stage", "path": "src/parser.rs"}));
    let refresh = page.apply(WorkspaceRequest::Stage, Ok(json!({"ok": true})));
    assert_eq!(
        request_of(&refresh).2,
        json!({"op": "files", "staged": false})
    );
    assert_eq!(
        request_of(&press(&mut page, &["tab"])).2,
        json!({"op": "files", "staged": true})
    );
    let (_, _, input) = request_of(&press(&mut page, &["down"]));
    assert_eq!(
        input,
        json!({"op": "diff", "path": "notes.md", "staged": false, "mode": "split"})
    );
    assert_eq!(request_of(&press(&mut page, &["v"])).2["mode"], "unified");
    assert_eq!(press(&mut page, &["escape"]), WorkspaceAction::Close);
}

#[test]
fn a_crew_review_goes_through_the_captain_backend() {
    let mut page = review(Some(("p-alpha".into(), "c-fix".into())));
    let (_, args, input) = request_of(&press(&mut page, &["s"]));
    assert_eq!(args, ["captain", "act", "review"]);
    assert_eq!(
        input,
        json!({"project": "p-alpha", "member": "c-fix", "request": {"op": "stage", "path": "src/parser.rs"}})
    );
}
