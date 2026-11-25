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
    assert!(
        all.contains("1 Overview !2  2 Digest  3 Drive  4 Factories"),
        "the right pane's tabs:\n{all}"
    );
    // Each crew member is two rows: icon, name, and kind; then state text and detail.
    let row = |name: &str| {
        let i = rows
            .iter()
            .position(|r| r.contains(name))
            .unwrap_or_else(|| panic!("{name}:\n{all}"));
        (rows[i].clone(), rows[i + 1].clone())
    };
    assert!(all.contains("Needs you (2)"), "{all}");
    assert!(
        all.contains("! parser-fix  NEEDS YOU: choose a grammar")
            && all.contains("◐ slow-scan  paused · scan"),
        "the gates lead the Overview:\n{all}"
    );
    let (first, second) = row("◐ parser-fix");
    assert!(
        first.trim_end().ends_with("! needs you · chat"),
        "orthogonal activity and needs-you badge:\n{first}"
    );
    assert!(
        second.contains("needs you · NEEDS YOU: choose a grammar"),
        "{second}"
    );
    let (first, second) = row("◐ slow-scan    ");
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
        all.contains("› ! parser-fix") && all.contains("NEEDS YOU"),
        "the focused gate is visible:\n{all}"
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
    // A Captain-started run opens its live factory page.
    view.handle_key("down", &kb());
    assert_eq!(
        view.handle_key("enter", &kb()),
        CaptainAction::OpenFactoryRun {
            project: "p-alpha".into(),
            member: "f-scan".into(),
            run_id: None,
        }
    );
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
        CaptainAction::PickModel(CaptainModelTarget::Setup {
            current_model: String::new(),
            current_thinking: String::new(),
        })
    );
    assert_eq!(
        view.apply_setup_model("anthropic/claude-opus-5-5".into(), "high".into()),
        CaptainAction::None
    );
    view.handle_key("down", &kb());
    view.handle_key("down", &kb());
    view.handle_key("enter", &kb()); // factory: none for now
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
    assert!(args.get("machine").is_none() && args.get("compose").is_none());
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
    assert_eq!(
        view.compose_done(purpose, Some("Keep going".into())),
        CaptainAction::PickModel(CaptainModelTarget::Setup {
            current_model: String::new(),
            current_thinking: String::new(),
        })
    );
    // Esc in the shared model picker returns to the brief in pa's editor.
    assert!(matches!(
        view.cancel_setup_model(),
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
        screen.contains("thread · pick model…")
            && screen.contains("thread · anthropic · claude-opus-5-5")
            && screen.contains("factory · check-sweep")
            && screen.contains("project factory · proj-build"),
        "{screen}"
    );
    // Choosing "thread · pick model…" opens pa's shared searchable model + thinking picker.
    assert_eq!(
        view.handle_key("enter", &kb()),
        CaptainAction::PickModel(CaptainModelTarget::Assign {
            project: "p-alpha".into()
        })
    );
    view.apply_assign_model(
        "p-alpha".into(),
        "anthropic/claude-opus-5-5".into(),
        "high".into(),
    );
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
        json!({"project": "p-alpha", "task": "Fix the lexer", "provider": "prime", "model": "anthropic/claude-opus-5-5", "thinking": "high", "write": true})
    );
}

#[test]
fn crew_rows_show_model_badges_and_shift_m_opens_shared_model_picker() {
    let mut data = snapshot();
    data["projects"][0]["crew"][0]["model"] = json!("anthropic/claude-sonnet-4-6");
    data["projects"][0]["crew"][0]["thinking"] = json!("high");
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&data);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("claude-sonnet-4-6 · high"),
        "crew row shows short model badge:\n{screen}"
    );
    // Focus crew and press M on the selected crew thread.
    view.handle_key("tab", &kb());
    assert_eq!(
        view.handle_key("M", &kb()),
        CaptainAction::PickModel(CaptainModelTarget::Crew {
            project: "p-alpha".into(),
            member: "c-fix".into(),
            label: "parser-fix".into(),
            current_model: "anthropic/claude-sonnet-4-6".into(),
            current_thinking: "high".into(),
        })
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
            status: "working".into(),
            kind: "captain".into(),
            ..CrewChat::default()
        },
        CrewChat {
            label: "parser-fix".into(),
            session: Some("/s/c".into()),
            member: Some("c-fix".into()),
            busy: false,
            status: "idle".into(),
            kind: "chat".into(),
            ..CrewChat::default()
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
fn the_factory_step_offers_design_then_chart_then_none() {
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
    assert_eq!(
        view.compose_done(purpose, Some("Finish beta".into())),
        CaptainAction::PickModel(CaptainModelTarget::Setup {
            current_model: String::new(),
            current_thinking: String::new(),
        })
    );
    view.apply_setup_model(String::new(), String::new());
    let screen = text(&view.render(&theme(), 110, &kb()));
    let at = |label: &str| {
        screen
            .iter()
            .position(|row| row.contains(label))
            .unwrap_or_else(|| panic!("{label} missing:\n{}", screen.join("\n")))
    };
    assert!(screen.iter().any(|row| row.contains("promote · factory")));
    assert!(
        at("Design one project FSM") < at("Build custom FSM from chart")
            && at("Build custom FSM from chart") < at("None for now (Captain + crew only)"),
        "{}",
        screen.join("\n")
    );
    assert!(
        !screen
            .iter()
            .any(|row| row.contains("dual-consensus") || row.contains("project-builder")),
        "library utilities are stages, not project choices:\n{}",
        screen.join("\n")
    );
    assert!(
        screen[at("Design one project FSM")].contains('›'),
        "design is the default choice"
    );
    view.handle_key("enter", &kb()); // factory: design
    view.handle_key("enter", &kb()); // factory models: same as the Captain
    view.handle_key("enter", &kb()); // workers
    view.handle_key("enter", &kb()); // threads
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("promote this chat to Captain?")
            && screen.contains("factory: Captain designs one project FSM")
            && screen.contains("factory models: same as the Captain"),
        "{screen}"
    );
    // n returns to the threads step; Esc back through workers and factory models to the
    // factory step keeps the choice.
    view.handle_key("n", &kb());
    view.handle_key("escape", &kb());
    view.handle_key("escape", &kb());
    view.handle_key("escape", &kb());
    let screen = text(&view.render(&theme(), 110, &kb()));
    let row = screen
        .iter()
        .find(|row| row.contains("Design one project FSM"))
        .unwrap();
    assert!(
        row.contains('›'),
        "the earlier choice stays selected: {row}"
    );
    view.handle_key("enter", &kb()); // factory
    view.handle_key("enter", &kb()); // factory models
    view.handle_key("enter", &kb()); // workers
    view.handle_key("enter", &kb()); // threads
    let CaptainAction::Act { op, args, done } = view.handle_key("y", &kb()) else {
        panic!("y promotes")
    };
    assert_eq!(op, "promote");
    assert_eq!(args["design"], json!(true));
    assert_eq!(args["factory_models"], json!({"mode": "captain"}));
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
    assert_eq!(
        setup.factory_summary(),
        "none for now (Captain + crew only)"
    );
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
        screen.contains("Needs you (3)")
            && screen.contains("! factory proj-build  draft · needs approval")
            && screen.contains("4 Factories !1"),
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
        screen.contains("machine: proj-build")
            && screen.contains("! draft · needs approval")
            && screen.contains("digest abababababab")
            && screen.contains("Python nodes run code")
            && screen.contains("│ plan")
            && screen.contains("python · checks.run_tests")
            && screen.contains(
                "↑/↓ node · Enter files · m/M model · + stage · x remove · b chart · a approve"
            ),
        "{screen}"
    );
    // m on the selected agent node opens the shared model picker.
    assert_eq!(
        view.handle_key("m", &kb()),
        CaptainAction::PickModel(CaptainModelTarget::FactoryNode {
            project: "p-alpha".into(),
            name: "proj-build".into(),
            state_id: "plan".into(),
            role: None,
            current_model: String::new(),
            current_thinking: String::new(),
        })
    );
    // ↑/↓ steps nodes in visual order and Enter opens the node detail view; Esc returns.
    view.handle_key("down", &kb());
    // m on a python node explains that Python states have no model.
    assert_eq!(view.handle_key("m", &kb()), CaptainAction::None);
    assert!(text(&view.render(&theme(), 110, &kb()))
        .join("\n")
        .contains("Python state has no model"));
    view.handle_key("enter", &kb());
    let detail_screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        detail_screen.contains("node: test") && detail_screen.contains("Esc back"),
        "{detail_screen}"
    );
    view.handle_key("escape", &kb());
    assert!(
        view.open_factory_member().is_some(),
        "Esc from node detail returns to the review diagram"
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
    } = view.handle_key("c", &kb())
    else {
        panic!("c drafts a change request")
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

#[test]
fn the_review_page_lists_vendored_modules_and_specialists_and_u_updates_drift() {
    let mut view = view(34);
    let mut show = factory_show("approved", &[]);
    show["approved"] = json!(true);
    show["modules"] = json!([
        {"alias": "review", "module": "review-triad", "version": 1, "latest": 1, "state": "current"},
        {"alias": "checks", "module": "validate", "version": 1, "latest": 2, "state": "update"}
    ]);
    show["specialists"] = json!([{"id": "theory", "states": ["plan"], "state": "changed"}]);
    view.open_factory("p-alpha".into(), &show, true);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("module review  review-triad v1 · current")
            && screen.contains("module checks  validate v1 · update to v2 - u")
            && screen.contains("specialist theory  plan · specialist changed - update - u")
            && screen.contains("m model · M default · u update · a approve"),
        "{screen}"
    );
    // The specialist update comes first; it is an edit, so the version needs approval again.
    let CaptainAction::Act { op, args, .. } = view.handle_key("u", &kb()) else {
        panic!("u updates the drifted specialist")
    };
    assert_eq!(op, "factory-update-specialist");
    assert_eq!(args["specialist"], "theory");
    assert_eq!(args["name"], "proj-build");
    show["specialists"] = json!([{"id": "theory", "states": ["plan"], "state": "current"}]);
    view.open_factory("p-alpha".into(), &show, true);
    let CaptainAction::Act { op, args, .. } = view.handle_key("u", &kb()) else {
        panic!("u updates the module")
    };
    assert_eq!(op, "factory-update-module");
    assert_eq!(args["alias"], "checks");
    show["modules"] = json!([]);
    view.open_factory("p-alpha".into(), &show, true);
    assert_eq!(view.handle_key("u", &kb()), CaptainAction::None);
    // Narrow panes keep each row on one line.
    let narrow = text(&view.render(&theme(), 64, &kb()));
    assert!(
        narrow.iter().all(|row| crate::width::str_width(row) <= 64),
        "{narrow:?}"
    );
}

