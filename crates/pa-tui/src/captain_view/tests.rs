//! The Captain panel: rendering at 110 and 64 columns, selection, the open, stop, resume, and
//! delete flows, the new-project form, the provider-thread view, and the crew picker.

use super::*;
use crate::keybindings::KeybindingsManager;
use crate::session_ui::CrewChat;
use crate::theme::{ColorMode, Theme};
use serde_json::json;

fn theme() -> Theme {
    Theme::builtin("prime", ColorMode::TrueColor)
}

fn kb() -> KeybindingsManager {
    KeybindingsManager::new()
}

fn text(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.iter().map(|s| s.content.as_str()).collect::<String>())
        .map(|row| row.trim_end().to_string())
        .collect()
}

fn snapshot() -> Value {
    json!({
        "current_root": "/work/alpha",
        "current": "p-alpha",
        "projects": [
            {
                "id": "p-alpha", "name": "alpha", "root": "/work/alpha", "objective": "Ship Alpha",
                "provider": "prime", "model": "", "thinking": "high", "parallel": 2, "state": "active",
                "lead": "lead-a", "lead_state": "running", "lead_session": "/s/lead-a.jsonl", "lead_busy": true,
                "rules": "Ask before deleting files.", "plan": "", "crew_rules": "Run unit tests.",
                "digest": "Needs you\n- pick a parser library\nNew\n- grammar drafted",
                "edits": [{"field": "rules", "actor": "user"}],
                "crew": [
                    {"id": "c-fix", "kind": "thread", "title": "parser-fix", "provider": "prime",
                     "state": "needs_you", "detail": "NEEDS YOU: choose a grammar", "session": "/s/c-fix.jsonl"},
                    {"id": "c-cli", "kind": "thread", "title": "claude review", "provider": "claude",
                     "state": "running", "busy": true},
                    {"id": "f-scan", "kind": "factory", "title": "slow-scan · Scan the tree", "machine": "slow-scan",
                     "state": "needs_you", "run_state": "paused", "progress": "1/3", "step": "scan", "headless": true}
                ]
            },
            {
                "id": "p-beta", "name": "beta", "root": "/work/beta", "objective": "Ship Beta",
                "state": "paused", "lead": "lead-b", "lead_state": "ready", "crew": []
            }
        ]
    })
}

fn view(rows: usize) -> CaptainView {
    let mut view = CaptainView::new(rows);
    view.apply_snapshot(&snapshot());
    view
}

#[test]
fn the_panel_renders_projects_details_and_crew_at_110_columns() {
    let view = view(34);
    let rows = text(&view.render(&theme(), 110, &kb()));
    let all = rows.join("\n");
    assert_eq!(rows.len(), 34, "the panel fills its viewport:\n{all}");
    assert!(
        rows[0].contains("captain") && rows[0].contains("2 projects · 1 needs you · 1 paused"),
        "{}",
        rows[0]
    );
    assert!(
        all.contains("› ■ alpha") && all.contains("!2 ●2"),
        "the selected project and counts:\n{all}"
    );
    assert!(all.contains("needs you  · here"), "{all}");
    assert!(
        all.contains("■ beta") && all.contains("paused"),
        "the paused indicator:\n{all}"
    );
    assert!(all.contains("alpha  prime · default model · high"), "{all}");
    assert!(
        all.contains("captain  ● running   rules updated by user"),
        "{all}"
    );
    assert!(all.contains("pick a parser library"), "{all}");
    assert!(all.contains("x  rules 1 · plan 0 · crew rules 1"), "{all}");
    // Each crew member is two rows: icon, name, and kind; then state text and detail.
    let row = |name: &str| {
        let i = rows
            .iter()
            .position(|r| r.contains(name))
            .unwrap_or_else(|| panic!("{name}:\n{all}"));
        (rows[i].clone(), rows[i + 1].clone())
    };
    assert!(all.contains("needs you  2"), "{all}");
    let (first, second) = row("! parser-fix");
    assert!(first.trim_end().ends_with("chat"), "{first}");
    assert!(
        second.contains("needs you · NEEDS YOU: choose a grammar"),
        "{second}"
    );
    let (first, second) = row("! slow-scan");
    assert!(first.trim_end().ends_with("factory"), "{first}");
    assert!(
        second.contains("needs you · 1/3 · scan · Scan the tree"),
        "a factory run row:\n{second}"
    );
    let (first, second) = row("● claude review");
    assert!(
        first.trim_end().ends_with("claude") && second.trim_end().ends_with("│     working"),
        "{first}\n{second}"
    );
    assert!(
        !all.contains("recent results  0"),
        "empty groups take no rows:\n{all}"
    );
    let footer = rows.last().unwrap();
    for key in ["s stop", "R resume", "D delete", "? help", "F6 close"] {
        assert!(footer.contains(key), "{key} in {footer}");
    }
    assert!(rows.iter().all(|r| str_width(r) <= 110));
}

