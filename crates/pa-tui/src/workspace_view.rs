//! Native workspace pages inside pa: conversation history (`/history`, `/threads`,
//! `/resume history`), the machine editor (`/fsm edit`, factory `e`), and git review
//! (`/review`, Captain `v`).
//!
//! The view is presentation and selection only. pa-cli owns the data and every change:
//! the session UI runs `pa thread ARGS` with one JSON request for each
//! [`WorkspaceAction::Call`] and hands the JSON answer back through [`WorkspaceView::apply`].
//! Text input goes through pa's prompt editor ([`WorkspaceAction::Compose`]). The only screen
//! outside pa is the optional "edit file in $EDITOR" key on a library machine draft.

use serde_json::{json, Value};

use crate::keybindings::KeybindingsManager;
use crate::menu_panel::{hint_row, menu_row, no_match_row, rule_row, MenuSegment};
use crate::search_input::SearchInput;
use crate::theme::{Theme, ThemeColor};
use crate::width::{line_width, str_width, truncate_line, truncate_to_width};
use crate::{Line, Span};

#[cfg(test)]
mod tests;

/// Which call a background answer belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceRequest {
    HistoryList,
    HistoryOpen,
    Machines,
    Draft,
    States,
    SetModel,
    Read,
    Write,
    Save,
    Discard,
    Files,
    Diff,
    Stage,
}

/// The view's answer to a key or a call result.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WorkspaceAction {
    None,
    Close,
    /// Run `pa thread ARGS` with `input` on stdin; the answer returns to [`WorkspaceView::apply`].
    Call {
        request: WorkspaceRequest,
        args: Vec<String>,
        input: Value,
    },
    /// Resume this native session file.
    Resume(String),
    /// Ask for text in pa's prompt editor; the answer returns to [`WorkspaceView::compose_done`].
    Compose {
        label: String,
        initial: String,
    },
    /// Hand the terminal to `$VISUAL`/`$EDITOR` on this file, then call
    /// [`WorkspaceView::file_edited`].
    EditFile(String),
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn call(request: WorkspaceRequest, args: &[&str], input: Value) -> WorkspaceAction {
    WorkspaceAction::Call {
        request,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        input,
    }
}

// History

#[derive(Debug, Clone)]
struct HistoryRow {
    title: String,
    search: String,
    target: Value,
}

#[derive(Debug)]
struct History {
    groups: Vec<(String, Vec<HistoryRow>)>,
    tab: usize,
    selected: usize,
    search: SearchInput,
}

impl History {
    fn items(&self) -> Vec<&HistoryRow> {
        let term = self.search.value().to_lowercase();
        self.groups
            .get(self.tab)
            .map(|(_, rows)| rows.iter().filter(|r| r.search.contains(&term)).collect())
            .unwrap_or_default()
    }
}

// Machine editor

#[derive(Debug, Clone)]
struct MachineRow {
    name: String,
    source: String,
    kind: String,
    editable: bool,
}

#[derive(Debug, Clone, Default)]
struct Draft {
    dir: String,
    file: String,
    name: String,
    target: String,
    kind: String,
    /// The project root of a project factory draft; none for a library machine.
    project: Option<String>,
    states: Vec<(String, String, bool)>,
    selected: usize,
    /// The compiled spec for the graph view, or why it does not compile.
    graph: Option<Value>,
    graph_error: Option<String>,
}

#[derive(Debug)]
enum MachineStep {
    List,
    Draft(Draft),
}

/// The text step the prompt editor answers.
#[derive(Debug, Clone, PartialEq)]
enum ComposeStep {
    /// The name for an editable copy of this library machine.
    CopyName(String),
    /// A provider/model ID for this state of the open draft.
    Model(String),
    /// The whole definition file of the open draft.
    File,
}

#[derive(Debug)]
struct Machines {
    rows: Vec<MachineRow>,
    selected: usize,
    /// A machine named when the editor opened (`/fsm edit NAME`, factory `e`).
    preselect: Option<String>,
    /// A project factory opened from the Captain's review page as (root, name): the page edits
    /// only that factory and closes back to the review page.
    project: Option<(String, String)>,
    step: MachineStep,
    compose: Option<ComposeStep>,
    /// The draft shows the state graph instead of the state table.
    graph: bool,
}

// Review

#[derive(Debug, Clone)]
struct DiffSide {
    number: i64,
    sign: String,
    text: String,
}

#[derive(Debug, Clone)]
enum DiffRow {
    Header(String),
    Hunk(String),
    Gap,
    Line {
        kind: String,
        old: Option<DiffSide>,
        new: Option<DiffSide>,
    },
}

#[derive(Debug, Default)]
struct Review {
    /// A Captain crew thread's checkout as (project, member), or none for this chat's checkout.
    member: Option<(String, String)>,
    root: String,
    staged: bool,
    files: Vec<(String, String)>,
    selected: usize,
    rows: Vec<DiffRow>,
    top: usize,
    column: usize,
    unified: bool,
}