#[test]
fn the_panel_lists_specialists_and_drift_and_s_opens_one_in_the_viewer() {
    let mut data = factory_snapshot("approved");
    data["projects"][0]["factories"][0]["drift"] = json!("specialist changed - update");
    data["projects"][0]["specialists"] = json!([
        {"id": "theory", "name": "ML theory verifier", "description": "Checks proofs and bounds.",
         "model": "anthropic/claude-opus-4-6", "thinking": "high",
         "prompt": "Verify every bound you rely on.", "path": "/work/alpha/.prime/agents/theory.md"},
        {"id": "data-audit", "name": "Data auditor", "description": "", "model": "", "thinking": "",
         "prompt": "Check the splits.", "path": "/work/alpha/.prime/agents/data-audit.md"}
    ]);
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&data);
    view.handle_key("4", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("proj-build") && screen.contains("approved · specialist changed - update"),
        "{screen}"
    );
    view.handle_key("3", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("specialists/theory.md")
            && screen.contains("claude-opus-4-6 · high")
            && screen.contains("specialists/data-audit.md"),
        "{screen}"
    );
    view.handle_key("1", &kb());
    assert_eq!(view.handle_key("S", &kb()), CaptainAction::None);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("theory · claude-opus-4-6 · high"),
        "{screen}"
    );
    assert_eq!(view.handle_key("enter", &kb()), CaptainAction::None);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("specialist › ML theory verifier  theory")
            && screen.contains("anthropic/claude-opus-4-6 · high")
            && screen.contains("Checks proofs and bounds.")
            && screen.contains("Verify every bound you rely on.")
            && screen.contains("m model · e edit · D delete · Esc back"),
        "{screen}"
    );
    view.handle_key("escape", &kb());
    assert!(text(&view.render(&theme(), 110, &kb()))
        .join("\n")
        .contains("1 Overview"));
}

#[test]
fn a_designed_factory_asks_for_its_models_after_the_factory_step() {
    let mut view = view(34);
    let mut data = snapshot();
    data["current_root"] = json!("/work/gamma");
    data["current"] = Value::Null;
    view.apply_snapshot(&data);
    view.handle_key("n", &kb());
    let choices = json!({
        "models": [{"provider": "prime", "model": ""}],
        "thinking": ["default"],
        "machines": ["project-builder"],
    });
    let CaptainAction::Compose { purpose, .. } = view.apply_choices(&choices, None) else {
        panic!()
    };
    view.compose_done(purpose, Some("Ship Gamma".into()));
    view.apply_setup_model("anthropic/claude-opus-5-5".into(), String::new());
    view.handle_key("enter", &kb()); // factory: design one
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("factory models")
            && screen.contains("Same as the Captain")
            && screen.contains("Pick one model for every step")
            && screen.contains("Captain chooses per step"),
        "{screen}"
    );
    // Esc goes back to the factory step; Enter returns here.
    view.handle_key("escape", &kb());
    view.handle_key("enter", &kb());
    view.handle_key("down", &kb());
    assert_eq!(
        view.handle_key("enter", &kb()),
        CaptainAction::PickModel(CaptainModelTarget::SetupFactory {
            current_model: String::new(),
            current_thinking: String::new(),
        }),
        "one model for every step opens the shared picker"
    );
    view.apply_setup_factory_model("openai-codex/gpt-5.4".into(), "high".into());
    view.handle_key("enter", &kb()); // workers
    view.handle_key("enter", &kb()); // threads
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("factory models: openai-codex/gpt-5.4 · high"),
        "{screen}"
    );
    let CaptainAction::Act { args, .. } = view.handle_key("y", &kb()) else {
        panic!("y creates")
    };
    assert_eq!(args["design"], true);
    assert_eq!(
        args["factory_models"],
        json!({"mode": "model", "model": "openai-codex/gpt-5.4", "thinking": "high"})
    );
}

#[test]
fn the_router_factory_models_choice_is_sent_as_router_mode() {
    let mut view = view(34);
    let mut data = snapshot();
    data["current_root"] = json!("/work/gamma");
    data["current"] = Value::Null;
    view.apply_snapshot(&data);
    view.handle_key("n", &kb());
    let choices = json!({
        "models": [{"provider": "prime", "model": ""}],
        "thinking": ["default"],
        "machines": ["project-builder"],
    });
    let CaptainAction::Compose { purpose, .. } = view.apply_choices(&choices, None) else {
        panic!()
    };
    view.compose_done(purpose, Some("Ship Gamma".into()));
    view.apply_setup_model("anthropic/claude-opus-5-5".into(), String::new());
    view.handle_key("enter", &kb()); // factory: design one
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("\u{21c4} Router chooses per step"),
        "{screen}"
    );
    for _ in 0..3 {
        view.handle_key("down", &kb());
    }
    view.handle_key("enter", &kb()); // router
    view.handle_key("enter", &kb()); // workers
    view.handle_key("enter", &kb()); // threads
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("factory models: \u{21c4} router chooses per step"),
        "{screen}"
    );
    let CaptainAction::Act { args, .. } = view.handle_key("y", &kb()) else {
        panic!("y creates")
    };
    assert_eq!(args["factory_models"], json!({"mode": "router"}));
}

#[test]
fn the_factory_review_shows_the_resolved_default_and_m_changes_it() {
    let mut view = view(40);
    let data = json!({
        "name": "proj-build", "state": "approved", "approved": true, "digest": "a".repeat(64),
        "inherit_model": {"model": "anthropic/claude-opus-5-5", "thinking": ""},
        "graph": {"states": [
            {"id": "build", "entry": true, "subagent": {}},
            {"id": "check", "subagent": {"role": "default", "model": "openai-codex/gpt-5.4"}},
        ], "transitions": [{"from": ["build"], "to": "check"}]},
    });
    view.open_factory("p-alpha".into(), &data, true);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(screen.contains("│ inherit (claude-opus-5-5)"), "{screen}");
    assert!(
        screen.contains("default model: inherit (claude-opus-5-5)"),
        "{screen}"
    );
    assert_eq!(
        view.handle_key("M", &kb()),
        CaptainAction::PickModel(CaptainModelTarget::FactoryDefault {
            project: "p-alpha".into(),
            name: "proj-build".into(),
            current_model: "anthropic/claude-opus-5-5".into(),
            current_thinking: String::new(),
        })
    );
    let mut with_role = data.clone();
    with_role["default_role"] = json!({"model": "openai-codex/gpt-5.4", "thinking": "high"});
    view.open_factory("p-alpha".into(), &with_role, true);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("default model: openai-codex/gpt-5.4 · high (role default)"),
        "{screen}"
    );
}

/// The alpha snapshot with the backend's unified needs-you list.
fn gated_snapshot() -> Value {
    let mut data = factory_snapshot("draft");
    data["projects"][0]["crew"][0]["worktree_missing"] = json!(true);
    data["projects"][0]["crew"][2]["run_id"] = json!("run-7");
    data["projects"][0]["needs_you"] = json!([
        {"kind": "worktree_missing", "id": "c-fix", "member": "c-fix", "label": "parser-fix",
         "detail": "Worktree missing at /wt/c-fix"},
        {"kind": "paused_run", "id": "f-scan", "member": "f-scan", "label": "slow-scan",
         "state": "paused", "step": "scan", "detail": "check failed"},
        {"kind": "draft_factory", "id": "p-alpha/proj-build", "factory": "proj-build",
         "label": "factory proj-build", "digest": "ab".repeat(32)},
        {"kind": "pending_grant", "id": "g1", "grant_id": "g1", "machine": "feature-implementer",
         "label": "grant · feature-implementer", "detail": "Implement the lexer"}
    ]);
    data
}

fn select_need(view: &mut CaptainView, index: usize) {
    view.handle_key("tab", &kb());
    for _ in 0..index {
        view.handle_key("down", &kb());
    }
}

#[test]
fn the_unified_needs_you_list_matches_the_rail_and_takes_one_key_actions() {
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&gated_snapshot());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("Needs you (4)") && screen.contains("!4 ●2"),
        "the rail counts exactly the listed gates:\n{screen}"
    );
    assert!(
        screen.contains("✗ parser-fix  Worktree missing at /wt/c-fix")
            && screen.contains("◐ slow-scan  paused · scan · check failed")
            && screen.contains("! factory proj-build  draft · needs approval")
            && screen.contains("◆ grant · feature-implementer  Implement the lexer"),
        "{screen}"
    );
    // A missing worktree: r recreates it.
    select_need(&mut view, 0);
    assert_eq!(
        view.handle_key("r", &kb()),
        CaptainAction::Act {
            op: "worktree-recreate".into(),
            args: json!({"project": "p-alpha", "member": "c-fix"}),
            done: "Recreated the worktree of parser-fix.".into(),
        }
    );
    // A pending writer machine: a approves once, g grants standing, x declines.
    view.handle_key("down", &kb());
    view.handle_key("down", &kb());
    view.handle_key("down", &kb());
    let footer = text(&view.render(&theme(), 110, &kb())).pop().unwrap();
    assert!(
        footer.contains("a approve once · g grant · x decline"),
        "{footer}"
    );
    let CaptainAction::Act { op, args, .. } = view.handle_key("a", &kb()) else {
        panic!("a approves")
    };
    assert_eq!(op, "grant-approve");
    assert_eq!(
        args,
        json!({"project": "p-alpha", "grant_id": "g1", "standing": false})
    );
    let CaptainAction::Act { op, args, .. } = view.handle_key("g", &kb()) else {
        panic!("g grants")
    };
    assert_eq!(
        (op.as_str(), &args["standing"]),
        ("grant-approve", &json!(true))
    );
    let CaptainAction::Act { op, args, .. } = view.handle_key("x", &kb()) else {
        panic!("x declines")
    };
    assert_eq!(op, "grant-decline");
    assert_eq!(args, json!({"project": "p-alpha", "grant_id": "g1"}));
}

#[test]
fn a_draft_factory_gate_reviews_first_and_approves_the_reviewed_digest() {
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&gated_snapshot());
    select_need(&mut view, 2);
    let load = CaptainAction::LoadFactory {
        project: "p-alpha".into(),
        name: "proj-build".into(),
    };
    assert_eq!(view.handle_key("enter", &kb()), load);
    assert_eq!(
        view.handle_key("a", &kb()),
        load,
        "unreviewed: a opens the review"
    );
    view.open_factory("p-alpha".into(), &factory_show("draft", &[]), true);
    view.handle_key("escape", &kb());
    assert_eq!(
        view.handle_key("a", &kb()),
        CaptainAction::Act {
            op: "factory-approve".into(),
            args: json!({"project": "p-alpha", "name": "proj-build", "digest": "ab".repeat(32)}),
            done: "Approved this version of proj-build.".into(),
        },
        "the reviewed digest approves in one key"
    );
}