#[test]
fn the_panel_fits_64_columns() {
    let view = view(24);
    let rows = text(&view.render(&theme(), 64, &kb()));
    assert!(rows.iter().all(|r| str_width(r) <= 64), "{rows:#?}");
    let all = rows.join("\n");
    assert!(all.contains("■ alpha") && all.contains("■ beta"), "{all}");
    let footer = rows.last().unwrap();
    assert!(
        footer.contains("s stop") && footer.contains("D delete") && footer.contains("F6 close"),
        "{footer}"
    );
}

#[test]
fn a_short_pane_shows_the_focused_crew_and_every_key_hint_at_80_columns() {
    let mut view = view(16);
    let rows = text(&view.render(&theme(), 80, &kb()));
    assert!(rows.last().unwrap().contains("F6 close"), "{rows:#?}");
    view.handle_key("tab", &kb());
    let all = text(&view.render(&theme(), 80, &kb())).join("\n");
    assert!(
        all.contains("! parser-fix") && all.contains("needs you · NEEDS YOU"),
        "the focused crew is visible:\n{all}"
    );
}

#[test]
fn tab_moves_to_the_crew_and_enter_opens_the_selected_chat() {
    let mut view = view(34);
    assert_eq!(
        view.handle_key("enter", &kb()),
        CaptainAction::Open {
            project: "p-alpha".into(),
            member: None
        },
        "Enter on a project opens its Captain chat"
    );
    view.handle_key("tab", &kb());
    assert_eq!(
        view.handle_key("enter", &kb()),
        CaptainAction::Open {
            project: "p-alpha".into(),
            member: Some("c-fix".into())
        }
    );
    let rows = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(rows.contains("› ! parser-fix"), "{rows}");
    // A Captain-started run has no chat: Enter explains instead of opening.
    view.handle_key("down", &kb());
    assert_eq!(view.handle_key("enter", &kb()), CaptainAction::None);
    assert!(text(&view.render(&theme(), 110, &kb()))
        .join("\n")
        .contains("runs headless"));
    // m on a paused run answers it in pa's editor.
    match view.handle_key("m", &kb()) {
        CaptainAction::Compose {
            purpose: ComposePurpose::Answer { member, .. },
            ..
        } => assert_eq!(member, "f-scan"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_message_goes_through_the_editor_to_the_lead_or_a_crew_thread() {
    let mut view = view(34);
    let CaptainAction::Compose { purpose, label, .. } = view.handle_key("m", &kb()) else {
        panic!("m composes")
    };
    assert_eq!(label, "message captain");
    assert_eq!(
        view.compose_done(purpose, Some("Status?".into())),
        CaptainAction::Act {
            op: "lead".into(),
            args: json!({"project": "p-alpha", "text": "Status?"}),
            done: "Message sent.".into(),
        }
    );
    view.handle_key("tab", &kb());
    let CaptainAction::Compose { purpose, .. } = view.handle_key("m", &kb()) else {
        panic!("m composes")
    };
    let CaptainAction::Act { op, args, .. } = view.compose_done(purpose, Some("Go".into())) else {
        panic!()
    };
    assert_eq!(
        (op.as_str(), args["member"].as_str()),
        ("reply", Some("c-fix"))
    );
}

#[test]
fn stop_asks_first_and_then_stops() {
    let mut view = view(34);
    assert_eq!(view.handle_key("s", &kb()), CaptainAction::None);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(screen.contains("stop the Captain of alpha?"), "{screen}");
    assert!(screen.contains("No new turns start, including digest wakeups, until you resume."));
    assert!(screen.contains("y confirm · n cancel"));
    assert_eq!(view.handle_key("n", &kb()), CaptainAction::None);
    assert!(!text(&view.render(&theme(), 110, &kb()))
        .join("\n")
        .contains("stop the Captain"));
    view.handle_key("s", &kb());
    assert_eq!(
        view.handle_key("y", &kb()),
        CaptainAction::Act {
            op: "stop".into(),
            args: json!({"project": "p-alpha"}),
            done: "Captain stopped. Press R to resume.".into(),
        }
    );
}

#[test]
fn a_paused_captain_resumes_and_refuses_new_work() {
    let mut view = view(34);
    view.handle_key("down", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(screen.contains("captain  ■ paused  R resume"), "{screen}");
    assert_eq!(
        view.handle_key("s", &kb()),
        CaptainAction::None,
        "already paused"
    );
    assert_eq!(view.handle_key("m", &kb()), CaptainAction::None);
    assert!(text(&view.render(&theme(), 110, &kb()))
        .join("\n")
        .contains("Resume this Captain"));
    assert_eq!(
        view.handle_key("R", &kb()),
        CaptainAction::Act {
            op: "resume".into(),
            args: json!({"project": "p-beta"}),
            done: "Captain resumed.".into(),
        }
    );
}

#[test]
fn delete_says_what_happens_and_stops_running_work_first() {
    let mut view = view(34);
    view.handle_key("D", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(screen.contains("delete the Captain of alpha?"), "{screen}");
    assert!(
        screen.contains("2 running turn(s) or run(s) stop first."),
        "{screen}"
    );
    assert!(screen.contains("The Captain thread and 2 crew thread(s) return to the thread list."));
    assert!(screen.contains("Transcripts and worktrees stay."));
    assert_eq!(
        view.handle_key("y", &kb()),
        CaptainAction::Act {
            op: "delete".into(),
            args: json!({"project": "p-alpha"}),
            done: "Captain deleted. Its threads stay in the thread list.".into(),
        }
    );
}

#[test]
fn the_new_project_form_walks_every_step_and_creates_the_captain() {
    let mut view = view(34);
    // This checkout has a Captain already.
    assert_eq!(view.handle_key("n", &kb()), CaptainAction::None);
    let mut data = snapshot();
    data["current_root"] = json!("/work/gamma");
    data["current"] = Value::Null;
    view.apply_snapshot(&data);
    assert_eq!(
        view.handle_key("n", &kb()),
        CaptainAction::LoadChoices {
            project: None,
            reference: None
        }
    );
    let choices = json!({
        "models": [{"provider": "prime", "model": ""}, {"provider": "prime", "model": "anthropic/claude-opus-5-5"}],
        "thinking": ["default", "low", "high"],
        "machines": ["project-builder"],
        "adopt": [{"ref": "/s/old.jsonl", "label": "Prime · old chat"}],
    });
    let CaptainAction::Compose { purpose, label, .. } = view.apply_choices(&choices, None) else {
        panic!("the brief uses pa's editor")
    };
    assert!(label.starts_with("captain brief"));
    assert_eq!(
        view.compose_done(purpose, Some("Ship Gamma".into())),
        CaptainAction::None
    );
    let screen = text(&view.render(&theme(), 64, &kb())).join("\n");
    assert!(
        screen.contains("new captain · model")
            && screen.contains("prime · anthropic/claude-opus-5-5"),
        "{screen}"
    );
    view.handle_key("down", &kb());
    view.handle_key("enter", &kb()); // model
    view.handle_key("down", &kb());
    view.handle_key("down", &kb());
    view.handle_key("enter", &kb()); // thinking: high
    view.handle_key("down", &kb());
    view.handle_key("enter", &kb()); // factory: project-builder
    view.handle_key("enter", &kb()); // workers: 2
    view.handle_key("space", &kb()); // mark the old chat
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("■ Prime · old chat") && screen.contains("Space mark"),
        "{screen}"
    );
    view.handle_key("enter", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(screen.contains("create this Captain?"), "{screen}");
    assert!(
        screen.contains("brief: Ship Gamma")
            && screen.contains("model: prime · anthropic/claude-opus-5-5 · high"),
        "{screen}"
    );
    let CaptainAction::Act { op, args, .. } = view.handle_key("y", &kb()) else {
        panic!("y creates")
    };
    assert_eq!(op, "init");
    assert_eq!(args["brief"], "Ship Gamma");
    assert_eq!(args["model"], "anthropic/claude-opus-5-5");
    assert_eq!(args["thinking"], "high");
    assert_eq!(args["parallel"], 2);
    assert_eq!(args["machine"], "project-builder");
    assert_eq!(args["crew"], json!(["/s/old.jsonl"]));
}

#[test]
fn escape_steps_back_through_the_form() {
    let mut view = view(34);
    let mut data = snapshot();
    data["current_root"] = json!("/work/gamma");
    view.apply_snapshot(&data);
    view.handle_key("p", &kb());
    let choices = json!({"models": [{"provider": "prime", "model": ""}], "thinking": ["default"], "brief": "Earlier goal"});
    let CaptainAction::Compose {
        purpose, initial, ..
    } = view.apply_choices(&choices, None)
    else {
        panic!()
    };
    assert_eq!(initial, "Earlier goal", "promote seeds the brief");
    view.compose_done(purpose, Some("Keep going".into()));
    // Esc on the model step returns to the brief in pa's editor.
    assert!(matches!(
        view.handle_key("escape", &kb()),
        CaptainAction::Compose {
            purpose: ComposePurpose::Brief,
            ..
        }
    ));
}

#[test]
fn assigning_work_starts_a_native_crew_thread_with_a_pa_model() {
    let mut view = view(34);
    assert!(matches!(
        view.handle_key("d", &kb()),
        CaptainAction::LoadChoices {
            project: Some(_),
            ..
        }
    ));
    let choices = json!({
        "crew_models": [{"label": "anthropic · claude-opus-5-5", "provider": "prime", "model": "anthropic/claude-opus-5-5"}],
        "machines": ["check-sweep"],
        "project_factories": ["proj-build"],
    });
    view.apply_choices(&choices, Some("p-alpha"));
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("thread · anthropic · claude-opus-5-5")
            && screen.contains("factory · check-sweep")
            && screen.contains("project factory · proj-build"),
        "{screen}"
    );
    view.handle_key("enter", &kb());
    view.handle_key("down", &kb());
    let CaptainAction::Compose { purpose, .. } = view.handle_key("enter", &kb()) else {
        panic!()
    };
    let CaptainAction::Act { op, args, .. } =
        view.compose_done(purpose, Some("Fix the lexer".into()))
    else {
        panic!()
    };
    assert_eq!(op, "delegate");
    assert_eq!(
        args,
        json!({"project": "p-alpha", "task": "Fix the lexer", "provider": "prime", "model": "anthropic/claude-opus-5-5", "write": true})
    );
}

#[test]
fn a_provider_thread_opens_read_and_reply_in_the_panel() {
    let mut view = view(34);
    view.open_thread(
        "p-alpha".into(),
        "c-cli".into(),
        &json!({"provider": "claude", "busy": false, "messages": [["user", "Review the parser"], ["assistant", "**Done.** No issues."]]}),
    );
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("captain › claude review  claude"),
        "{screen}"
    );
    assert!(
        screen.contains("Review the parser") && screen.contains("Done. No issues."),
        "{screen}"
    );
    let CaptainAction::Compose { purpose, label, .. } = view.handle_key("r", &kb()) else {
        panic!()
    };
    assert_eq!(label, "reply to claude review");
    assert_eq!(
        purpose,
        ComposePurpose::Message {
            project: "p-alpha".into(),
            member: Some("c-cli".into())
        }
    );
    assert_eq!(view.handle_key("escape", &kb()), CaptainAction::Refresh);
}

#[test]
fn the_crew_picker_lists_the_captain_first_and_opens_a_chat() {
    let chats = vec![
        CrewChat {
            label: "captain".into(),
            session: Some("/s/l".into()),
            member: None,
            busy: true,
        },
        CrewChat {
            label: "parser-fix".into(),
            session: Some("/s/c".into()),
            member: Some("c-fix".into()),
            busy: false,
        },
    ];
    let mut picker = CrewPicker::new("alpha".into(), "parser-fix".into(), chats.clone());
    let screen = text(&picker.render(&theme(), 64, &kb())).join("\n");
    assert!(screen.contains("switch chat  alpha"), "{screen}");
    assert!(
        screen.contains("● captain") && screen.contains("working"),
        "{screen}"
    );
    let open = screen.lines().find(|l| l.contains("parser-fix")).unwrap();
    assert!(
        open.starts_with("› · parser-fix") && open.contains("open · idle"),
        "{open}"
    );
    assert_eq!(
        picker.handle_key("enter", &kb()),
        CrewPick::Close,
        "the open chat stays"
    );
    picker.handle_key("up", &kb());
    assert_eq!(
        picker.handle_key("enter", &kb()),
        CrewPick::Open(chats[0].clone())
    );
    assert_eq!(picker.handle_key("f7", &kb()), CrewPick::Close);
}

#[test]
fn an_open_reply_maps_to_a_chat_a_thread_or_the_factory() {
    assert_eq!(
        OpenTarget::from_reply(
            &json!({"handoff": {"action": "resume", "value": "/s/a.jsonl", "panel": "factory"}})
        ),
        Some(OpenTarget::Session {
            path: "/s/a.jsonl".into(),
            factory: true,
            draft: None
        })
    );
    assert_eq!(
        OpenTarget::from_reply(
            &json!({"handoff": {"action": "thread", "value": "t1", "title": "x"}})
        ),
        Some(OpenTarget::Thread {
            id: "t1".into(),
            title: "x".into()
        })
    );
    assert_eq!(OpenTarget::from_reply(&json!({"ok": true})), None);
}

#[test]
fn a_refresh_keeps_the_selected_project_and_member() {
    let mut view = view(34);
    view.handle_key("tab", &kb());
    view.handle_key("down", &kb());
    let mut data = snapshot();
    data["projects"].as_array_mut().unwrap().reverse();
    view.apply_snapshot(&data);
    assert_eq!(view.selected_project().unwrap().id, "p-alpha");
    assert_eq!(view.selected_member().unwrap().id, "f-scan");
}

#[test]
fn a_factory_launch_picks_a_machine_and_asks_each_parameter_in_the_editor() {
    let mut view = CaptainView::new(24);
    assert!(matches!(
        view.start_launch(),
        CaptainAction::LoadChoices { project: None, .. }
    ));
    let choices = json!({
        "machines": ["check-sweep", "plain"],
        "machine_params": {
            "check-sweep": [
                {"name": "task", "type": "text", "required": true, "description": "What to review"},
                {"name": "depth", "type": "integer", "required": false, "description": ""}
            ],
            "plain": []
        }
    });
    assert_eq!(view.apply_choices(&choices, None), CaptainAction::None);
    let screen = text(&view.render(&theme(), 64, &kb())).join("\n");
    assert!(
        screen.contains("launch a factory run") && screen.contains("check-sweep"),
        "{screen}"
    );
    let CaptainAction::Compose { purpose, label, .. } = view.handle_key("enter", &kb()) else {
        panic!()
    };
    assert_eq!(label, "check-sweep · task · What to review");
    // A required parameter asks again when empty.
    let CaptainAction::Compose { purpose, .. } = view.compose_done(purpose, Some(String::new()))
    else {
        panic!()
    };
    let CaptainAction::Compose { purpose, label, .. } =
        view.compose_done(purpose, Some("Review the parser".into()))
    else {
        panic!()
    };
    assert_eq!(
        label,
        "check-sweep · depth (integer, JSON) · empty keeps the default"
    );
    assert_eq!(
        view.compose_done(purpose, Some(String::new())),
        CaptainAction::Act {
            op: "launch".into(),
            args: json!({"machine": "check-sweep", "params": {"task": "Review the parser", "depth": ""}}),
            done: String::new(),
        }
    );
    // Esc on the machine list closes the launch.
    let mut view = CaptainView::new(24);
    view.start_launch();
    view.apply_choices(&choices, None);
    assert_eq!(view.handle_key("escape", &kb()), CaptainAction::Close);
}

#[test]
fn an_action_error_survives_the_status_poll_until_the_next_key() {
    let mut view = view(34);
    view.set_error(Some("Captain: That project no longer exists.".into()));
    view.apply_snapshot(&snapshot());
    let rows = text(&view.render(&theme(), 110, &kb()));
    assert!(
        rows[rows.len() - 3].contains("That project no longer exists."),
        "{rows:#?}"
    );
    view.handle_key("down", &kb());
    let rows = text(&view.render(&theme(), 110, &kb()));
    assert!(!rows[rows.len() - 3].contains("no longer exists"));
}

#[test]
fn the_factory_step_offers_design_first_then_machines_then_none() {
    let mut view = view(34);
    let mut data = snapshot();
    data["current_root"] = json!("/work/gamma");
    data["current"] = Value::Null;
    view.apply_snapshot(&data);
    view.handle_key("p", &kb());
    let choices = json!({
        "models": [{"provider": "prime", "model": ""}],
        "thinking": ["default"],
        "machines": ["project-builder", "dual-consensus"],
        "brief": "Keep going",
    });
    let CaptainAction::Compose { purpose, .. } = view.apply_choices(&choices, None) else {
        panic!("the brief uses pa's editor")
    };
    view.compose_done(purpose, Some("Finish beta".into()));
    view.handle_key("enter", &kb()); // model
    view.handle_key("enter", &kb()); // thinking
    let screen = text(&view.render(&theme(), 110, &kb()));
    let at = |label: &str| {
        screen
            .iter()
            .position(|row| row.contains(label))
            .unwrap_or_else(|| panic!("{label} missing:\n{}", screen.join("\n")))
    };
    assert!(screen.iter().any(|row| row.contains("promote · factory")));
    assert!(
        at("Design a factory for this project") < at("project-builder")
            && at("project-builder") < at("dual-consensus")
            && at("dual-consensus") < at("No factory"),
        "{}",
        screen.join("\n")
    );
    assert!(
        screen[at("Design a factory for this project")].contains('›'),
        "design is the first choice"
    );
    view.handle_key("enter", &kb()); // factory: design
    view.handle_key("enter", &kb()); // workers
    view.handle_key("enter", &kb()); // threads
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("promote this chat to Captain?")
            && screen.contains("factory: design one for this project"),
        "{screen}"
    );
    // n returns to the threads step; Esc twice back to the factory step keeps the choice.
    view.handle_key("n", &kb());
    view.handle_key("escape", &kb());
    view.handle_key("escape", &kb());
    let screen = text(&view.render(&theme(), 110, &kb()));
    let row = screen
        .iter()
        .find(|row| row.contains("Design a factory"))
        .unwrap();
    assert!(
        row.contains('›'),
        "the earlier choice stays selected: {row}"
    );
    view.handle_key("enter", &kb());
    view.handle_key("enter", &kb());
    view.handle_key("enter", &kb());
    let CaptainAction::Act { op, args, done } = view.handle_key("y", &kb()) else {
        panic!("y promotes")
    };
    assert_eq!(op, "promote");
    assert_eq!(args["design"], json!(true));
    assert!(args.get("machine").is_none());
    assert!(done.contains("factory draft"), "{done}");
}

#[test]
fn no_factory_sends_neither_a_machine_nor_a_design() {
    let mut setup = Setup {
        factory_chosen: true,
        ..Setup::default()
    };
    assert_eq!(setup.factory_value(), Value::Null);
    assert_eq!(setup.factory_summary(), "none");
    let args = setup.request();
    assert!(args.get("design").is_none() && args.get("machine").is_none());
    setup.machine = Some("project-builder".into());
    assert_eq!(setup.request()["machine"], "project-builder");
    assert_eq!(setup.factory_summary(), "project-builder");
}

fn factory_snapshot(state: &str) -> Value {
    let mut data = snapshot();
    data["projects"][0]["factories"] = json!([{
        "name": "proj-build", "state": state, "approved": state == "changed",
        "path": "/work/alpha/.prime/machines/proj-build",
        "file": "/work/alpha/.prime/machines/proj-build/machine.toml",
        "digest": "ab".repeat(32), "errors": []
    }]);
    data
}

fn factory_show(state: &str, errors: &[&str]) -> Value {
    let valid = errors.is_empty();
    json!({
        "name": "proj-build", "state": state, "approved": state == "changed",
        "path": "/work/alpha/.prime/machines/proj-build",
        "file": "/work/alpha/.prime/machines/proj-build/machine.toml",
        "digest": "ab".repeat(32), "errors": errors, "live": false,
        "graph": if valid { json!({
            "states": [
                {"id": "plan", "entry": true, "subagent": {"prompt": "x"}},
                {"id": "test", "python": {"import": "checks", "callable": "run_tests"}}
            ],
            "transitions": [{"from": "plan", "to": "test"}]
        }) } else { Value::Null },
    })
}

#[test]
fn a_factory_draft_shows_under_needs_you_and_f_opens_its_review() {
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&factory_snapshot("draft"));
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("needs you: 1 factory draft needs review")
            && screen.contains("! proj-build  draft · needs approval")
            && screen.contains("F review"),
        "{screen}"
    );
    assert!(
        screen.contains("!3"),
        "the draft counts as needs you:\n{screen}"
    );
    assert_eq!(view.handle_key("F", &kb()), CaptainAction::None);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("project factories")
            && screen.contains("proj-build · draft · needs approval")
            && screen.contains("Design a factory for this project"),
        "{screen}"
    );
    assert_eq!(
        view.handle_key("enter", &kb()),
        CaptainAction::LoadFactory {
            project: "p-alpha".into(),
            name: "proj-build".into()
        }
    );
    view.handle_key("F", &kb());
    view.handle_key("down", &kb());
    let CaptainAction::Act { op, args, .. } = view.handle_key("enter", &kb()) else {
        panic!("design requests a Captain turn")
    };
    assert_eq!(op, "factory-design");
    assert_eq!(args["project"], "p-alpha");
    assert!(args["request_id"].as_str().is_some_and(|id| !id.is_empty()));
}