impl Review {
    /// One review request: `pa thread review json` here, or through the Captain backend for a
    /// crew checkout (it resolves the thread's checkout in the Captain's thread store).
    fn request(&self, request: WorkspaceRequest, body: Value) -> WorkspaceAction {
        match &self.member {
            Some((project, member)) => call(
                request,
                &["captain", "act", "review"],
                json!({"project": project, "member": member, "request": body}),
            ),
            None => call(request, &["review", "json"], body),
        }
    }

    fn files_call(&self, staged: Option<bool>) -> WorkspaceAction {
        let mut input = json!({"op": "files"});
        if let Some(staged) = staged {
            input["staged"] = json!(staged);
        }
        self.request(WorkspaceRequest::Files, input)
    }

    fn diff_call(&self) -> WorkspaceAction {
        match self.files.get(self.selected) {
            Some((path, _)) => self.request(
                WorkspaceRequest::Diff,
                json!({"op": "diff", "path": path, "staged": self.staged,
                       "mode": if self.unified { "unified" } else { "split" }}),
            ),
            None => WorkspaceAction::None,
        }
    }
}

enum Page {
    History(History),
    Machines(Machines),
    Review(Review),
}

/// The open workspace page.
pub(crate) struct WorkspaceView {
    page: Page,
    viewport_rows: usize,
    loading: bool,
    notice: Option<String>,
    error: Option<String>,
}

impl WorkspaceView {
    fn with(page: Page) -> Self {
        Self {
            page,
            viewport_rows: 24,
            loading: true,
            notice: None,
            error: None,
        }
    }

    /// The history page and its first call; `query` prefills the search.
    pub(crate) fn history(query: &str) -> (Self, WorkspaceAction) {
        let mut history = History {
            groups: Vec::new(),
            tab: 0,
            selected: 0,
            search: SearchInput::new(),
        };
        history.search.prefill(query.trim());
        (
            Self::with(Page::History(history)),
            call(
                WorkspaceRequest::HistoryList,
                &["history", "--json"],
                json!({}),
            ),
        )
    }

    /// The machine editor and its first call; `name` opens that machine.
    pub(crate) fn machines(name: &str) -> (Self, WorkspaceAction) {
        let machines = Machines {
            rows: Vec::new(),
            selected: 0,
            preselect: (!name.trim().is_empty()).then(|| name.trim().to_owned()),
            project: None,
            step: MachineStep::List,
            compose: None,
            graph: false,
        };
        (
            Self::with(Page::Machines(machines)),
            call(WorkspaceRequest::Machines, &["fsm-edit", "list"], json!({})),
        )
    }

    /// The machine editor on one project factory (`<root>/.prime/machines/<name>/`), opened
    /// from the Captain's factory review page. A saved edit changes the factory's digest, so
    /// the Captain asks for approval again.
    pub(crate) fn project_machine(root: &str, name: &str) -> (Self, WorkspaceAction) {
        let machines = Machines {
            rows: Vec::new(),
            selected: 0,
            preselect: None,
            project: Some((root.to_owned(), name.to_owned())),
            step: MachineStep::List,
            compose: None,
            graph: false,
        };
        (
            Self::with(Page::Machines(machines)),
            call(
                WorkspaceRequest::Draft,
                &["fsm-edit", "draft"],
                json!({"name": name, "project": root}),
            ),
        )
    }

    /// Whether closing this page returns to the Captain's factory review page.
    pub(crate) fn returns_to_captain(&self) -> bool {
        matches!(&self.page, Page::Machines(m) if m.project.is_some())
    }

    /// The last notice, for the page that opens after this one closes.
    pub(crate) fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    /// The review page for this checkout, or for a Captain crew thread's checkout
    /// given as (project, member).
    pub(crate) fn review(member: Option<(String, String)>) -> (Self, WorkspaceAction) {
        let review = Review {
            member,
            ..Review::default()
        };
        let action = review.files_call(None);
        (Self::with(Page::Review(review)), action)
    }

    pub(crate) fn set_viewport_rows(&mut self, rows: usize) {
        self.viewport_rows = rows.max(10);
    }

    /// The `$EDITOR` handoff returned: re-read the draft.
    pub(crate) fn file_edited(&mut self) -> WorkspaceAction {
        if let Page::Machines(Machines {
            step: MachineStep::Draft(draft),
            ..
        }) = &self.page
        {
            self.loading = true;
            return call(
                WorkspaceRequest::States,
                &["fsm-edit", "states"],
                json!({"draft": draft.dir}),
            );
        }
        WorkspaceAction::None
    }

    pub(crate) fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> WorkspaceAction {
        if key == "ctrl+c" {
            return self.leave();
        }
        self.notice = None;
        self.error = None;
        match &mut self.page {
            Page::History(history) => Self::history_key(history, key, kb),
            Page::Machines(machines) => {
                let action = Self::machines_key(machines, key, kb, &mut self.notice);
                if matches!(action, WorkspaceAction::Call { .. }) {
                    self.loading = true;
                }
                action
            }
            Page::Review(review) => Self::review_key(review, key, kb, self.viewport_rows),
        }
    }