#[test]
fn a_paused_run_gate_inspects_retries_answers_and_stops() {
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&gated_snapshot());
    select_need(&mut view, 1);
    assert_eq!(
        view.handle_key("enter", &kb()),
        CaptainAction::OpenFactoryRun {
            project: "p-alpha".into(),
            member: "f-scan".into(),
            run_id: Some("run-7".into()),
        }
    );
    let CaptainAction::Act { op, args, .. } = view.handle_key("r", &kb()) else {
        panic!("r retries")
    };
    assert_eq!(op, "answer");
    assert_eq!(
        args,
        json!({"project": "p-alpha", "member": "f-scan", "decision": "resume"})
    );
    let CaptainAction::Act { args, .. } = view.handle_key("s", &kb()) else {
        panic!("s stops the run, not the Captain")
    };
    assert_eq!(args["decision"], "stop");
    assert!(matches!(
        view.handle_key("m", &kb()),
        CaptainAction::Compose {
            purpose: ComposePurpose::Answer { .. },
            ..
        }
    ));
}

#[test]
fn number_keys_and_brackets_switch_the_right_pane_tabs() {
    let mut view = view(34);
    assert_eq!(view.tab, Tab::Overview);
    view.handle_key("2", &kb());
    assert_eq!(view.tab, Tab::Digest);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("DIGEST.md") && screen.contains("pick a parser library"),
        "{screen}"
    );
    view.handle_key("]", &kb());
    assert_eq!(view.tab, Tab::Drive);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("AGENTS.md")
            && screen.contains("CREW.md")
            && screen.contains("NOTES.md")
            && screen.contains("Ask before deleting files."),
        "the Drive lists the Captain's files and previews the first:\n{screen}"
    );
    view.handle_key("]", &kb());
    assert_eq!(view.tab, Tab::Factories);
    view.handle_key("]", &kb());
    assert_eq!(view.tab, Tab::Overview, "] wraps");
    view.handle_key("[", &kb());
    assert_eq!(view.tab, Tab::Factories);
    view.handle_key("1", &kb());
    assert_eq!(view.tab, Tab::Overview);
}

#[test]
fn the_overview_shows_needs_you_and_the_whole_crew_at_120_by_36_without_tab() {
    let mut data = gated_snapshot();
    let crew = data["projects"][0]["crew"].as_array_mut().unwrap();
    for n in 0..6 {
        crew.push(
            json!({"id": format!("c{n}"), "kind": "thread", "title": format!("worker-{n}"),
            "state": "ready", "session": format!("/s/w{n}.jsonl")}),
        );
    }
    // The session sizes the panel from the terminal: 36 rows give it 34.
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&data);
    let rows = text(&view.render(&theme(), 120, &kb()));
    let all = rows.join("\n");
    assert_eq!(rows.len(), 34);
    assert!(all.contains("Needs you (4)"), "{all}");
    for name in ["parser-fix", "claude review", "slow-scan"]
        .into_iter()
        .map(str::to_owned)
        .chain((0..6).map(|n| format!("worker-{n}")))
    {
        assert!(
            rows.iter()
                .any(|r| r.contains(&format!(" {name} "))
                    || r.trim_end().ends_with(&format!(" {name}"))),
            "{name} is visible on open:\n{all}"
        );
    }
}

#[test]
fn the_drive_edits_a_file_and_restores_an_earlier_version_from_its_history() {
    let mut view = view(34);
    view.handle_key("3", &kb());
    assert!(matches!(
        view.handle_key("e", &kb()),
        CaptainAction::Compose {
            purpose: ComposePurpose::EditText { ref field, .. },
            ref initial,
            ..
        } if field == "rules" && initial == "Ask before deleting files."
    ));
    view.handle_key("down", &kb());
    assert_eq!(
        view.handle_key("h", &kb()),
        CaptainAction::LoadHistory {
            project: "p-alpha".into(),
            field: "crew_rules".into()
        }
    );
    view.open_history(
        "p-alpha".into(),
        "crew_rules".into(),
        &json!({"edits": [
            {"id": 9, "field": "crew_rules", "old_len": 0, "new_len": 15, "actor": "captain", "stamp": 1.0, "restorable": false},
            {"id": 4, "field": "crew_rules", "old_len": 20, "new_len": 0, "actor": "user", "stamp": 1.0, "restorable": true}
        ]}),
    );
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("history › CREW.md") && screen.contains("20 → 0 chars"),
        "{screen}"
    );
    assert_eq!(
        view.handle_key("r", &kb()),
        CaptainAction::None,
        "nothing kept"
    );
    view.handle_key("down", &kb());
    assert_eq!(
        view.handle_key("r", &kb()),
        CaptainAction::Act {
            op: "edit-restore".into(),
            args: json!({"project": "p-alpha", "edit_id": 4}),
            done: "Restored the crew rules before that edit.".into(),
        }
    );
    assert_eq!(view.tab, Tab::Drive, "the tab stays after the restore");
}

#[test]
fn the_factories_tab_reviews_designs_approves_and_deletes() {
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&factory_snapshot("draft"));
    view.handle_key("4", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("! proj-build") && screen.contains("draft · needs approval"),
        "{screen}"
    );
    assert!(matches!(
        view.handle_key("enter", &kb()),
        CaptainAction::LoadFactory { .. }
    ));
    let CaptainAction::Act { op, args, .. } = view.handle_key("n", &kb()) else {
        panic!("n requests a design")
    };
    assert_eq!(op, "factory-design");
    let request = args["request_id"].clone();
    let CaptainAction::Act { args, .. } = view.handle_key("n", &kb()) else {
        panic!()
    };
    assert_eq!(args["request_id"], request, "one request per visit");
    view.handle_key("D", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("delete the project factory proj-build?"),
        "{screen}"
    );
    assert_eq!(view.handle_key("n", &kb()), CaptainAction::None);
    assert_eq!(view.tab, Tab::Factories, "cancel returns to the tab");
    view.handle_key("D", &kb());
    let CaptainAction::Act { op, .. } = view.handle_key("y", &kb()) else {
        panic!()
    };
    assert_eq!(op, "factory-delete");
}

fn specialist_view() -> CaptainView {
    let mut data = snapshot();
    data["projects"][0]["specialists"] = json!([
        {"id": "theory", "name": "ML theory verifier", "description": "Checks proofs.",
         "model": "anthropic/claude-opus-4-6", "thinking": "high",
         "prompt": "Verify every bound.", "path": "/work/alpha/.prime/agents/theory.md"}
    ]);
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&data);
    view.handle_key("S", &kb());
    view.handle_key("enter", &kb());
    view
}

#[test]
fn the_specialist_view_changes_the_model_edits_and_deletes() {
    let mut view = specialist_view();
    let CaptainAction::PickModel(CaptainModelTarget::Crew { member, .. }) =
        view.handle_key("m", &kb())
    else {
        panic!("m opens the shared model picker")
    };
    assert_eq!(member, format!("{SPECIALIST_MEMBER}theory"));
    let CaptainAction::Act { op, args, .. } =
        view.apply_specialist_model("theory", "openai/gpt-5.4".into(), false, String::new())
    else {
        panic!("the pick writes the specialist")
    };
    assert_eq!(op, "specialist-write");
    assert_eq!(
        args,
        json!({"project": "p-alpha", "id": "theory", "name": "ML theory verifier",
            "description": "Checks proofs.", "model": "openai/gpt-5.4", "thinking": "high",
            "prompt": "Verify every bound."})
    );
    // e asks which field, then edits it in pa's editor.
    assert_eq!(view.handle_key("e", &kb()), CaptainAction::None);
    view.handle_key("down", &kb());
    let CaptainAction::Compose {
        purpose, initial, ..
    } = view.handle_key("enter", &kb())
    else {
        panic!()
    };
    assert_eq!(initial, "Checks proofs.");
    let CaptainAction::Act { op, args, .. } =
        view.compose_done(purpose, Some("Checks proofs and bounds.".into()))
    else {
        panic!()
    };
    assert_eq!(op, "specialist-write");
    assert_eq!(args["description"], "Checks proofs and bounds.");
    assert_eq!(args["prompt"], "Verify every bound.");
    // D asks first; n returns to the specialist.
    view.handle_key("D", &kb());
    assert!(text(&view.render(&theme(), 110, &kb()))
        .join("\n")
        .contains("delete the specialist theory?"));
    view.handle_key("n", &kb());
    assert!(text(&view.render(&theme(), 110, &kb()))
        .join("\n")
        .contains("specialist › ML theory verifier"));
    view.handle_key("D", &kb());
    assert_eq!(
        view.handle_key("y", &kb()),
        CaptainAction::Act {
            op: "specialist-delete".into(),
            args: json!({"project": "p-alpha", "id": "theory"}),
            done: "Deleted the specialist theory.".into(),
        }
    );
}

#[test]
fn m_on_a_shared_role_asks_role_or_state_before_the_model_picker() {
    let mut view = view(34);
    let mut show = factory_show("approved", &[]);
    show["state_details"] = json!({
        "plan": {"kind": "agent", "role": "reviewer", "model": "anthropic/claude-opus-4-6"},
        "review": {"kind": "agent", "role": "reviewer"},
        "test": {"kind": "python"}
    });
    view.open_factory("p-alpha".into(), &show, true);
    assert_eq!(view.handle_key("m", &kb()), CaptainAction::None);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("Change role 'reviewer' (2 states)")
            && screen.contains("Override only 'plan'"),
        "{screen}"
    );
    view.handle_key("down", &kb());
    let CaptainAction::PickModel(CaptainModelTarget::FactoryNode { role, state_id, .. }) =
        view.handle_key("enter", &kb())
    else {
        panic!("the choice opens the model picker")
    };
    assert_eq!((state_id.as_str(), role), ("plan", None), "only this state");
    view.handle_key("m", &kb());
    let CaptainAction::PickModel(CaptainModelTarget::FactoryNode { role, .. }) =
        view.handle_key("enter", &kb())
    else {
        panic!()
    };
    assert_eq!(role.as_deref(), Some("reviewer"), "the shared role");
    view.handle_key("m", &kb());
    view.handle_key("escape", &kb());
    assert!(
        view.open_factory_member().is_some(),
        "Esc returns to the review page"
    );
}