#[test]
fn the_review_page_draws_a_valid_draft_and_approves_its_digest() {
    let mut view = view(34);
    view.open_factory("p-alpha".into(), &factory_show("draft", &[]), true);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("captain › factory proj-build  ! draft · needs approval")
            && screen.contains("digest abababababab")
            && screen.contains("Python nodes run code")
            && screen.contains("│ plan")
            && screen.contains("python · checks.run_tests")
            && screen.contains("a approve · e edit · m change · D delete · Esc back"),
        "{screen}"
    );
    let CaptainAction::Act { op, args, .. } = view.handle_key("a", &kb()) else {
        panic!("a approves")
    };
    assert_eq!(op, "factory-approve");
    assert_eq!(args["digest"], "ab".repeat(32));
    assert_eq!(args["name"], "proj-build");
    assert_eq!(
        view.handle_key("e", &kb()),
        CaptainAction::EditFactory {
            root: "/work/alpha".into(),
            name: "proj-build".into()
        },
        "e opens pa's machine editor on the project factory, not $EDITOR"
    );
    let CaptainAction::Compose {
        purpose, initial, ..
    } = view.handle_key("m", &kb())
    else {
        panic!("m drafts a change request")
    };
    assert_eq!(initial, "Change the project factory proj-build: ");
    assert_eq!(
        purpose,
        ComposePurpose::Message {
            project: "p-alpha".into(),
            member: None
        }
    );
    assert_eq!(
        view.open_factory_member(),
        Some(("p-alpha".into(), "proj-build".into()))
    );
    // Delete asks first; n returns to the page, y deletes.
    view.handle_key("D", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("delete the project factory proj-build?"),
        "{screen}"
    );
    view.handle_key("n", &kb());
    assert!(
        view.open_factory_member().is_some(),
        "n keeps the review page"
    );
    view.handle_key("D", &kb());
    let CaptainAction::Act { op, .. } = view.handle_key("y", &kb()) else {
        panic!("y deletes")
    };
    assert_eq!(op, "factory-delete");
    assert!(view.open_factory_member().is_none());
}