    /// One bracketed paste: the history search takes it; other pages ignore it.
    pub(crate) fn paste(&mut self, text: &str) {
        if let Page::History(h) = &mut self.page {
            h.search.paste(&clean(text));
            h.selected = 0;
        }
    }

    /// The prompt editor's answer to a [`WorkspaceAction::Compose`]: `None` when cancelled.
    pub(crate) fn compose_done(&mut self, text: Option<String>) -> WorkspaceAction {
        let Page::Machines(m) = &mut self.page else {
            return WorkspaceAction::None;
        };
        let Some(step) = m.compose.take() else {
            return WorkspaceAction::None;
        };
        let Some(text) = text else {
            return WorkspaceAction::None;
        };
        let action = match (step, &m.step) {
            (ComposeStep::CopyName(name), _) => call(
                WorkspaceRequest::Draft,
                &["fsm-edit", "draft"],
                json!({"name": name, "target": text.trim()}),
            ),
            (ComposeStep::Model(state), MachineStep::Draft(d)) => call(
                WorkspaceRequest::SetModel,
                &["fsm-edit", "set-model"],
                json!({"draft": d.dir, "state": state, "model": text.trim()}),
            ),
            (ComposeStep::File, MachineStep::Draft(d)) => call(
                WorkspaceRequest::Write,
                &["fsm-edit", "write"],
                json!({"draft": d.dir, "text": text}),
            ),
            _ => WorkspaceAction::None,
        };
        self.loading = matches!(action, WorkspaceAction::Call { .. });
        action
    }

    /// Close the page; an open machine draft is discarded first.
    fn leave(&mut self) -> WorkspaceAction {
        if let Page::Machines(m) = &mut self.page {
            if let MachineStep::Draft(d) = &m.step {
                let draft = d.dir.clone();
                m.step = MachineStep::List;
                return call(
                    WorkspaceRequest::Discard,
                    &["fsm-edit", "discard"],
                    json!({"draft": draft}),
                );
            }
        }
        WorkspaceAction::Close
    }

    fn history_key(h: &mut History, key: &str, kb: &KeybindingsManager) -> WorkspaceAction {
        let count = h.items().len();
        let tabs = h.groups.len().max(1);
        let empty = h.search.value().is_empty();
        match key {
            "escape" if !empty => h.search = SearchInput::new(),
            "escape" => return WorkspaceAction::Close,
            "tab" => h.tab = (h.tab + 1) % tabs,
            "shift+tab" => h.tab = (h.tab + tabs - 1) % tabs,
            "right" if empty => h.tab = (h.tab + 1) % tabs,
            "left" if empty => h.tab = (h.tab + tabs - 1) % tabs,
            "enter" => {
                if let Some(row) = h.items().get(h.selected) {
                    return call(
                        WorkspaceRequest::HistoryOpen,
                        &["history", "--open"],
                        json!({"target": row.target, "title": row.title}),
                    );
                }
                return WorkspaceAction::None;
            }
            _ if empty && key.len() == 1 && key.chars().all(|c| ('1'..='9').contains(&c)) => {
                let index = key.parse::<usize>().unwrap_or(1) - 1;
                h.tab = index.min(tabs - 1);
            }
            _ if kb.matches(key, "tui.select.up") => {
                h.selected = h.selected.saturating_sub(1);
                return WorkspaceAction::None;
            }
            _ if kb.matches(key, "tui.select.down") => {
                h.selected = (h.selected + 1).min(count.saturating_sub(1));
                return WorkspaceAction::None;
            }
            _ if kb.matches(key, "tui.select.pageUp") => {
                h.selected = h.selected.saturating_sub(10);
                return WorkspaceAction::None;
            }
            _ if kb.matches(key, "tui.select.pageDown") => {
                h.selected = (h.selected + 10).min(count.saturating_sub(1));
                return WorkspaceAction::None;
            }
            _ => {
                // Every other key edits the search, as in the settings menu.
                let before = h.search.value().to_owned();
                h.search.handle_key(key, kb);
                if h.search.value() == before {
                    return WorkspaceAction::None;
                }
            }
        }
        h.selected = 0;
        WorkspaceAction::None
    }