#[test]
fn crew_rows_carry_activity_needs_you_and_right_aligned_metadata() {
    let mut data = snapshot();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    data["projects"][0]["crew"][1] = json!({"id": "c-cli", "kind": "thread", "title": "claude review",
        "provider": "claude", "model": "anthropic/claude-sonnet-4-6", "state": "running",
        "activity_state": "working", "diff_added": 19, "diff_deleted": 7,
        "updated_at": now - 7200.0, "session": "/s/cli.jsonl"});
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&data);
    view.set_subagents([("/s/cli.jsonl".to_owned(), 2)].into_iter().collect());
    let rows = text(&view.render(&theme(), 120, &kb()));
    let row = rows.iter().find(|r| r.contains("● claude review")).unwrap();
    assert!(
        row.trim_end()
            .ends_with("claude-sonnet-4-6 · +19 -7 · ◆2 · 2h · chat"),
        "{row}"
    );
    // → on a crew row with subagents opens them.
    view.handle_key("tab", &kb());
    while view.selected_member().map(|m| m.id) != Some("c-cli".into()) {
        view.handle_key("down", &kb());
    }
    assert_eq!(
        view.handle_key("right", &kb()),
        CaptainAction::OpenSubagents {
            session: "/s/cli.jsonl".into()
        }
    );
    assert_eq!(
        view.handle_key("A", &kb()),
        CaptainAction::Act {
            op: "archive".into(),
            args: json!({"project": "p-alpha", "member": "c-cli"}),
            done: "claude review archived.".into(),
        }
    );
}

#[test]
fn archived_crew_hides_until_h_shows_it_and_a_unarchives() {
    let mut data = snapshot();
    data["projects"][0]["crew"][1]["archived"] = json!(true);
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&data);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        !screen.contains("claude review") && screen.contains("1 archived · H show"),
        "{screen}"
    );
    view.handle_key("H", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("archived  1") && screen.contains("claude review"),
        "{screen}"
    );
    view.handle_key("tab", &kb());
    while view.selected_member().map(|m| m.id) != Some("c-cli".into()) {
        view.handle_key("down", &kb());
    }
    let CaptainAction::Act { op, .. } = view.handle_key("A", &kb()) else {
        panic!()
    };
    assert_eq!(op, "unarchive");
}

/// The snapshot with alpha (this checkout's Captain) archived.
fn archived_alpha() -> Value {
    let mut data = snapshot();
    data["projects"][0]["state"] = json!("archived");
    data["projects"][0]["lead_busy"] = json!(false);
    data
}

#[test]
fn an_archived_captain_frees_its_checkout_for_a_new_one() {
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&archived_alpha());
    assert!(!view.here_has_captain());
    assert_eq!(
        view.handle_key("n", &kb()),
        CaptainAction::LoadChoices {
            project: None,
            reference: None
        }
    );
    // Archiving does not free the checkout until it settles.
    let mut data = snapshot();
    data["projects"][0]["state"] = json!("archiving");
    view.apply_snapshot(&data);
    assert!(view.here_has_captain());
}

#[test]
fn archived_captains_hide_until_h_shows_them() {
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&archived_alpha());
    // The first status selects the live project, not the archived one at this checkout.
    assert_eq!(
        view.selected_project().map(|p| p.id.as_str()),
        Some("p-beta")
    );
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("1 project · 1 archived"),
        "the header counts both:\n{screen}"
    );
    assert!(
        !screen.contains("■ alpha") && screen.contains("1 archived · H show"),
        "{screen}"
    );
    view.handle_key("down", &kb());
    assert_eq!(
        view.selected_project().map(|p| p.id.as_str()),
        Some("p-beta")
    );

    view.handle_key("H", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("■ alpha") && !screen.contains("H show"),
        "{screen}"
    );
    let beta = screen.find("■ beta").unwrap();
    let alpha = screen.find("■ alpha").unwrap();
    assert!(beta < alpha, "archived Captains list last:\n{screen}");
    view.handle_key("down", &kb());
    assert_eq!(
        view.selected_project().map(|p| p.id.as_str()),
        Some("p-alpha")
    );
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("alpha  archived")
            && screen.contains("Archived Captain. Press A or R to unarchive"),
        "the Overview marks it archived:\n{screen}"
    );

    // Hiding again moves the selection off the hidden Captain.
    view.handle_key("H", &kb());
    assert_eq!(
        view.selected_project().map(|p| p.id.as_str()),
        Some("p-beta")
    );
}

#[test]
fn only_archived_captains_leave_an_empty_list_with_the_hint() {
    let mut data = archived_alpha();
    data["projects"].as_array_mut().unwrap().pop();
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&data);
    assert!(view.selected_project().is_none());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("No active projects") && screen.contains("1 archived · H show"),
        "{screen}"
    );
    view.handle_key("H", &kb());
    assert_eq!(
        view.selected_project().map(|p| p.id.as_str()),
        Some("p-alpha")
    );
}

#[test]
fn shift_a_archives_and_unarchives_the_selected_captain() {
    let archive = |project: &str| CaptainAction::Act {
        op: "archive-project".into(),
        args: json!({"project": project}),
        done: "Captain archived. H shows archived Captains.".into(),
    };
    let unarchive = |project: &str| CaptainAction::Act {
        op: "unarchive-project".into(),
        args: json!({"project": project}),
        done: "Captain unarchived.".into(),
    };
    // Paused beta has no running work: it archives at once.
    let mut panel = view(34);
    panel.handle_key("down", &kb());
    assert_eq!(panel.handle_key("A", &kb()), archive("p-beta"));

    // Active alpha has running work: confirm first, as for stop and delete.
    let mut view = view(34);
    assert_eq!(view.handle_key("A", &kb()), CaptainAction::None);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("archive the Captain of alpha?")
            && screen.contains("running turn(s) or run(s) stop first."),
        "{screen}"
    );
    assert_eq!(view.handle_key("n", &kb()), CaptainAction::None);
    view.handle_key("A", &kb());
    assert_eq!(view.handle_key("y", &kb()), archive("p-alpha"));

    // An archived Captain: A and R unarchive it.
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&archived_alpha());
    view.handle_key("H", &kb());
    view.handle_key("down", &kb());
    assert_eq!(view.handle_key("A", &kb()), unarchive("p-alpha"));
    assert_eq!(view.handle_key("shift+a", &kb()), unarchive("p-alpha"));
    assert_eq!(view.handle_key("R", &kb()), unarchive("p-alpha"));
    // New work names the unarchive step.
    assert_eq!(view.handle_key("m", &kb()), CaptainAction::None);
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("Unarchive this Captain to send messages."),
        "{screen}"
    );
}

#[test]
fn focusing_an_archived_captain_by_name_shows_it() {
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&archived_alpha());
    assert!(view.focus_project("alpha"));
    assert_eq!(
        view.selected_project().map(|p| p.id.as_str()),
        Some("p-alpha")
    );
}

#[test]
fn a_thread_opened_from_a_chat_closes_back_to_it() {
    let mut view = CaptainView::new(34);
    view.set_transient();
    view.begin_thread("p-alpha".into(), "c-cli".into(), "claude review".into());
    view.apply_snapshot(&snapshot());
    view.open_thread(
        "p-alpha".into(),
        "c-cli".into(),
        &json!({"provider": "claude", "messages": [["user", "Review"]]}),
    );
    let rows = text(&view.render(&theme(), 110, &kb()));
    assert!(rows.join("\n").contains("captain › claude review"));
    assert!(rows.last().unwrap().contains("Esc back to chat"));
    assert_eq!(view.handle_key("escape", &kb()), CaptainAction::Close);
    assert!(view.is_transient(), "its close is not parked");
}

#[test]
fn parking_keeps_the_tab_and_selection_but_drops_sub_pages() {
    let mut view = view(34);
    view.handle_key("tab", &kb());
    view.handle_key("down", &kb());
    view.handle_key("2", &kb());
    view.handle_key("?", &kb());
    view.park();
    assert_eq!(view.tab, Tab::Digest);
    assert!(!view.help);
    view.handle_key("1", &kb());
    assert_eq!(view.selected_member().unwrap().id, "f-scan");
}

#[test]
fn the_crew_picker_shows_each_status_and_model() {
    let chat = |label: &str, status: &str, kind: &str| CrewChat {
        label: label.into(),
        member: Some(label.into()),
        status: status.into(),
        kind: kind.into(),
        model: Some("claude-sonnet-4-6".into()),
        ..CrewChat::default()
    };
    let picker = CrewPicker::new(
        "alpha".into(),
        "captain".into(),
        vec![
            chat("a", "needs_you", "chat"),
            chat("b", "working", "claude"),
            chat("c", "paused", "factory"),
            chat("d", "failed", "cursor"),
            chat("e", "idle", "codex"),
        ],
    );
    let screen = text(&picker.render(&theme(), 80, &kb())).join("\n");
    for row in [
        "! a",
        "● b",
        "◐ c",
        "✗ d",
        "· e",
        "claude-sonnet-4-6 · claude · working",
        "factory run · paused",
        "cursor · failed",
        "needs you",
    ] {
        assert!(screen.contains(row), "{row}:\n{screen}");
    }
}

#[test]
fn ages_read_briefly() {
    assert_eq!(age_label(5.0), "now");
    assert_eq!(age_label(300.0), "5m");
    assert_eq!(age_label(7300.0), "2h");
    assert_eq!(age_label(3.0 * 86_400.0), "3d");
}

#[test]
fn the_crew_picker_opens_a_new_thread_or_adds_an_existing_one() {
    let chats = vec![CrewChat {
        label: "captain".into(),
        session: Some("/s/l".into()),
        kind: "captain".into(),
        ..CrewChat::default()
    }];
    let mut picker = CrewPicker::new("alpha".into(), "captain".into(), chats);
    let screen = text(&picker.render(&theme(), 100, &kb())).join("\n");
    assert!(
        screen
            .contains("↑↓ select · Enter open · n new thread · a add existing · Esc/Ctrl+C close"),
        "{screen}"
    );
    assert_eq!(picker.handle_key("n", &kb()), CrewPick::NewThread);
    assert_eq!(picker.handle_key("a", &kb()), CrewPick::AddExisting);
}

#[test]
fn the_crew_picker_add_existing_opens_the_adopt_picker() {
    let mut view = view(30);
    assert!(matches!(
        view.start_adopt("p-alpha".into()),
        CaptainAction::LoadChoices { project: Some(ref p), .. } if p == "p-alpha"
    ));
    view.apply_choices(
        &json!({"adopt": [{"ref": "/s/old.jsonl", "label": "old chat"}]}),
        Some("p-alpha"),
    );
    assert_eq!(
        view.handle_key("enter", &kb()),
        CaptainAction::Act {
            op: "adopt".into(),
            args: json!({"project": "p-alpha", "ref": "/s/old.jsonl"}),
            done: "Thread added to crew.".into(),
        }
    );
}