#[test]
fn the_review_page_shows_errors_and_a_change_since_approval() {
    let mut view = view(34);
    let mut broken = factory_show("invalid", &["machine.toml: unclosed array (line 9)"]);
    broken["approved"] = json!(true);
    view.open_factory("p-alpha".into(), &broken, true);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("✗ invalid · changed since approval")
            && screen.contains("machine.toml: unclosed array (line 9)")
            && !screen.contains("Python nodes run code"),
        "{screen}"
    );
    assert_eq!(view.handle_key("a", &kb()), CaptainAction::None);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("Fix the errors before approval."),
        "{screen}"
    );

    view.open_factory("p-alpha".into(), &factory_show("changed", &[]), true);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("! changed since approval · needs approval"),
        "{screen}"
    );
    assert!(matches!(
        view.handle_key("a", &kb()),
        CaptainAction::Act { .. }
    ));

    let mut live = factory_show("approved", &[]);
    live["live"] = json!(true);
    view.open_factory("p-alpha".into(), &live, true);
    assert_eq!(view.handle_key("a", &kb()), CaptainAction::None);
    assert_eq!(view.handle_key("D", &kb()), CaptainAction::None);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("✓ approved") && screen.contains("run live") && screen.contains("is live"),
        "{screen}"
    );
    assert_eq!(view.handle_key("escape", &kb()), CaptainAction::Refresh);
    assert!(view.open_factory_member().is_none());
}