    fn machines_key(
        m: &mut Machines,
        key: &str,
        kb: &KeybindingsManager,
        notice: &mut Option<String>,
    ) -> WorkspaceAction {
        let up = kb.matches(key, "tui.select.up") || key == "k";
        let down = kb.matches(key, "tui.select.down") || key == "j";
        let project = m.project.is_some();
        match &mut m.step {
            MachineStep::List => match key {
                "q" | "escape" => WorkspaceAction::Close,
                "enter" => Self::open_machine(m),
                _ if up => {
                    m.selected = m.selected.saturating_sub(1);
                    WorkspaceAction::None
                }
                _ if down => {
                    m.selected = (m.selected + 1).min(m.rows.len().saturating_sub(1));
                    WorkspaceAction::None
                }
                _ => WorkspaceAction::None,
            },
            MachineStep::Draft(d) => match key {
                "escape" | "q" => {
                    let input = json!({"draft": d.dir});
                    m.step = MachineStep::List;
                    call(WorkspaceRequest::Discard, &["fsm-edit", "discard"], input)
                }
                "e" => call(
                    WorkspaceRequest::Read,
                    &["fsm-edit", "read"],
                    json!({"draft": d.dir}),
                ),
                // The optional outside editor stays for library machines only.
                "E" | "shift+e" if !project => WorkspaceAction::EditFile(d.file.clone()),
                "g" => {
                    m.graph = !m.graph;
                    WorkspaceAction::None
                }
                "s" => call(
                    WorkspaceRequest::Save,
                    &["fsm-edit", "save"],
                    json!({"draft": d.dir}),
                ),
                "r" => call(
                    WorkspaceRequest::States,
                    &["fsm-edit", "states"],
                    json!({"draft": d.dir}),
                ),
                "m" | "enter" => {
                    if let Some((id, model, true)) = d.states.get(d.selected).cloned() {
                        m.compose = Some(ComposeStep::Model(id.clone()));
                        return WorkspaceAction::Compose {
                            label: format!("model for {id} · provider/model, empty inherits"),
                            initial: if model == "inherit" {
                                String::new()
                            } else {
                                model
                            },
                        };
                    }
                    *notice = Some(if d.kind == "toml" {
                        "Press e to edit machine.toml and change a model.".into()
                    } else {
                        "This state uses a referenced agent. Press e to edit the definition.".into()
                    });
                    WorkspaceAction::None
                }
                _ if up => {
                    d.selected = d.selected.saturating_sub(1);
                    WorkspaceAction::None
                }
                _ if down => {
                    d.selected = (d.selected + 1).min(d.states.len().saturating_sub(1));
                    WorkspaceAction::None
                }
                _ => WorkspaceAction::None,
            },
        }
    }

    /// Enter on a machine: draft it, or first ask for the copy's name.
    fn open_machine(m: &mut Machines) -> WorkspaceAction {
        let Some(row) = m.rows.get(m.selected) else {
            return WorkspaceAction::None;
        };
        if row.editable {
            return call(
                WorkspaceRequest::Draft,
                &["fsm-edit", "draft"],
                json!({"name": row.name}),
            );
        }
        m.compose = Some(ComposeStep::CopyName(row.name.clone()));
        WorkspaceAction::Compose {
            label: format!("name for your editable copy of {}", row.name),
            initial: format!("my-{}", row.name),
        }
    }