#[test]
fn crew_focus_n_opens_a_blank_crew_chat_and_projects_n_still_starts_a_captain() {
    let new_thread = CaptainAction::Act {
        op: "new-thread".into(),
        args: json!({"project": "p-alpha"}),
        done: "New crew chat.".into(),
    };
    let mut view = view(30);
    assert_eq!(
        view.handle_key("N", &kb()),
        new_thread,
        "N works from the project rail"
    );
    view.handle_key("?", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("Crew: n/N new chat · a add thread"),
        "{screen}"
    );
    view.handle_key("?", &kb());
    view.handle_key("tab", &kb());
    assert_eq!(view.handle_key("n", &kb()), new_thread);
    view.handle_key("tab", &kb());
    assert!(
        !matches!(view.handle_key("n", &kb()), CaptainAction::Act { ref op, .. } if op == "new-thread"),
        "n on the project rail starts a new Captain"
    );
    assert!(
        view.notice.as_deref() == Some("This project already has a Captain."),
        "n on the rail reaches the new-Captain setup"
    );
}

#[test]
fn a_new_thread_reply_opens_its_session() {
    assert_eq!(
        OpenTarget::from_new_thread(&json!({"ok": true, "member": "m", "session": "/s/new.jsonl"})),
        Some(OpenTarget::Session {
            path: "/s/new.jsonl".into(),
            factory: false,
            draft: None,
        })
    );
}

/// Setup up to its factory step, on a new project in /work/gamma.
fn setup_at_factory_step() -> CaptainView {
    let mut view = view(40);
    let mut data = snapshot();
    data["current_root"] = json!("/work/gamma");
    data["current"] = Value::Null;
    view.apply_snapshot(&data);
    view.handle_key("n", &kb());
    let choices = json!({"models": [{"provider": "prime", "model": ""}], "thinking": ["default"]});
    let CaptainAction::Compose { purpose, .. } = view.apply_choices(&choices, None) else {
        panic!("the brief uses pa's editor")
    };
    view.compose_done(purpose, Some("Ship gamma".into()));
    view.apply_setup_model(String::new(), String::new());
    view
}

#[test]
fn onboarding_builds_a_custom_fsm_from_the_chart() {
    let mut view = setup_at_factory_step();
    view.handle_key("down", &kb());
    assert_eq!(view.handle_key("enter", &kb()), CaptainAction::None);
    let screen = text(&view.render(&theme(), 120, &kb())).join("\n");
    assert!(
        screen.contains("new captain · build custom FSM")
            && screen.contains("[x] plan")
            && screen.contains("[ ] consensus")
            && screen.contains("[x] review-triad")
            && screen.contains("live chart")
            && screen.contains("plan → implement → validate → review-triad"),
        "{screen}"
    );
    // Esc returns to the factory step; choosing the chart again reopens it.
    view.handle_key("escape", &kb());
    let screen = text(&view.render(&theme(), 120, &kb())).join("\n");
    assert!(screen.contains("Build custom FSM from chart"), "{screen}");
    view.handle_key("down", &kb());
    view.handle_key("enter", &kb());
    view.handle_key("down", &kb()); // consensus
    view.handle_key("space", &kb());
    // Saving skips "factory models" (the chart sets models per stage) and goes to workers.
    view.handle_key("s", &kb());
    let screen = text(&view.render(&theme(), 120, &kb())).join("\n");
    assert!(screen.contains("worker limit"), "{screen}");
    view.handle_key("enter", &kb()); // workers
    view.handle_key("enter", &kb()); // threads
    let screen = text(&view.render(&theme(), 120, &kb())).join("\n");
    assert!(
        screen.contains(
            "factory: custom chart · plan → consensus → implement → validate → review-triad"
        ),
        "{screen}"
    );
    let CaptainAction::Act { op, args, .. } = view.handle_key("y", &kb()) else {
        panic!("y creates the Captain")
    };
    assert_eq!(op, "init");
    assert_eq!(
        args["compose"]["stages"],
        json!(["plan", "consensus", "implement", "validate", "review-triad"])
    );
    assert!(args.get("design").is_none() && args.get("machine").is_none());
}

#[test]
fn none_for_now_skips_the_factory() {
    let mut view = setup_at_factory_step();
    view.handle_key("down", &kb());
    view.handle_key("down", &kb());
    view.handle_key("enter", &kb()); // none
    view.handle_key("enter", &kb()); // workers
    view.handle_key("enter", &kb()); // threads
    let CaptainAction::Act { args, .. } = view.handle_key("y", &kb()) else {
        panic!("y creates the Captain")
    };
    assert!(args.get("design").is_none() && args.get("compose").is_none());
}

#[test]
fn the_chart_builder_redraws_live_reorders_and_saves_factory_compose() {
    let mut view = view(48);
    view.handle_key("4", &kb());
    assert_eq!(view.handle_key("b", &kb()), CaptainAction::None);
    let chart = |view: &CaptainView| text(&view.render(&theme(), 130, &kb())).join("\n");
    let before = chart(&view);
    assert!(
        before.contains("build custom FSM · pipeline")
            && before.contains("review-triad-verdict")
            && !before.contains("consensus-a"),
        "{before}"
    );
    // Space on consensus adds its two voters to the live chart at once.
    view.handle_key("down", &kb());
    view.handle_key("space", &kb());
    let after = chart(&view);
    assert!(
        after.contains("[x] consensus") && after.contains("consensus-a") && after != before,
        "{after}"
    );
    // The preview wires the rework loops back to implement.
    let Mode::Compose(compose) = &view.mode else {
        panic!("the chart builder is open")
    };
    let spec = compose.spec();
    let edges = spec["transitions"].as_array().unwrap();
    assert!(edges.iter().any(|e| e["from"] == "review-triad-verdict"
        && e["to"] == "implement"
        && e["when"]["value"] == "rework"));
    assert!(edges.iter().any(|e| e["from"] == "validate-run"
        && e["to"] == "implement"
        && e["when"]["value"] == json!(false)));
    // J moves consensus below implement; the chart order follows.
    view.handle_key("J", &kb());
    assert!(
        chart(&view).contains("plan → implement → consensus → validate"),
        "{}",
        chart(&view)
    );
    view.handle_key("K", &kb());
    // m picks a model for the selected stage with the shared picker.
    let CaptainAction::PickModel(CaptainModelTarget::Crew { member, .. }) =
        view.handle_key("m", &kb())
    else {
        panic!("m opens the shared model picker")
    };
    assert_eq!(member, format!("{COMPOSE_MEMBER}consensus"));
    view.apply_compose_model("consensus", "openai/gpt-5".into(), "high".into());
    assert!(chart(&view).contains("gpt-5 · high"), "{}", chart(&view));
    // A chart that starts with a module is refused before it reaches the backend.
    view.handle_key("up", &kb());
    view.handle_key("space", &kb()); // plan off
    view.handle_key("down", &kb());
    view.handle_key("space", &kb()); // consensus off
    view.handle_key("down", &kb());
    view.handle_key("space", &kb()); // implement off
    assert_eq!(view.handle_key("s", &kb()), CaptainAction::None);
    assert!(chart(&view).contains("Start the factory with plan or implement."));
    view.handle_key("space", &kb()); // implement on
    view.handle_key("up", &kb());
    view.handle_key("space", &kb()); // consensus on
    view.handle_key("up", &kb());
    view.handle_key("space", &kb()); // plan on
    let CaptainAction::SaveFactory {
        op,
        project,
        name,
        args,
    } = view.handle_key("enter", &kb())
    else {
        panic!("Enter saves the chart")
    };
    assert_eq!(
        (op.as_str(), project.as_str(), name.as_str()),
        ("factory-compose", "p-alpha", "pipeline")
    );
    assert_eq!(
        args["stages"],
        json!(["plan", "consensus", "implement", "validate", "review-triad"])
    );
    assert_eq!(
        args["models"]["consensus"],
        json!({"model": "openai/gpt-5", "thinking": "high"})
    );
    assert_eq!(args["replace"], json!(false));
}

fn composed_show() -> Value {
    json!({
        "name": "pipeline", "state": "draft", "approved": false,
        "path": "/work/alpha/.prime/machines/pipeline", "file": "/work/alpha/.prime/machines/pipeline/machine.toml",
        "digest": "cd".repeat(32), "errors": [], "live": false,
        "graph": {
            "states": [
                {"id": "plan", "entry": true, "subagent": {"prompt": "x"}},
                {"id": "implement", "subagent": {"prompt": "y"}},
                {"id": "validate-run", "python": {"import": "validate", "callable": "run"}},
                {"id": "done", "python": {"import": "done", "callable": "done"}}
            ],
            "transitions": [
                {"from": "plan", "to": "implement"}, {"from": "implement", "to": "validate-run"},
                {"from": "validate-run", "to": "done"}, {"from": "validate-run", "to": "implement"}
            ]
        },
    })
}

#[test]
fn the_review_page_adds_and_removes_stages() {
    let mut view = view(40);
    view.open_factory("p-alpha".into(), &composed_show(), true);
    // + lists the stages the factory lacks and adds the chosen one.
    view.handle_key("+", &kb());
    let screen = text(&view.render(&theme(), 120, &kb())).join("\n");
    assert!(
        screen.contains("pipeline · add a stage")
            && screen.contains("review-triad · module")
            && !screen.contains("implement · code"),
        "{screen}"
    );
    let at = |screen: &str| {
        screen
            .lines()
            .position(|l| l.contains("review-triad ·"))
            .unwrap()
    };
    let consensus = screen
        .lines()
        .position(|l| l.contains("consensus ·"))
        .unwrap();
    for _ in consensus..at(&screen) {
        view.handle_key("down", &kb());
    }
    let CaptainAction::SaveFactory { op, args, .. } = view.handle_key("enter", &kb()) else {
        panic!("Enter adds the stage")
    };
    assert_eq!(op, "factory-add-stage");
    assert_eq!(
        args,
        json!({"project": "p-alpha", "name": "pipeline", "stage": "review-triad"})
    );
    assert_eq!(
        view.open_factory_member(),
        Some(("p-alpha".into(), "pipeline".into()))
    );
    // x on the validate node asks first, then removes its stage.
    view.handle_key("down", &kb());
    view.handle_key("down", &kb());
    assert_eq!(view.handle_key("x", &kb()), CaptainAction::None);
    let screen = text(&view.render(&theme(), 120, &kb())).join("\n");
    assert!(
        screen.contains("remove the validate stage from pipeline?"),
        "{screen}"
    );
    let CaptainAction::SaveFactory { op, args, .. } = view.handle_key("y", &kb()) else {
        panic!("y removes the stage")
    };
    assert_eq!(op, "factory-remove-stage");
    assert_eq!(args["stage"], "validate");
    // b reopens the chart builder on this factory's stages, to re-compose it.
    view.handle_key("b", &kb());
    let Mode::Compose(compose) = &view.mode else {
        panic!("b opens the chart builder")
    };
    assert!(compose.replace && compose.name == "pipeline");
    assert_eq!(
        compose.choice().stages,
        vec!["plan", "implement", "validate"]
    );
}