#[test]
fn a_refresh_with_a_new_digest_blocks_approval_until_r() {
    let mut view = view(34);
    view.open_factory("p-alpha".into(), &factory_show("draft", &[]), true);
    // A poll before any explicit load opens nothing elsewhere; here it refreshes the page.
    let mut newer = factory_show("draft", &[]);
    newer["digest"] = json!("cd".repeat(32));
    view.open_factory("p-alpha".into(), &newer, false);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("Changed - press r to review again"),
        "{screen}"
    );
    assert_eq!(view.handle_key("a", &kb()), CaptainAction::None);
    assert_eq!(
        view.handle_key("r", &kb()),
        CaptainAction::LoadFactory {
            project: "p-alpha".into(),
            name: "proj-build".into()
        }
    );
    view.open_factory("p-alpha".into(), &newer, true);
    let CaptainAction::Act { args, .. } = view.handle_key("a", &kb()) else {
        panic!("a approves the version reviewed with r")
    };
    assert_eq!(args["digest"], "cd".repeat(32));
    // A poll never opens the page by itself.
    let mut closed = CaptainView::new(34);
    closed.open_factory("p-alpha".into(), &newer, false);
    assert!(closed.open_factory_member().is_none());
}

#[test]
fn a_crew_state_change_raises_one_toast_and_the_first_status_raises_none() {
    let mut snapshot: CaptainSnapshot = serde_json::from_value(json!({"projects": [{
        "id": "p", "name": "alpha",
        "crew": [{"id": "c1", "kind": "thread", "title": "parser-fix", "state": "running"}],
        "factories": [{"name": "proj-build", "state": "approved", "approved": true}]
    }]}))
    .unwrap();
    let (states, toasts) = crew_toasts(None, &snapshot);
    assert!(toasts.is_empty(), "{toasts:?}");
    let (states, toasts) = crew_toasts(Some(&states), &snapshot);
    assert!(toasts.is_empty(), "no change, no toast: {toasts:?}");
    snapshot.projects[0].crew[0].state = "needs_you".into();
    snapshot.projects[0].factories[0].state = "changed".into();
    let (states, toasts) = crew_toasts(Some(&states), &snapshot);
    assert_eq!(
        toasts,
        [
            "alpha · parser-fix needs you",
            "alpha · factory proj-build needs review"
        ]
    );
    snapshot.projects[0].crew[0].state = "done".into();
    let (_, toasts) = crew_toasts(Some(&states), &snapshot);
    assert_eq!(toasts, ["alpha · parser-fix done"]);
}

#[test]
fn crew_state_text_names_each_state_briefly() {
    let member = |state: &str, run: Option<&str>, busy: bool| CaptainMember {
        state: state.into(),
        run_state: run.map(str::to_owned),
        busy,
        ..CaptainMember::default()
    };
    assert_eq!(member("needs_you", None, false).state_text(), "needs you");
    assert_eq!(member("running", None, false).state_text(), "working");
    assert_eq!(member("ready", None, true).state_text(), "working");
    assert_eq!(member("ready", None, false).state_text(), "idle");
    assert_eq!(member("done", None, false).state_text(), "done");
    assert_eq!(
        member("done", Some("stopped"), false).state_text(),
        "stopped"
    );
    assert_eq!(member("failed", None, false).state_text(), "failed");
    assert_eq!(
        member("waiting", Some("paused"), false).state_text(),
        "paused"
    );
}