    fn review_key(
        r: &mut Review,
        key: &str,
        kb: &KeybindingsManager,
        rows: usize,
    ) -> WorkspaceAction {
        let page = rows.saturating_sub(10).max(1);
        let max = r.rows.len().saturating_sub(1);
        match key {
            "q" | "escape" => return WorkspaceAction::Close,
            "tab" => return r.files_call(Some(!r.staged)),
            "r" => return r.files_call(Some(r.staged)),
            "v" => {
                r.unified = !r.unified;
                return r.diff_call();
            }
            "s" => {
                if let Some((path, _)) = r.files.get(r.selected) {
                    let op = if r.staged { "unstage" } else { "stage" };
                    return r.request(WorkspaceRequest::Stage, json!({"op": op, "path": path}));
                }
            }
            "right" => r.column += 8,
            "left" => r.column = r.column.saturating_sub(8),
            "home" => r.column = 0,
            "J" => r.top = (r.top + 1).min(max),
            "K" => r.top = r.top.saturating_sub(1),
            "n" | "p" => {
                let mut hunks = r
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| matches!(row, DiffRow::Hunk(_)))
                    .map(|(i, _)| i);
                let next = if key == "n" {
                    hunks.find(|i| *i > r.top)
                } else {
                    hunks.rfind(|i| *i < r.top)
                };
                if let Some(i) = next {
                    r.top = i;
                }
            }
            _ if kb.matches(key, "tui.select.pageDown") => r.top = (r.top + page).min(max),
            _ if kb.matches(key, "tui.select.pageUp") => r.top = r.top.saturating_sub(page),
            _ if kb.matches(key, "tui.select.up") || key == "k" => {
                if r.selected > 0 {
                    r.selected -= 1;
                    return r.diff_call();
                }
            }
            _ if kb.matches(key, "tui.select.down") || key == "j" => {
                if r.selected + 1 < r.files.len() {
                    r.selected += 1;
                    return r.diff_call();
                }
            }
            _ => {}
        }
        WorkspaceAction::None
    }

    /// One background call's answer.
    pub(crate) fn apply(
        &mut self,
        request: WorkspaceRequest,
        result: Result<Value, String>,
    ) -> WorkspaceAction {
        self.loading = false;
        let data = match result {
            Ok(data) => data,
            Err(error) => {
                self.error = Some(error);
                return WorkspaceAction::None;
            }
        };
        match (&mut self.page, request) {
            (Page::History(h), WorkspaceRequest::HistoryList) => {
                h.groups = data["groups"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|g| {
                        let rows = g["rows"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|r| HistoryRow {
                                title: clean(r["title"].as_str().unwrap_or("Untitled thread")),
                                search: r["search"].as_str().unwrap_or_default().to_lowercase(),
                                target: r["target"].clone(),
                            })
                            .collect();
                        (clean(g["name"].as_str().unwrap_or("")), rows)
                    })
                    .collect();
                h.tab = h.tab.min(h.groups.len().saturating_sub(1));
            }
            (Page::History(_), WorkspaceRequest::HistoryOpen) => {
                if let Some(path) = data["path"].as_str() {
                    return WorkspaceAction::Resume(path.to_owned());
                }
            }
            (Page::Machines(m), WorkspaceRequest::Machines) => {
                m.rows = data["machines"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|r| MachineRow {
                        name: clean(r["name"].as_str().unwrap_or("")),
                        source: clean(r["source"].as_str().unwrap_or("")),
                        kind: clean(r["kind"].as_str().unwrap_or("")),
                        editable: r["editable"].as_bool().unwrap_or(false),
                    })
                    .collect();
                m.selected = m.selected.min(m.rows.len().saturating_sub(1));
                if let Some(name) = m.preselect.take() {
                    match m.rows.iter().position(|r| r.name == name) {
                        Some(index) => {
                            m.selected = index;
                            let action = Self::open_machine(m);
                            self.loading = matches!(action, WorkspaceAction::Call { .. });
                            return action;
                        }
                        None => {
                            self.notice = Some(format!(
                                "{name} has no library definition. Choose a library machine to edit."
                            ));
                        }
                    }
                }
            }
            (Page::Machines(m), WorkspaceRequest::Read) => {
                if let MachineStep::Draft(d) = &m.step {
                    m.compose = Some(ComposeStep::File);
                    let file = std::path::Path::new(&d.file)
                        .file_name()
                        .map_or_else(|| d.file.clone(), |f| f.to_string_lossy().into_owned());
                    return WorkspaceAction::Compose {
                        label: format!("{} · {file}", d.target),
                        initial: data["text"].as_str().unwrap_or_default().to_owned(),
                    };
                }
            }
            (
                Page::Machines(m),
                WorkspaceRequest::Draft
                | WorkspaceRequest::States
                | WorkspaceRequest::SetModel
                | WorkspaceRequest::Write,
            ) => {
                let selected = match &m.step {
                    MachineStep::Draft(d) => d.selected,
                    MachineStep::List => 0,
                };
                let states: Vec<(String, String, bool)> = data["states"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|s| {
                        (
                            clean(s["id"].as_str().unwrap_or("")),
                            clean(s["model"].as_str().unwrap_or("")),
                            s["editable"].as_bool().unwrap_or(false),
                        )
                    })
                    .collect();
                m.step = MachineStep::Draft(Draft {
                    dir: data["draft"].as_str().unwrap_or_default().to_owned(),
                    file: data["file"].as_str().unwrap_or_default().to_owned(),
                    name: clean(data["name"].as_str().unwrap_or("")),
                    target: clean(data["target"].as_str().unwrap_or("")),
                    kind: data["kind"].as_str().unwrap_or_default().to_owned(),
                    project: data["project"].as_str().map(str::to_owned),
                    selected: selected.min(states.len().saturating_sub(1)),
                    states,
                    graph: data.get("graph").filter(|g| g.is_object()).cloned(),
                    graph_error: data["graph_error"].as_str().map(clean),
                });
                match request {
                    WorkspaceRequest::SetModel => {
                        self.notice = Some("Model updated in the draft. Press s to save.".into());
                    }
                    WorkspaceRequest::Write => {
                        self.notice = Some("Draft updated. Press s to validate and save.".into());
                    }
                    _ => {}
                }
            }
            (Page::Machines(m), WorkspaceRequest::Save) => {
                m.step = MachineStep::List;
                let saved = data["saved"].as_str().unwrap_or("the machine").to_owned();
                if m.project.is_some() {
                    self.notice = Some(format!(
                        "Saved {saved}. This version needs your approval before it runs."
                    ));
                    return WorkspaceAction::Close;
                }
                self.notice = Some(format!("Saved {saved} for future runs."));
                self.loading = true;
                return call(WorkspaceRequest::Machines, &["fsm-edit", "list"], json!({}));
            }
            (Page::Machines(m), WorkspaceRequest::Discard) => {
                m.step = MachineStep::List;
                self.notice = Some("Draft discarded.".into());
                if m.project.is_some() {
                    return WorkspaceAction::Close;
                }
            }
            (Page::Review(r), WorkspaceRequest::Files) => {
                let previous = r.files.get(r.selected).map(|f| f.0.clone());
                data["root"]
                    .as_str()
                    .unwrap_or_default()
                    .clone_into(&mut r.root);
                r.staged = data["staged"].as_bool().unwrap_or(false);
                r.files = data["files"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|f| {
                        (
                            f["path"].as_str().unwrap_or_default().to_owned(),
                            f["status"].as_str().unwrap_or(" ").to_owned(),
                        )
                    })
                    .collect();
                r.selected = previous
                    .and_then(|p| r.files.iter().position(|f| f.0 == p))
                    .unwrap_or(r.selected)
                    .min(r.files.len().saturating_sub(1));
                r.rows.clear();
                r.top = 0;
                r.column = 0;
                let action = r.diff_call();
                self.loading = matches!(action, WorkspaceAction::Call { .. });
                return action;
            }
            (Page::Review(r), WorkspaceRequest::Diff) => {
                r.rows = data["rows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(diff_row)
                    .collect();
                r.top = r.top.min(r.rows.len().saturating_sub(1));
            }
            (Page::Review(r), WorkspaceRequest::Stage) => {
                self.loading = true;
                return r.files_call(Some(r.staged));
            }
            _ => {}
        }
        WorkspaceAction::None
    }

    pub(crate) fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let width = width.max(20);
        let (title, summary, path) = self.header();
        let mut head = vec![
            Span::raw(" "),
            theme.bold(theme.fg_span(ThemeColor::Accent, title)),
            Span::raw("  "),
            theme.fg_span(ThemeColor::Muted, summary),
        ];
        if !path.is_empty() {
            head.push(theme.fg_span(ThemeColor::Dim, format!("  {path}")));
        }
        let mut out: Vec<Line> = vec![truncate_line(&head, width, "…")];
        // The history page opens on its search field, whose rules frame the header.
        if !matches!(self.page, Page::History(_)) {
            out.push(rule_row(theme, width));
        }
        let body_rows = self.viewport_rows.saturating_sub(out.len() + 3);
        let body = match &self.page {
            Page::History(h) => Self::render_history(h, theme, width, body_rows),
            Page::Machines(m) => Self::render_machines(m, theme, width, body_rows),
            Page::Review(r) => Self::render_review(r, theme, width, body_rows),
        };
        out.extend(body.into_iter().take(body_rows));
        while out.len() < self.viewport_rows.saturating_sub(3) {
            out.push(Vec::new());
        }
        let status = self
            .error
            .as_ref()
            .map(|e| theme.fg_span(ThemeColor::Error, clean(e)))
            .or_else(|| {
                self.notice
                    .as_ref()
                    .map(|n| theme.fg_span(ThemeColor::Text, clean(n)))
            })
            .or_else(|| {
                self.loading
                    .then(|| theme.fg_span(ThemeColor::Dim, "Loading…"))
            })
            .unwrap_or_else(|| Span::raw(""));
        out.push(truncate_line(&vec![Span::raw(" "), status], width, "…"));
        out.push(rule_row(theme, width));
        out.push(hint_row(theme, width, &self.footer(width, kb)));
        out
    }

    fn header(&self) -> (&'static str, String, String) {
        match &self.page {
            Page::History(h) => {
                let total: usize = h.groups.iter().map(|g| g.1.len()).sum();
                ("history", format!("{total} conversations"), String::new())
            }
            Page::Machines(m) => match &m.step {
                MachineStep::Draft(d) => match &m.project {
                    // The factory's own folder: the draft path is an implementation detail.
                    Some((root, name)) => (
                        "machines",
                        "project factory".into(),
                        format!("{root}/.prime/machines/{name}"),
                    ),
                    None => (
                        "machines",
                        format!("{} installed", m.rows.len()),
                        d.file.clone(),
                    ),
                },
                MachineStep::List => (
                    "machines",
                    format!("{} installed", m.rows.len()),
                    String::new(),
                ),
            },
            Page::Review(r) => (
                "review",
                format!(
                    "{} {} file{}",
                    r.files.len(),
                    if r.staged { "staged" } else { "unstaged" },
                    if r.files.len() == 1 { "" } else { "s" }
                ),
                r.root.clone(),
            ),
        }
    }

    fn footer(&self, width: usize, kb: &KeybindingsManager) -> String {
        let _ = kb;
        let narrow = width < 100;
        match &self.page {
            Page::History(_) if narrow => {
                "Type to search · Tab provider · Enter resume · Esc close".into()
            }
            Page::History(_) => {
                "Type to search · Tab/1-9 provider · ↑↓ select · Enter resume · Esc close".into()
            }
            Page::Machines(m) => match &m.step {
                MachineStep::List => "↑↓ select · Enter edit · Esc close".into(),
                MachineStep::Draft(_) => {
                    let graph = if m.graph { "g states" } else { "g graph" };
                    let outside = if m.project.is_some() {
                        ""
                    } else {
                        " · E $EDITOR"
                    };
                    let back = if m.project.is_some() {
                        "Esc back"
                    } else {
                        "Esc discard"
                    };
                    if m.graph && narrow {
                        format!("e edit · {graph} · s save · {back}")
                    } else if m.graph {
                        format!("e edit file · {graph} · s validate and save · r reload{outside} · {back}")
                    } else if narrow {
                        format!("↑↓ · m model · e edit · {graph} · s save · {back}")
                    } else {
                        format!("↑↓ state · m model · e edit file · {graph} · s validate and save · r reload{outside} · {back}")
                    }
                }
            },
            Page::Review(r) => {
                let stage = if r.staged { "s unstage" } else { "s stage" };
                if narrow {
                    format!("↑↓ file · PgUp/PgDn · Tab staged · {stage} · Esc close")
                } else {
                    format!("↑↓ file · ←→ pan · PgUp/PgDn scroll · n/p hunk · Tab staged · v layout · {stage} · r refresh · Esc close")
                }
            }
        }
    }

    /// The rows `top..top+room` that keep `selected` visible.
    fn window(selected: usize, len: usize, room: usize) -> std::ops::Range<usize> {
        let room = room.max(1);
        let top = selected
            .saturating_sub(room - 1)
            .min(len.saturating_sub(room));
        top..(top + room).min(len)
    }

    fn render_history(h: &History, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        // The settings menu's layout: search field, tab strip, rows.
        let mut out = crate::menu_panel::search_field_lines(
            theme,
            width,
            h.search.value(),
            h.search.cursor(),
            true,
            "Search threads",
        );
        out.push(Vec::new());
        let names: Vec<String> = h
            .groups
            .iter()
            .map(|(name, rows)| format!("{name} {}", rows.len()))
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        out.push(crate::settings_menu::tabs::strip_row(
            theme, width, &names, h.tab,
        ));
        out.push(Vec::new());
        let items = h.items();
        if items.is_empty() {
            out.push(no_match_row(theme, width, "No matching threads"));
            return out;
        }
        let room = rows.saturating_sub(out.len());
        for i in Self::window(h.selected, items.len(), room) {
            out.push(menu_row(
                theme,
                width,
                vec![theme.fg_span(ThemeColor::Text, items[i].title.clone())],
                &[],
                i == h.selected,
            ));
        }
        out
    }

    fn render_machines(m: &Machines, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        let dim = |t: String| theme.fg_span(ThemeColor::Dim, t);
        match &m.step {
            MachineStep::List => {
                let mut out: Vec<Line> = Vec::new();
                if m.rows.is_empty() && m.project.is_none() {
                    out.push(no_match_row(theme, width, "No machines installed"));
                }
                for i in Self::window(m.selected, m.rows.len(), rows) {
                    let row = &m.rows[i];
                    let kind = if row.kind == "toml" {
                        "machine.toml"
                    } else {
                        "MACHINE.md"
                    };
                    let mut trailing =
                        vec![MenuSegment::muted(&row.source), MenuSegment::muted(kind)];
                    if !row.editable {
                        trailing.push(MenuSegment::muted("copy to edit"));
                    }
                    out.push(menu_row(
                        theme,
                        width,
                        vec![theme.fg_span(ThemeColor::Text, row.name.clone())],
                        &trailing,
                        i == m.selected,
                    ));
                }
                out
            }
            MachineStep::Draft(d) => {
                let mut out: Vec<Line> = vec![vec![
                    Span::raw(" "),
                    theme.bold(theme.fg_span(ThemeColor::Text, d.target.clone())),
                    dim(if d.project.is_some() {
                        "  draft · saving needs approval again".into()
                    } else if d.target == d.name {
                        "  draft".into()
                    } else {
                        format!("  copy of {}", d.name)
                    }),
                ]];
                out.push(Vec::new());
                if m.graph {
                    match d
                        .graph
                        .as_ref()
                        .and_then(|spec| crate::factory_view::machine_graph_lines(theme, spec))
                    {
                        Some(lines) => out.extend(
                            lines
                                .into_iter()
                                .map(|line| {
                                    let mut row = vec![Span::raw(" ")];
                                    row.extend(line);
                                    truncate_line(&row, width, "…")
                                })
                                .take(rows.saturating_sub(out.len())),
                        ),
                        None => out.push(vec![theme.fg_span(
                            ThemeColor::Warning,
                            format!(
                                " No graph: {}",
                                d.graph_error
                                    .as_deref()
                                    .unwrap_or("the draft does not compile")
                            ),
                        )]),
                    }
                    return out;
                }
                let id_width = d
                    .states
                    .iter()
                    .map(|s| str_width(&s.0))
                    .max()
                    .unwrap_or(5)
                    .max(5)
                    .min(width / 2);
                let room = rows.saturating_sub(out.len());
                for i in Self::window(d.selected, d.states.len(), room) {
                    let (id, model, editable) = &d.states[i];
                    let pad = id_width.saturating_sub(str_width(id));
                    out.push(menu_row(
                        theme,
                        width,
                        vec![
                            theme.fg_span(ThemeColor::Text, id.clone()),
                            Span::raw(" ".repeat(pad + 2)),
                            theme.fg_span(
                                if *editable {
                                    ThemeColor::MdLink
                                } else {
                                    ThemeColor::Muted
                                },
                                model.clone(),
                            ),
                        ],
                        &[],
                        i == d.selected,
                    ));
                }
                out
            }
        }
    }

    fn render_review(r: &Review, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        let wide = width >= 90;
        let list_width = if wide {
            (width / 4).clamp(14, 32)
        } else {
            width
        };
        let mut files: Vec<Line> = vec![vec![theme.fg_span(
            ThemeColor::Dim,
            if r.staged { " staged" } else { " unstaged" },
        )]];
        let list_room = if wide {
            rows.saturating_sub(1)
        } else {
            rows.saturating_sub(1).min(5)
        };
        if r.files.is_empty() {
            files.push(vec![theme.fg_span(
                ThemeColor::Dim,
                if r.staged {
                    "   No staged changes"
                } else {
                    "   No unstaged changes"
                },
            )]);
        }
        for i in Self::window(r.selected, r.files.len(), list_room) {
            let (path, status) = &r.files[i];
            let selected = i == r.selected;
            let name = theme.fg_span(ThemeColor::Text, clean(path));
            files.push(truncate_line(
                &vec![
                    Span::raw(" "),
                    theme.bold(theme.fg_span(ThemeColor::Accent, if selected { "›" } else { " " })),
                    Span::raw(" "),
                    theme.fg_span(ThemeColor::Warning, format!("{status} ")),
                    if selected { theme.bold(name) } else { name },
                ],
                list_width,
                "…",
            ));
        }
        let diff_width = if wide {
            width.saturating_sub(list_width + 3)
        } else {
            width
        };
        let diff_rows = if wide {
            rows
        } else {
            rows.saturating_sub(files.len() + 1)
        };
        // A narrow pane shows one column: split halves would be too thin to read.
        let unified = r.unified || diff_width < 80;
        let diff = Self::render_diff(r, theme, diff_width, diff_rows, unified);
        if !wide {
            let mut out = files;
            out.push(rule_row(theme, width));
            out.extend(diff);
            return out;
        }
        let border = theme.fg_span(ThemeColor::BorderMuted, "│");
        (0..rows)
            .map(|row| {
                let mut line =
                    truncate_line(files.get(row).unwrap_or(&Vec::new()), list_width, "…");
                line.push(Span::raw(
                    " ".repeat(list_width.saturating_sub(line_width(&line))),
                ));
                line.push(border.clone());
                line.push(Span::raw(" "));
                line.extend(diff.get(row).cloned().unwrap_or_default());
                line
            })
            .collect()
    }

    fn side_text(side: Option<&DiffSide>, column: usize, width: usize) -> (String, &'static str) {
        let Some(side) = side else {
            return (String::new(), "");
        };
        let text: String = side.text.chars().skip(column).collect();
        let body = format!("{:>4} {} {text}", side.number, side.sign);
        (
            truncate_to_width(&body, width, "…"),
            match side.sign.as_str() {
                "-" => "-",
                "+" => "+",
                _ => " ",
            },
        )
    }

    fn sign_color(sign: &str) -> ThemeColor {
        match sign {
            "-" => ThemeColor::ToolDiffRemoved,
            "+" => ThemeColor::ToolDiffAdded,
            _ => ThemeColor::Text,
        }
    }

    fn render_diff(
        r: &Review,
        theme: &Theme,
        width: usize,
        rows: usize,
        unified: bool,
    ) -> Vec<Line> {
        if r.files.is_empty() {
            return Vec::new();
        }
        let half = width.saturating_sub(3) / 2;
        r.rows
            .iter()
            .skip(r.top)
            .take(rows)
            .map(|row| match row {
                DiffRow::Header(text) => {
                    vec![theme.fg_span(ThemeColor::Dim, truncate_to_width(text, width, "…"))]
                }
                DiffRow::Hunk(text) => {
                    vec![theme.fg_span(ThemeColor::Accent, truncate_to_width(text, width, "…"))]
                }
                DiffRow::Gap => vec![theme.fg_span(ThemeColor::Dim, "…")],
                DiffRow::Line { kind, old, new } if unified => {
                    let side = if kind == "removed" {
                        old.as_ref()
                    } else {
                        new.as_ref().or(old.as_ref())
                    };
                    let (text, sign) = Self::side_text(side, r.column, width);
                    vec![theme.fg_span(Self::sign_color(sign), text)]
                }
                DiffRow::Line { old, new, .. } => {
                    let (left, left_sign) = Self::side_text(old.as_ref(), r.column, half);
                    let (right, right_sign) = Self::side_text(new.as_ref(), r.column, half);
                    let pad = half.saturating_sub(str_width(&left));
                    vec![
                        theme.fg_span(Self::sign_color(left_sign), left),
                        Span::raw(" ".repeat(pad)),
                        theme.fg_span(ThemeColor::BorderMuted, " │ "),
                        theme.fg_span(Self::sign_color(right_sign), right),
                    ]
                }
            })
            .collect()
    }
}

fn diff_side(value: &Value) -> Option<DiffSide> {
    value.is_object().then(|| DiffSide {
        number: value["n"].as_i64().unwrap_or(0),
        sign: value["sign"].as_str().unwrap_or(" ").to_owned(),
        text: clean(value["text"].as_str().unwrap_or("")),
    })
}

fn diff_row(value: &Value) -> DiffRow {
    let text = || clean(value["text"].as_str().unwrap_or(""));
    match value["kind"].as_str() {
        Some("file") => DiffRow::Header(text()),
        Some("hunk") => DiffRow::Hunk(text()),
        Some("gap") => DiffRow::Gap,
        kind => DiffRow::Line {
            kind: kind.unwrap_or("context").to_owned(),
            old: diff_side(&value["old"]),
            new: diff_side(&value["new"]),
        },
    }
}