#[test]
fn x_keeps_the_last_stage() {
    let mut show = composed_show();
    show["graph"] = json!({
        "states": [{"id": "implement", "entry": true, "subagent": {"prompt": "y"}},
                   {"id": "done", "python": {"import": "done", "callable": "done"}}],
        "transitions": [{"from": "implement", "to": "done"}]
    });
    let mut view = view(40);
    view.open_factory("p-alpha".into(), &show, true);
    assert_eq!(view.handle_key("x", &kb()), CaptainAction::None);
    let screen = text(&view.render(&theme(), 120, &kb())).join("\n");
    assert!(screen.contains("keeps at least one stage"), "{screen}");
}

/// A project with a live factory run (two step subagents), an unread thread whose title is a
/// bare id, and two notifications.
fn live_run_snapshot() -> Value {
    let steps = json!([
        {"state": "implement", "role": "implementer",
         "model": "antigravity/gemini-4-argon", "thinking": "high",
         "status": "running", "child_id": "sub-abc12345",
         "child_name": "eigen-pipeline › implement #1.1",
         "session": "/s/child-impl.jsonl", "progress_note": "▸ bash pytest"},
        {"state": "design", "status": "done", "child_id": "sub-0001", "session": ""}
    ]);
    let run = json!({
        "id": "d0e6edff", "kind": "factory", "title": "d0e6edff",
        "display_name": "eigen-pipeline · Phase 2 classical baselines",
        "machine": "eigen-pipeline", "state": "running", "run_state": "running",
        "progress": "1/4", "step": "implement", "headless": true, "run_id": "run-9",
        "unread": true, "unread_count": 2, "active_states": ["implement"],
        "step_agents": steps
    });
    let thread = json!({
        "id": "1a2b3c4d", "kind": "native", "title": "1a2b3c4d", "provider": "prime",
        "state": "ready", "session": "/s/n.jsonl", "unread": true, "unread_count": 1
    });
    let notifications = json!([
        {"id": 2, "member": "d0e6edff", "kind": "done", "title": "implement finished",
         "detail": "tests pass", "created_at": "2026-11-23T06:00:00+00:00"},
        {"id": 1, "member": "1a2b3c4d", "kind": "needs_you", "title": "pick a library",
         "read_at": "2026-11-23T06:01:00+00:00"}
    ]);
    json!({
        "current_root": "/work/eigen",
        "projects": [{
            "id": "p-eigen", "name": "eigen", "root": "/work/eigen", "objective": "Eigen",
            "state": "active", "lead": "lead-e", "lead_state": "ready",
            "unread": true, "unread_count": 3,
            "crew": [run, thread],
            "notifications": notifications
        }]
    })
}

fn live_view() -> CaptainView {
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&live_run_snapshot());
    view
}

#[test]
fn crew_rows_show_display_names_never_bare_ids() {
    let view = live_view();
    let all = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        all.contains("● • eigen-pipeline · Phase 2 classical"),
        "{all}"
    );
    assert!(
        !all.contains("d0e6edff") && !all.contains("1a2b3c4d"),
        "a bare id is never a row title:\n{all}"
    );
    assert!(all.contains("crew thread"), "an unnamed thread:\n{all}");
    let run = &view.selected_project().unwrap().crew[0];
    assert_eq!(run.label(), "eigen-pipeline · Phase 2 classical baselines");
    assert!(is_bare_id("d0e6edff") && !is_bare_id("parser-fix"));
}

#[test]
fn a_live_run_shows_its_stage_and_enter_on_a_step_row_opens_the_subagent() {
    let mut view = live_view();
    let rows = text(&view.render(&theme(), 110, &kb()));
    let all = rows.join("\n");
    let run_row = rows
        .iter()
        .find(|r| r.contains("eigen-pipeline · Phase 2"))
        .unwrap();
    assert!(
        run_row.contains("• eigen-pipeline") && run_row.contains("▶ implement"),
        "{run_row}"
    );
    assert!(
        all.contains("└─ ◐ implement · gemini-4-argon · high · running")
            && all.contains("▸ bash pytest")
            && all.contains("└─ ✓ design · done"),
        "indented step rows:\n{all}"
    );
    view.handle_key("tab", &kb());
    assert_eq!(view.selected_member().unwrap().id, "d0e6edff");
    assert_eq!(
        view.handle_key("enter", &kb()),
        CaptainAction::OpenFactoryRun {
            project: "p-eigen".into(),
            member: "d0e6edff".into(),
            run_id: Some("run-9".into()),
        },
        "Enter on a live run opens its live factory page"
    );
    assert!(
        !view.selected_project().unwrap().crew[0].unread,
        "opening clears the unread mark"
    );
    view.handle_key("down", &kb());
    assert_eq!(
        view.handle_key("enter", &kb()),
        CaptainAction::Open {
            project: "p-eigen".into(),
            member: Some("step:/s/child-impl.jsonl".into()),
        },
        "Enter on a step row enters its live subagent"
    );
    view.handle_key("down", &kb());
    assert_eq!(
        view.handle_key("enter", &kb()),
        CaptainAction::Open {
            project: "p-eigen".into(),
            member: Some("step:sub-0001".into()),
        },
        "a step with no session opens by child id"
    );
    assert!(matches!(
        view.handle_key("l", &kb()),
        CaptainAction::OpenFactoryRun { ref member, .. } if member == "d0e6edff"
    ));
    assert!(matches!(
        view.handle_key("f", &kb()),
        CaptainAction::OpenFactoryRun { ref member, .. } if member == "d0e6edff"
    ));
    view.handle_key("down", &kb());
    assert_eq!(view.selected_member().unwrap().id, "1a2b3c4d");
}

#[test]
fn the_crew_picker_lists_a_runs_step_subagents_and_enter_enters_one() {
    let view = live_view();
    let project = view.selected_project().unwrap().clone();
    let chats = vec![
        CrewChat {
            label: "captain".into(),
            kind: "captain".into(),
            session: Some("/s/lead.jsonl".into()),
            ..CrewChat::default()
        },
        CrewChat {
            label: project.crew[0].label(),
            member: Some("d0e6edff".into()),
            status: "working".into(),
            kind: "factory".into(),
            ..CrewChat::default()
        },
    ];
    let mut picker = CrewPicker::new("eigen".into(), "captain".into(), chats);
    picker.set_project(&project);
    let screen = text(&picker.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("• eigen-pipeline · Phase 2 classical baselines")
            && screen.contains("▶ implement"),
        "{screen}"
    );
    assert!(
        screen.contains("└─ ◐ implement · gemini-4-argon · high · running"),
        "{screen}"
    );
    assert!(!screen.contains("d0e6edff"), "{screen}");
    picker.handle_key("down", &kb());
    picker.handle_key("down", &kb());
    let CrewPick::Open(chat) = picker.handle_key("enter", &kb()) else {
        panic!("Enter on a step row opens it");
    };
    assert_eq!(chat.member.as_deref(), Some("step:/s/child-impl.jsonl"));
    assert_eq!(chat.session.as_deref(), Some("/s/child-impl.jsonl"));
    assert!(!chat.is_provider_thread(), "a step opens as a native chat");
}

#[test]
fn unread_dots_and_the_inbox_mark_all_read_and_open() {
    let mut view = live_view();
    let all = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        all.contains("•1 unread") && all.contains("i inbox"),
        "{all}"
    );
    assert_eq!(view.handle_key("i", &kb()), CaptainAction::None);
    let inbox = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        inbox.contains("Inbox")
            && inbox.contains("1 unread")
            && inbox.contains("• done implement finished · tests pass")
            && inbox.contains("needs_you pick a library"),
        "{inbox}"
    );
    assert_eq!(
        view.handle_key("r", &kb()),
        CaptainAction::Act {
            op: "notifications-read".into(),
            args: json!({"project": "p-eigen"}),
            done: "Marked all notifications read.".into(),
        }
    );
    let inbox = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(inbox.contains("0 unread"), "{inbox}");
    view.handle_key("down", &kb());
    assert_eq!(
        view.handle_key("enter", &kb()),
        CaptainAction::Open {
            project: "p-eigen".into(),
            member: Some("1a2b3c4d".into()),
        },
        "Enter opens the member a notification names"
    );
    assert!(view.inbox.is_none());
    view.handle_key("i", &kb());
    view.handle_key("escape", &kb());
    assert!(view.inbox.is_none(), "Esc leaves the inbox, not the panel");
}

#[test]
fn the_unread_badge_counts_only_inbox_notifications() {
    let mut data = live_run_snapshot();
    data["projects"][0]["unread_count"] = json!(4);
    data["projects"][0]["notifications"] = json!([]);
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&data);
    let all = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        !all.contains("unread") && !all.contains("i inbox"),
        "unread crew reports alone must not point at an empty inbox: {all}"
    );
    assert!(
        all.contains("● • eigen-pipeline"),
        "the crew row keeps its unread dot: {all}"
    );

    data["projects"][0]["notifications"] = json!([
        {"id": 2, "member": "d0e6edff", "kind": "done", "title": "implement finished"},
        {"id": 1, "member": "1a2b3c4d", "kind": "needs_you", "title": "pick a library"}
    ]);
    view.apply_snapshot(&data);
    let all = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(all.contains("•2 unread  i inbox"), "{all}");
}

#[test]
fn iso_times_read_as_epoch_seconds() {
    assert_eq!(iso_seconds("1970-01-02T00:00:00+00:00"), Some(86_400.0));
    assert_eq!(
        iso_seconds("2026-11-23T06:00:00.5+01:00"),
        Some(1_795_410_000.0)
    );
    assert_eq!(iso_seconds("soon"), None);
}

fn capy_view(extra: Value) -> CaptainView {
    let mut data = snapshot();
    let project = data["projects"][0].as_object_mut().unwrap();
    for (key, value) in extra.as_object().unwrap() {
        project.insert(key.clone(), value.clone());
    }
    let mut view = CaptainView::new(40);
    view.apply_snapshot(&data);
    view
}

#[test]
fn the_spend_pill_shows_the_budget_and_b_extends_it() {
    let mut view = capy_view(json!({
        "usage": {"input_tokens": 80000, "output_tokens": 4000, "cost_usd": 0.42},
        "budget": {"budget_usd": 5.0, "spent_usd": 0.42, "exceeded": false},
        "needs_you": [{"kind": "budget_exceeded", "id": "budget", "detail": "spent $5.10 of $5.00"}]
    }));
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(screen.contains("$0.42 / $5.00"), "{screen}");
    view.handle_key("tab", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(screen.contains("b +$5 budget · c clear cap"), "{screen}");
    let CaptainAction::Act { op, args, done } = view.handle_key("b", &kb()) else {
        panic!("b extends the budget")
    };
    assert_eq!(op, "budget");
    assert_eq!(args, json!({"project": "p-alpha", "extend_usd": 5.0}));
    assert_eq!(done, "Extended budget by +$5.");
    let CaptainAction::Act { args, .. } = view.handle_key("c", &kb()) else {
        panic!("c clears the cap")
    };
    assert_eq!(args, json!({"project": "p-alpha", "clear": true}));

    let uncapped = capy_view(json!({
        "usage": {"input_tokens": 80000, "output_tokens": 4000, "cost_usd": 0.42}
    }));
    let screen = text(&uncapped.render(&theme(), 110, &kb())).join("\n");
    assert!(screen.contains("$0.42 · 84k tok"), "{screen}");
}

#[test]
fn a_decision_card_renders_and_a_digit_resolves_it() {
    let mut view = capy_view(json!({
        "needs_you": [{
            "kind": "decision", "id": "d-1", "decision_id": "d-1", "label": "pick a parser",
            "title": "Parser library", "summary": "Two libraries fit the grammar.",
            "recommendation": "nom, it is already vendored",
            "options": [
                {"key": "1", "label": "nom", "reply_text": "Use nom."},
                {"key": "2", "label": "pest", "reply_text": "Use pest."}
            ]
        }]
    }));
    view.handle_key("tab", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("Parser library")
            && screen.contains("Two libraries fit the grammar.")
            && screen.contains("recommend nom, it is already vendored")
            && screen.contains("[1] nom  [2] pest  [m] custom reply"),
        "{screen}"
    );
    let CaptainAction::Act { op, args, done } = view.handle_key("2", &kb()) else {
        panic!("2 resolves the decision")
    };
    assert_eq!(op, "decision-resolve");
    assert_eq!(args, json!({"project": "p-alpha", "id": "d-1", "key": "2"}));
    assert_eq!(done, "Decision sent to Captain.");
    let CaptainAction::Act { args, .. } = view.handle_key("1", &kb()) else {
        panic!()
    };
    assert_eq!(args["key"], "1");
    assert_eq!(
        view.tab,
        Tab::Overview,
        "a digit on a decision does not switch tabs"
    );
}

#[test]
fn crew_rows_show_stack_pr_and_findings_badges() {
    let mut data = snapshot();
    data["projects"][0]["crew"][1]["stack_position"] = json!("2/3");
    data["projects"][0]["crew"][1]["depends_on"] = json!("c-fix");
    data["projects"][0]["crew"][1]["pr_number"] = json!(42);
    data["projects"][0]["crew"][1]["pr_status"] =
        json!({"state": "open", "checks": "success", "review_decision": ""});
    data["projects"][0]["crew"][1]["open_findings_count"] = json!(3);
    data["projects"][0]["crew"][0]["depends_on"] = json!("c-root");
    let mut view = CaptainView::new(40);
    view.apply_snapshot(&data);
    let screen = text(&view.render(&theme(), 140, &kb())).join("\n");
    assert!(
        screen.contains("[2/3]") && screen.contains("PR #42 ✓") && screen.contains("!3"),
        "{screen}"
    );
    assert!(screen.contains("↳ waits"), "{screen}");
}

#[test]
fn the_wake_drawer_cycles_policies() {
    let mut view = capy_view(json!({
        "wake_policy": {"done": "react", "failed": "context", "needs_you": "ignore",
                        "debounce_seconds": 30}
    }));
    view.handle_key("w", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("Wake Policies") && screen.contains("factory paused"),
        "{screen}"
    );
    let policy = |action: CaptainAction| {
        let CaptainAction::Act { op, args, .. } = action else {
            panic!("the drawer updates the policy")
        };
        assert_eq!(op, "wake-policy");
        args["policy"].clone()
    };
    assert_eq!(
        policy(view.handle_key("enter", &kb())),
        json!({"done": "context"})
    );
    view.handle_key("down", &kb());
    assert_eq!(
        policy(view.handle_key("space", &kb())),
        json!({"failed": "ignore"})
    );
    view.handle_key("down", &kb());
    assert_eq!(
        policy(view.handle_key("space", &kb())),
        json!({"needs_you": "react"})
    );
    view.handle_key("escape", &kb());
    assert_eq!(view.drawer, None);
    view.handle_key("w", &kb());
    view.handle_key("w", &kb());
    assert_eq!(view.drawer, None, "w closes the drawer");
}

#[test]
fn the_schedules_drawer_toggles_and_deletes() {
    let mut view = capy_view(json!({
        "schedules": [
            {"id": "s-1", "title": "nightly digest", "cron_expr": "0 3 * * *", "task": "digest",
             "enabled": true, "next_run_at": "2026-11-24T03:00"},
            {"id": "s-2", "title": "weekly audit", "cron_expr": "0 9 * * 1", "task": "audit",
             "enabled": false}
        ]
    }));
    view.handle_key("S", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("Schedules")
            && screen.contains("0 3 * * *  nightly digest")
            && screen.contains("next 2026-11-24T03:00")
            && screen.contains("paused"),
        "{screen}"
    );
    let CaptainAction::Act { op, args, .. } = view.handle_key("space", &kb()) else {
        panic!()
    };
    assert_eq!(op, "schedule-toggle");
    assert_eq!(
        args,
        json!({"project": "p-alpha", "id": "s-1", "enabled": false})
    );
    view.handle_key("down", &kb());
    let CaptainAction::Act { op, args, .. } = view.handle_key("x", &kb()) else {
        panic!()
    };
    assert_eq!(op, "schedule-delete");
    assert_eq!(args, json!({"project": "p-alpha", "id": "s-2"}));
    view.handle_key("S", &kb());
    assert_eq!(view.drawer, None);
}

#[test]
fn the_drive_lists_and_previews_reports_and_handoffs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap().to_owned();
    let captain = dir.path().join(".prime/captain");
    std::fs::create_dir_all(captain.join("reports")).unwrap();
    std::fs::create_dir_all(captain.join("handoffs")).unwrap();
    std::fs::write(
        captain.join("reports/c-fix.md"),
        "# Parser fix\nAll tests pass.",
    )
    .unwrap();
    std::fs::write(
        captain.join("handoffs/c-fix.json"),
        r#"{"next":"merge the parser"}"#,
    )
    .unwrap();
    let mut view = capy_view(json!({"root": root}));
    view.handle_key("3", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("reports/c-fix.md") && screen.contains("handoffs/c-fix.json"),
        "the disk fallback lists both:\n{screen}"
    );
    for _ in 0..4 {
        view.handle_key("down", &kb());
    }
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(screen.contains("All tests pass."), "{screen}");
    view.handle_key("down", &kb());
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("\"next\": \"merge the parser\""),
        "{screen}"
    );

    let listed = capy_view(json!({
        "root": root, "drive_files": {"reports": ["c-fix.md"], "handoffs": []}
    }));
    let files = listed.drive_files();
    assert!(files.contains(&DriveFile::Saved {
        dir: "reports",
        name: "c-fix.md".into()
    }));
    assert!(files.contains(&DriveFile::Saved {
        dir: "handoffs",
        name: "c-fix.json".into()
    }));
}

/// A panel at a checkout with no Captain yet; `alpha` and `beta` stay in the rail.
fn gamma_view() -> CaptainView {
    let mut view = view(34);
    let mut data = snapshot();
    data["current_root"] = json!("/work/gamma");
    data["current"] = Value::Null;
    view.apply_snapshot(&data);
    view
}

fn setup_choices() -> Value {
    json!({
        "models": [{"provider": "prime", "model": ""}],
        "thinking": ["default", "low", "high"],
        "machines": ["project-builder"],
        "adopt": [],
    })
}

/// `n`, the options, a brief, and a model: the form waits on the factory step.
fn walk_to_factory(view: &mut CaptainView) {
    assert!(matches!(
        view.handle_key("n", &kb()),
        CaptainAction::LoadChoices { .. }
    ));
    let CaptainAction::Compose { purpose, .. } = view.apply_choices(&setup_choices(), None) else {
        panic!("the brief uses pa's editor")
    };
    view.compose_done(purpose, Some("Build Gamma end to end".into()));
    view.apply_setup_model("antigravity/gemini-4-neon".into(), "high".into());
    assert!(matches!(
        &view.mode,
        Mode::Pick(Picker { purpose: PickPurpose::Setup(s), .. }) if SETUP_STEPS[s.step] == "factory"
    ));
}

fn resume_labels(view: &CaptainView) -> Vec<String> {
    match &view.mode {
        Mode::Pick(picker) if matches!(picker.purpose, PickPurpose::ResumeSetup { .. }) => {
            picker.items.iter().map(|i| i.label.clone()).collect()
        }
        other => panic!("expected the resume prompt, got {other:?}"),
    }
}

#[test]
fn ctrl_c_saves_the_setup_and_n_offers_resume_or_fresh() {
    let mut view = gamma_view();
    walk_to_factory(&mut view);
    view.focus = Focus::Crew;
    assert_eq!(view.handle_key("ctrl+c", &kb()), CaptainAction::None);
    assert!(matches!(view.mode, Mode::Browse));
    assert!(view.pending_setup.is_none());
    assert_eq!(view.focus, Focus::Projects, "n works right away");
    assert_eq!(
        view.notice.as_deref(),
        Some("Setup saved. Press n to resume or start fresh.")
    );
    assert_eq!(view.handle_key("n", &kb()), CaptainAction::None);
    let labels = resume_labels(&view);
    assert_eq!(labels.len(), 2);
    assert!(
        labels[0].starts_with("Resume where you left off · step: factory")
            && labels[0].contains("antigravity/gemini-4-neon")
            && labels[0].contains("\"Build Gamma"),
        "{labels:?}"
    );
    assert_eq!(labels[1], "Start fresh (redo from scratch)");
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(screen.contains("unfinished captain setup"), "{screen}");
    // Esc keeps the draft.
    view.handle_key("escape", &kb());
    assert!(matches!(view.mode, Mode::Browse));
    assert!(view.saved_draft().is_some());
    view.notice = None;
    let screen = text(&view.render(&theme(), 110, &kb())).join("\n");
    assert!(
        screen.contains("Unfinished Captain setup saved. Press n to resume or redo."),
        "{screen}"
    );
}

#[test]
fn resume_restores_the_saved_step_and_answers_once_options_arrive() {
    let mut view = gamma_view();
    walk_to_factory(&mut view);
    view.handle_key("down", &kb());
    view.handle_key("down", &kb());
    view.handle_key("enter", &kb()); // factory: none
    view.handle_key("enter", &kb()); // workers: 2 → threads
    view.park();
    assert!(matches!(view.mode, Mode::Browse) && view.pending_setup.is_none());
    view.handle_key("n", &kb());
    assert_eq!(
        view.handle_key("enter", &kb()),
        CaptainAction::LoadChoices {
            project: None,
            reference: None
        }
    );
    assert_eq!(
        view.apply_choices(&setup_choices(), None),
        CaptainAction::None
    );
    let Mode::Pick(Picker {
        purpose: PickPurpose::Setup(setup),
        ..
    }) = &view.mode
    else {
        panic!("resume opens the saved step: {:?}", view.mode)
    };
    assert_eq!(SETUP_STEPS[setup.step], "threads");
    assert_eq!(setup.brief, "Build Gamma end to end");
    assert_eq!(setup.model, "antigravity/gemini-4-neon");
    assert_eq!(setup.thinking, "high");
    assert!(setup.factory_chosen && !setup.design && setup.machine.is_none());
    assert_eq!(setup.parallel, 2);
    assert_eq!(setup.choices.thinking, vec!["default", "low", "high"]);
}

#[test]
fn start_fresh_clears_the_draft_and_begins_at_the_brief() {
    let mut view = gamma_view();
    walk_to_factory(&mut view);
    view.handle_key("ctrl+c", &kb());
    view.handle_key("n", &kb());
    view.handle_key("down", &kb());
    assert!(matches!(
        view.handle_key("enter", &kb()),
        CaptainAction::LoadChoices { .. }
    ));
    assert!(view.saved_draft().is_none());
    let CaptainAction::Compose {
        purpose, initial, ..
    } = view.apply_choices(&setup_choices(), None)
    else {
        panic!("a fresh setup starts at the brief")
    };
    assert_eq!(purpose, ComposePurpose::Brief);
    assert_eq!(initial, "");
}

#[test]
fn a_cancelled_brief_keeps_the_typed_text_for_the_resume() {
    let mut view = gamma_view();
    view.handle_key("n", &kb());
    let CaptainAction::Compose { purpose, .. } = view.apply_choices(&setup_choices(), None) else {
        panic!()
    };
    view.keep_brief_text("Half a brief");
    assert_eq!(view.compose_done(purpose, None), CaptainAction::None);
    assert!(matches!(view.mode, Mode::Browse) && view.pending_setup.is_none());
    view.handle_key("n", &kb());
    view.handle_key("enter", &kb());
    let CaptainAction::Compose { initial, .. } = view.apply_choices(&setup_choices(), None) else {
        panic!("the resume reopens the brief")
    };
    assert_eq!(initial, "Half a brief");
}

#[test]
fn an_empty_cancelled_brief_saves_nothing_and_n_starts_again() {
    let mut view = gamma_view();
    view.handle_key("n", &kb());
    let CaptainAction::Compose { purpose, .. } = view.apply_choices(&setup_choices(), None) else {
        panic!()
    };
    view.compose_done(purpose, None);
    assert!(view.saved_draft().is_none());
    assert!(matches!(
        view.handle_key("n", &kb()),
        CaptainAction::LoadChoices { .. }
    ));
}

#[test]
fn confirm_ctrl_c_and_chart_ctrl_c_pause_while_n_and_esc_step_back() {
    let mut view = gamma_view();
    walk_to_factory(&mut view);
    view.handle_key("down", &kb());
    view.handle_key("enter", &kb()); // chart builder
    assert!(matches!(view.mode, Mode::Compose(_)));
    view.handle_key("ctrl+c", &kb());
    assert!(matches!(view.mode, Mode::Browse));
    assert_eq!(view.saved_draft().unwrap().step, 3);

    let mut view = gamma_view();
    walk_to_factory(&mut view);
    view.handle_key("down", &kb());
    view.handle_key("down", &kb());
    for _ in 0..3 {
        view.handle_key("enter", &kb());
    }
    assert!(matches!(view.mode, Mode::Confirm(Confirm::Create(_))));
    view.handle_key("n", &kb());
    assert!(matches!(view.mode, Mode::Pick(_)), "n steps back");
    view.handle_key("enter", &kb());
    view.handle_key("ctrl+c", &kb());
    assert!(matches!(view.mode, Mode::Browse));
    assert_eq!(SETUP_STEPS[view.saved_draft().unwrap().step], "confirm");
}

#[test]
fn drafts_persist_on_disk_and_a_created_captain_clears_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut view = gamma_view();
    view.set_draft_dir(Some(dir.path().to_path_buf()));
    walk_to_factory(&mut view);
    view.handle_key("ctrl+c", &kb());
    let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(files.len(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let path = files[0].as_ref().unwrap().path();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let mut fresh = CaptainView::new(34);
    fresh.set_draft_dir(Some(dir.path().to_path_buf()));
    let mut data = snapshot();
    data["current_root"] = json!("/work/gamma");
    data["current"] = Value::Null;
    fresh.apply_snapshot(&data);
    let draft = fresh.saved_draft().expect("the draft survives a new panel");
    assert_eq!(draft.brief, "Build Gamma end to end");
    assert_eq!(draft.model, "antigravity/gemini-4-neon");
    assert_eq!(draft.root, "/work/gamma");

    fresh.clear_draft();
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    assert!(fresh.saved_draft().is_none());
}

#[test]
fn n_starts_setup_with_crew_focus_when_no_project_exists() {
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&json!({"current_root": "/work/solo", "projects": []}));
    view.focus = Focus::Crew;
    assert!(matches!(
        view.handle_key("n", &kb()),
        CaptainAction::LoadChoices { .. }
    ));
}

#[test]
fn a_deleting_project_or_unknown_root_does_not_block_setup() {
    let mut view = view(34);
    let mut data = snapshot();
    data["projects"][0]["state"] = json!("deleting");
    view.apply_snapshot(&data);
    assert!(matches!(
        view.handle_key("n", &kb()),
        CaptainAction::LoadChoices { .. }
    ));
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&json!({"current_root": "", "projects": [{"id": "p", "root": ""}]}));
    assert!(!view.here_has_captain());
}

#[test]
fn folder_picker_badges_git_and_existing_captains_and_navigates() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let repo_a = root.join("alpha-repo");
    let repo_b = root.join("beta-existing");
    let nested = repo_a.join("sub");
    std::fs::create_dir_all(repo_a.join(".git")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();

    let mut view = CaptainView::new(34);
    view.apply_snapshot(&json!({
        "current_root": repo_b.to_string_lossy(),
        "current": "proj-beta",
        "projects": [
            {"id": "proj-other", "name": "Other", "root": "/work/other", "state": "active"},
            {"id": "proj-beta", "name": "Beta", "root": repo_b.to_string_lossy(), "state": "active"},
        ]
    }));
    view.project = 0; // start on Other
    view.open_folder_picker_at(root.to_path_buf());

    let rendered = text(&view.render(&theme(), 100, &kb())).join("\n");
    assert!(rendered.contains("alpha-repo/"), "{rendered}");
    assert!(rendered.contains("git"), "{rendered}");
    assert!(rendered.contains("[Captain: Beta]"), "{rendered}");
    assert!(!rendered.contains(".hidden/"), "dotfiles stay hidden until filtered");

    // Down to alpha-repo/, Tab descends into it, Left ascends back.
    view.handle_key("down", &kb());
    view.handle_key("tab", &kb());
    let inside = text(&view.render(&theme(), 100, &kb())).join("\n");
    assert!(inside.contains("sub/"), "{inside}");
    view.handle_key("left", &kb());

    // Down twice (past UseCurrent and alpha-repo/) to beta-existing/ -> Enter opens Beta without Setup.
    view.handle_key("down", &kb());
    view.handle_key("down", &kb());
    let action = view.handle_key("enter", &kb());
    assert!(matches!(action, CaptainAction::None), "{action:?}");
    assert!(matches!(view.mode, Mode::Browse));
    assert_eq!(view.selected_project().map(|p| p.id.as_str()), Some("proj-beta"));
}

#[test]
fn folder_picker_selects_existing_or_creates_new_folder_for_init_request() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let existing = root.join("gamma");
    std::fs::create_dir_all(&existing).unwrap();

    // 1. Selecting an existing unregistered folder sends `root` without `create_dir`.
    let mut view = CaptainView::new(34);
    view.apply_snapshot(&json!({"current_root": "/work/other", "projects": []}));
    view.open_folder_picker_at(root.to_path_buf());
    view.handle_key("down", &kb()); // gamma/
    assert!(matches!(
        view.handle_key("enter", &kb()),
        CaptainAction::LoadChoices { .. }
    ));
    let setup = view.pending_setup.as_ref().expect("pending setup");
    let req = setup.request();
    assert_eq!(req["root"], json!(existing.to_string_lossy()));
    assert!(req.get("create_dir").is_none(), "{req}");

    // 2. Typing a brand-new folder name offers "+ Create new folder" and sets `create_dir: true`.
    let mut view2 = CaptainView::new(34);
    view2.apply_snapshot(&json!({"current_root": "/work/other", "projects": []}));
    view2.open_folder_picker_at(root.to_path_buf());
    for ch in ["n", "e", "w", "a", "p", "p"] {
        view2.handle_key(ch, &kb());
    }
    let rendered = text(&view2.render(&theme(), 100, &kb())).join("\n");
    assert!(rendered.contains("Create new folder \"newapp\" here"), "{rendered}");
    assert!(matches!(
        view2.handle_key("enter", &kb()),
        CaptainAction::LoadChoices { .. }
    ));
    let setup2 = view2.pending_setup.as_ref().expect("pending setup");
    let req2 = setup2.request();
    assert_eq!(req2["root"], json!(root.join("newapp").to_string_lossy()));
    assert_eq!(req2["create_dir"], json!(true));
}
