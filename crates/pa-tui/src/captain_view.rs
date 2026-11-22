//! The Captain panel: every project Captain on this machine, the selected project's records,
//! and its crew (threads and factory runs). F6 and `/captain` open it.
//!
//! The view is presentation and selection only. The Python Captain backend owns the data and
//! every action: the session UI reads `pa thread captain status` and runs
//! `pa thread captain act OP` for each [`CaptainAction::Act`] the view returns. Text input
//! (a brief, a message, an answer) uses pa's own prompt editor through
//! [`CaptainAction::Compose`]; lists use the panel's picker.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::keybindings::KeybindingsManager;
use crate::menu_panel::{menu_row, rule_row, MenuSegment};
use crate::theme::{Theme, ThemeColor};
use crate::width::{line_width, str_width, truncate_line, truncate_to_width};
use crate::{Line, Span};

#[cfg(test)]
mod tests;

/// `pa thread captain status`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CaptainSnapshot {
    pub current_root: String,
    pub current: Option<String>,
    pub projects: Vec<CaptainProject>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CaptainProject {
    pub id: String,
    pub root: String,
    pub name: String,
    pub objective: String,
    pub rules: String,
    pub plan: String,
    pub crew_rules: String,
    pub digest: String,
    pub provider: String,
    pub model: String,
    pub thinking: String,
    pub parallel: i64,
    /// active, stopping, paused, or deleting.
    pub state: String,
    pub lead: String,
    pub lead_state: String,
    pub lead_detail: Option<String>,
    pub lead_session: Option<String>,
    pub lead_busy: bool,
    pub crew: Vec<CaptainMember>,
    pub edits: Vec<CaptainEdit>,
    /// Machines the Captain wrote in `<root>/.prime/machines/<name>/`.
    pub factories: Vec<ProjectFactory>,
    pub factories_error: Option<String>,
}

/// The crew states and factory states the last status showed, keyed by member id and by
/// `project/name`, so the next status can tell what changed.
pub(crate) type CrewStates = std::collections::HashMap<String, String>;

/// The toasts for crew members and project factories whose state changed into one that
/// needs the user or ended, and the new state map. `previous` none (the first status) only
/// records the states.
pub(crate) fn crew_toasts(
    previous: Option<&CrewStates>,
    snapshot: &CaptainSnapshot,
) -> (CrewStates, Vec<String>) {
    let mut states = CrewStates::new();
    let mut toasts = Vec::new();
    for project in &snapshot.projects {
        for member in &project.crew {
            let before = previous.and_then(|p| p.get(&member.id));
            if previous.is_some() && before != Some(&member.state) {
                let text = match member.state.as_str() {
                    "needs_you" => Some("needs you"),
                    "done" => Some("done"),
                    "failed" => Some("failed"),
                    _ => None,
                };
                if let Some(text) = text {
                    toasts.push(format!("{} · {} {text}", project.name, member.label()));
                }
            }
            states.insert(member.id.clone(), member.state.clone());
        }
        for factory in &project.factories {
            let key = format!("{}/{}", project.id, factory.name);
            let before = previous.and_then(|p| p.get(&key));
            if previous.is_some() && factory.needs_review() && before != Some(&factory.state) {
                toasts.push(format!(
                    "{} · factory {} needs review",
                    project.name, factory.name
                ));
            }
            states.insert(key, factory.state.clone());
        }
    }
    (states, toasts)
}

/// A project factory and its approval state, from the backend's `captain_factories.py`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct ProjectFactory {
    pub name: String,
    /// missing, invalid, draft, approved, or changed (since approval).
    pub state: String,
    pub path: String,
    pub file: String,
    pub digest: String,
    /// Whether the user approved some version of it.
    pub approved: bool,
    pub errors: Vec<String>,
}

impl ProjectFactory {
    /// A valid version that waits for the user's approval.
    fn needs_review(&self) -> bool {
        matches!(self.state.as_str(), "draft" | "changed")
    }
}

/// The short label of a project factory state.
fn factory_state_label(state: &str, approved: bool) -> &'static str {
    match state {
        "missing" => "missing",
        "invalid" if approved => "invalid · changed since approval",
        "invalid" => "invalid draft",
        "draft" => "draft · needs approval",
        "approved" => "approved",
        "changed" => "changed since approval · needs approval",
        _ => "unknown",
    }
}

fn factory_glyph(state: &str) -> (&'static str, ThemeColor) {
    match state {
        "approved" => ("✓", ThemeColor::Success),
        "draft" | "changed" => ("!", ThemeColor::Warning),
        "invalid" => ("✗", ThemeColor::Error),
        _ => ("○", ThemeColor::Dim),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CaptainMember {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub provider: String,
    pub state: String,
    pub detail: String,
    pub run_state: Option<String>,
    pub machine: Option<String>,
    pub progress: Option<String>,
    pub step: Option<String>,
    pub headless: bool,
    pub session: Option<String>,
    pub busy: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct CaptainEdit {
    pub field: String,
    pub actor: String,
}

/// `pa thread captain act choices`: the options the forms offer.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct CaptainChoices {
    pub models: Vec<ModelChoice>,
    pub crew_models: Vec<CrewModel>,
    pub thinking: Vec<String>,
    pub machines: Vec<String>,
    pub machine_params: std::collections::BTreeMap<String, Vec<MachineParam>>,
    /// Approved project factories of the project the options are for.
    pub project_factories: Vec<String>,
    pub adopt: Vec<AdoptChoice>,
    pub brief: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct MachineParam {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub required: bool,
    pub description: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct ModelChoice {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct CrewModel {
    pub label: String,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub(crate) struct AdoptChoice {
    #[serde(rename = "ref")]
    pub reference: String,
    pub label: String,
}

impl CaptainProject {
    pub(crate) fn display_state(&self) -> &str {
        match self.state.as_str() {
            "" => "active",
            other => other,
        }
    }

    fn is_active(&self) -> bool {
        self.display_state() == "active"
    }

    fn counts(&self) -> (usize, usize, usize) {
        let mut needs = usize::from(self.lead_state == "needs_you");
        let mut running = usize::from(self.lead_state == "running");
        let mut done = 0;
        for member in &self.crew {
            match member.state.as_str() {
                "needs_you" | "failed" | "unknown" => needs += 1,
                "running" => running += 1,
                "done" => done += 1,
                _ => {}
            }
        }
        needs += self.factories.iter().filter(|f| f.needs_review()).count();
        (needs, running, done)
    }

    /// One state for the rail's ■ indicator, in priority order.
    fn summary_state(&self) -> &'static str {
        match self.display_state() {
            "paused" => return "paused",
            "stopping" | "deleting" => return "stopping",
            _ => {}
        }
        let (needs, running, _) = self.counts();
        if needs > 0 {
            "needs you"
        } else if running > 0 {
            "running"
        } else {
            "idle"
        }
    }

    pub(crate) fn model_label(&self) -> String {
        let provider = if self.provider.is_empty() {
            "prime"
        } else {
            &self.provider
        };
        let model = if self.model.is_empty() {
            "default model"
        } else {
            &self.model
        };
        if self.thinking.is_empty() {
            format!("{provider} · {model}")
        } else {
            format!("{provider} · {model} · {}", self.thinking)
        }
    }

    /// The crew in digest order: needs you, in progress, crew, recent results.
    fn ordered_crew(&self) -> Vec<(&'static str, Vec<&CaptainMember>)> {
        let group = |states: &[&str]| -> Vec<&CaptainMember> {
            self.crew
                .iter()
                .filter(|m| states.contains(&m.state.as_str()))
                .collect()
        };
        vec![
            ("needs you", group(&["needs_you", "failed"])),
            ("in progress", group(&["running"])),
            ("crew", group(&["ready", "unknown", ""])),
            ("recent results", group(&["done"])),
        ]
    }

    fn flat_crew(&self) -> Vec<&CaptainMember> {
        self.ordered_crew()
            .into_iter()
            .flat_map(|(_, members)| members)
            .collect()
    }
}

impl CaptainMember {
    /// The short state text under a crew row.
    pub(crate) fn state_text(&self) -> String {
        let run = self.run_state.as_deref().unwrap_or("");
        match self.state.as_str() {
            "needs_you" => "needs you".into(),
            "running" => "working".into(),
            "ready" if self.busy => "working".into(),
            "ready" => "idle".into(),
            "done" if matches!(run, "stopped" | "cancelled") => "stopped".into(),
            "done" => "done".into(),
            "failed" => "failed".into(),
            _ if run == "paused" => "paused".into(),
            "" | "unknown" => "unknown".into(),
            other => other.replace('_', " "),
        }
    }

    pub(crate) fn label(&self) -> String {
        if self.kind == "factory" {
            self.machine.clone().unwrap_or_else(|| self.title.clone())
        } else {
            clean(&self.title)
        }
    }

    fn is_run(&self) -> bool {
        self.kind == "factory"
    }

    fn run_ended(&self) -> bool {
        matches!(
            self.run_state.as_deref(),
            Some("done" | "failed" | "stopped" | "lost")
        )
    }
}

/// A target pa opens from the panel, as returned by the backend's `open` action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenTarget {
    /// A native session: pa's chat view opens it in place.
    Session {
        path: String,
        factory: bool,
        draft: Option<String>,
    },
    /// A provider CLI thread with no native session: the read-and-reply view.
    Thread { id: String, title: String },
    /// The factory page of the current chat.
    Factory,
    /// A draft for the current chat.
    Draft(String),
}

impl OpenTarget {
    /// The backend's `{"handoff": {...}}` reply.
    pub(crate) fn from_reply(reply: &Value) -> Option<Self> {
        let target = reply.get("handoff")?;
        let text = |key: &str| target.get(key).and_then(Value::as_str).map(str::to_owned);
        match target.get("action")?.as_str()? {
            "resume" => Some(Self::Session {
                path: text("value")?,
                factory: text("panel").as_deref() == Some("factory"),
                draft: text("draft").filter(|d| !d.trim().is_empty()),
            }),
            "thread" => Some(Self::Thread {
                id: text("value")?,
                title: text("title").unwrap_or_default(),
            }),
            "factory" => Some(Self::Factory),
            "draft" => Some(Self::Draft(text("value")?)),
            _ => None,
        }
    }
}

/// What a text step in pa's editor is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ComposePurpose {
    Brief,
    Message {
        project: String,
        member: Option<String>,
    },
    Answer {
        project: String,
        member: String,
    },
    EditText {
        project: String,
        field: String,
    },
    DelegateTask {
        project: String,
        worker: Worker,
    },
    Consensus {
        project: String,
    },
    AdoptRef {
        project: String,
    },
    /// One parameter of a factory run being launched from the factory page.
    LaunchParam(Box<Launch>),
}

/// A factory launch: the machine, its parameters, and the values entered so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Launch {
    pub machine: String,
    pub params: Vec<(String, String, bool, String)>,
    pub index: usize,
    pub values: Vec<(String, String)>,
}

/// Who runs a new assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Worker {
    /// A crew thread on a native Prime session with this pa model ("" is the default).
    Model { model: String, write: bool },
    /// A factory machine.
    Machine(String),
}

/// What the session UI does for a key press.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CaptainAction {
    None,
    Close,
    Refresh,
    /// Run `pa thread captain act OP` with `args`; `done` is the notice on success.
    Act {
        op: String,
        args: Value,
        done: String,
    },
    /// Ask the backend what to open, then open it.
    Open {
        project: String,
        member: Option<String>,
    },
    /// Ask for text in pa's prompt editor.
    Compose {
        purpose: ComposePurpose,
        label: String,
        initial: String,
    },
    /// Load the form options for a project ("" for a new one).
    LoadChoices {
        project: Option<String>,
        reference: Option<String>,
    },
    /// Open the git review of a crew thread (an external review tool).
    Review {
        project: String,
        member: String,
    },
    /// Load a project factory for its review page (`factory-show`).
    LoadFactory {
        project: String,
        name: String,
    },
    /// Open pa's machine editor on this project factory; closing it reloads the review page.
    EditFactory {
        root: String,
        name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Projects,
    Crew,
}

/// One row of a picker.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PickItem {
    pub label: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
enum PickPurpose {
    EditField {
        project: String,
    },
    Model {
        project: String,
    },
    Thinking {
        project: String,
    },
    Workers {
        project: String,
    },
    Delegate {
        project: String,
    },
    DelegateWrite {
        project: String,
        model: String,
    },
    Adopt {
        project: String,
    },
    Setup(Box<Setup>),
    Launch,
    /// `request` makes repeated design presses from one list a single request.
    Factories {
        project: String,
        request: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct Picker {
    title: String,
    items: Vec<PickItem>,
    selected: usize,
    filter: String,
    purpose: PickPurpose,
    /// Rows marked in a multi-select step (the setup's existing threads).
    marked: Vec<usize>,
    multi: bool,
}

impl Picker {
    fn new(title: &str, items: Vec<PickItem>, purpose: PickPurpose) -> Self {
        Self {
            title: title.into(),
            items,
            selected: 0,
            filter: String::new(),
            purpose,
            marked: Vec::new(),
            multi: false,
        }
    }

    fn visible(&self) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        (0..self.items.len())
            .filter(|&i| filter.is_empty() || self.items[i].label.to_lowercase().contains(&filter))
            .collect()
    }

    fn current(&self) -> Option<usize> {
        self.visible().get(self.selected).copied()
    }

    fn select_value(&mut self, value: &Value) {
        if let Some(index) = self
            .visible()
            .iter()
            .position(|&i| self.items[i].value == *value)
        {
            self.selected = index;
        }
    }
}

/// The new-project steps, in order.
pub(crate) const SETUP_STEPS: [&str; 7] = [
    "brief", "model", "thinking", "factory", "workers", "threads", "confirm",
];

/// The new-project or promote form: one value per step.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Setup {
    pub step: usize,
    pub reference: Option<String>,
    pub brief: String,
    pub provider: String,
    pub model: String,
    pub thinking: String,
    pub machine: Option<String>,
    /// The Captain designs a project factory after it writes its plan.
    pub design: bool,
    /// Whether the factory step has an answer (its picker then selects it).
    pub factory_chosen: bool,
    pub parallel: u32,
    pub crew: Vec<String>,
    pub choices: CaptainChoices,
}

impl Setup {
    fn request(&self) -> Value {
        let mut args = json!({
            "brief": self.brief,
            "provider": if self.provider.is_empty() { "prime" } else { &self.provider },
            "model": self.model,
            "thinking": self.thinking,
            "parallel": self.parallel,
            "crew": self.crew,
        });
        if let Some(reference) = &self.reference {
            args["ref"] = json!(reference);
        }
        if let Some(machine) = &self.machine {
            args["machine"] = json!(machine);
        }
        if self.design {
            args["design"] = json!(true);
        }
        args
    }

    /// The factory step's picker value for the current answer.
    fn factory_value(&self) -> Value {
        match &self.machine {
            _ if self.design => json!({"design": true}),
            Some(machine) => json!(machine),
            None => Value::Null,
        }
    }

    fn factory_summary(&self) -> String {
        match &self.machine {
            _ if self.design => "design one for this project".into(),
            Some(machine) => machine.clone(),
            None => "none".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Confirm {
    Stop {
        project: String,
        name: String,
    },
    Delete {
        project: String,
        name: String,
        crew: usize,
        running: usize,
    },
    Release {
        project: String,
        member: String,
        title: String,
    },
    Create(Box<Setup>),
    DeleteFactory(Box<FactoryReview>),
}

/// The review page of one project factory: its state, errors, and static diagram.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct FactoryReview {
    pub project: String,
    pub name: String,
    pub state: String,
    pub approved: bool,
    pub path: String,
    pub file: String,
    pub digest: String,
    /// The digest from the user's last explicit load (`F` or `r`): the version they saw.
    /// A background refresh with another digest blocks approval until `r`.
    pub reviewed: String,
    pub errors: Vec<String>,
    /// The compiled spec (`pa factory compile`), when the factory is valid.
    pub graph: Option<Value>,
    pub graph_error: Option<String>,
    /// A Captain run of this factory is queued or running.
    pub live: bool,
    pub scroll: usize,
}

/// A provider CLI thread's transcript and reply state.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ThreadView {
    pub project: String,
    pub member: String,
    pub title: String,
    pub provider: String,
    pub messages: Vec<(String, String)>,
    pub busy: bool,
    pub scroll: usize,
}

#[derive(Debug, Clone, PartialEq)]
enum Mode {
    Browse,
    Pick(Picker),
    Confirm(Confirm),
    Thread(ThreadView),
    Factory(FactoryReview),
}

pub(crate) struct CaptainView {
    snapshot: CaptainSnapshot,
    loaded: bool,
    /// The last action's error; it stays until the next key press.
    error: Option<String>,
    /// The last status read's error; the next good read clears it.
    snapshot_error: Option<String>,
    notice: Option<String>,
    project: usize,
    member: usize,
    focus: Focus,
    expanded: bool,
    help: bool,
    mode: Mode,
    viewport_rows: usize,
    /// Setup steps wait for the backend's options.
    pending_setup: Option<Setup>,
    /// A factory launch waits for the machine list.
    pending_launch: bool,
    /// The machines' parameters, for the launch form.
    machine_params: std::collections::BTreeMap<String, Vec<MachineParam>>,
}

/// Collapse whitespace to one line.
fn clean(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn status_glyph(state: &str) -> (&'static str, ThemeColor) {
    match state {
        "needs_you" => ("!", ThemeColor::Warning),
        "running" => ("●", ThemeColor::Accent),
        "ready" => ("✓", ThemeColor::Success),
        "done" => ("·", ThemeColor::Dim),
        "failed" => ("×", ThemeColor::Error),
        _ => ("?", ThemeColor::Dim),
    }
}

fn summary_color(state: &str) -> ThemeColor {
    match state {
        "needs you" => ThemeColor::Warning,
        "running" => ThemeColor::Accent,
        "paused" | "stopping" => ThemeColor::Error,
        _ => ThemeColor::Dim,
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut rows = Vec::new();
    for paragraph in text.lines() {
        let mut row = String::new();
        for word in paragraph.split_whitespace() {
            if !row.is_empty() && str_width(&row) + 1 + str_width(word) > width {
                rows.push(std::mem::take(&mut row));
            }
            if !row.is_empty() {
                row.push(' ');
            }
            row.push_str(word);
        }
        rows.push(row);
    }
    rows
}

impl CaptainView {
    pub(crate) fn new(viewport_rows: usize) -> Self {
        Self {
            snapshot: CaptainSnapshot::default(),
            loaded: false,
            error: None,
            snapshot_error: None,
            notice: None,
            project: 0,
            member: 0,
            focus: Focus::Projects,
            expanded: false,
            help: false,
            mode: Mode::Browse,
            viewport_rows: viewport_rows.max(12),
            pending_setup: None,
            pending_launch: false,
            machine_params: std::collections::BTreeMap::new(),
        }
    }

    pub(crate) fn set_viewport_rows(&mut self, rows: usize) {
        self.viewport_rows = rows.max(10);
    }

    /// Apply a fresh status reply; the selection follows the same project and member ids.
    pub(crate) fn apply_snapshot(&mut self, data: &Value) {
        let selected_project = self.selected_project().map(|p| p.id.clone());
        let selected_member = self.selected_member().map(|m| m.id.clone());
        match serde_json::from_value::<CaptainSnapshot>(data.clone()) {
            Ok(snapshot) => {
                let first = !self.loaded;
                self.snapshot = snapshot;
                self.loaded = true;
                self.snapshot_error = None;
                let target = if first {
                    let here = &self.snapshot.current_root;
                    self.snapshot
                        .projects
                        .iter()
                        .position(|p| &p.root == here)
                        .or_else(|| {
                            let current = self.snapshot.current.as_ref()?;
                            self.snapshot.projects.iter().position(|p| &p.id == current)
                        })
                } else {
                    selected_project
                        .and_then(|id| self.snapshot.projects.iter().position(|p| p.id == id))
                };
                self.project = target
                    .unwrap_or(self.project)
                    .min(self.snapshot.projects.len().saturating_sub(1));
                if let (Some(id), Some(project)) = (selected_member, self.selected_project()) {
                    if let Some(index) = project.flat_crew().iter().position(|m| m.id == id) {
                        self.member = index;
                    }
                }
                self.clamp_member();
            }
            Err(error) => {
                self.snapshot_error = Some(format!("Captain returned invalid status: {error}"));
            }
        }
    }

    pub(crate) fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }

    pub(crate) fn set_snapshot_error(&mut self, error: String) {
        self.snapshot_error = Some(error);
    }

    pub(crate) fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    /// Focus a project by name, id, or root (the `/captain <name>` argument).
    pub(crate) fn focus_project(&mut self, query: &str) -> bool {
        let term = query.trim().to_lowercase();
        let found = self.snapshot.projects.iter().position(|p| {
            p.id == term || p.name.to_lowercase() == term || p.root.to_lowercase() == term
        });
        let found = found.or_else(|| {
            self.snapshot
                .projects
                .iter()
                .position(|p| p.name.to_lowercase().contains(&term))
        });
        if let Some(index) = found {
            self.project = index;
            self.focus = Focus::Projects;
            self.clamp_member();
        }
        found.is_some()
    }

    pub(crate) fn snapshot(&self) -> &CaptainSnapshot {
        &self.snapshot
    }

    pub(crate) fn selected_project(&self) -> Option<&CaptainProject> {
        self.snapshot.projects.get(self.project)
    }

    fn selected_member(&self) -> Option<&CaptainMember> {
        if self.focus != Focus::Crew {
            return None;
        }
        self.selected_project()?
            .flat_crew()
            .get(self.member)
            .copied()
    }

    fn clamp_member(&mut self) {
        let count = self.selected_project().map_or(0, |p| p.flat_crew().len());
        self.member = self.member.min(count.saturating_sub(1));
        if count == 0 && self.focus == Focus::Crew {
            self.focus = Focus::Projects;
        }
    }

    /// The project at the current checkout, if it has a Captain.
    fn here_has_captain(&self) -> bool {
        self.snapshot
            .projects
            .iter()
            .any(|p| p.root == self.snapshot.current_root)
    }

    /// Start the new-project form; `reference` promotes that conversation instead.
    pub(crate) fn start_setup(&mut self, reference: Option<String>) -> CaptainAction {
        if self.here_has_captain() {
            self.notice = Some("This project already has a Captain.".into());
            return CaptainAction::None;
        }
        self.notice = Some("Loading models and machines…".into());
        self.pending_setup = Some(Setup {
            reference: reference.clone(),
            parallel: 2,
            provider: "prime".into(),
            ..Setup::default()
        });
        CaptainAction::LoadChoices {
            project: None,
            reference,
        }
    }

    /// The backend's form options arrived.
    pub(crate) fn apply_choices(&mut self, data: &Value, project: Option<&str>) -> CaptainAction {
        let choices: CaptainChoices = match serde_json::from_value(data.clone()) {
            Ok(choices) => choices,
            Err(error) => {
                self.error = Some(format!("Captain returned invalid options: {error}"));
                return CaptainAction::None;
            }
        };
        if std::mem::take(&mut self.pending_launch) {
            self.machine_params = choices.machine_params.clone();
            let items = choices
                .machines
                .iter()
                .map(|name| PickItem {
                    label: name.clone(),
                    value: json!(name),
                })
                .collect::<Vec<_>>();
            if items.is_empty() {
                self.notice = Some("No machines installed.".into());
                return CaptainAction::Close;
            }
            self.mode = Mode::Pick(Picker::new(
                "launch a factory run",
                items,
                PickPurpose::Launch,
            ));
            return CaptainAction::None;
        }
        if let Some(mut setup) = self.pending_setup.take() {
            self.notice = None;
            setup.brief = choices.brief.clone().unwrap_or_default();
            setup.choices = choices;
            return self.setup_step(setup);
        }
        let Some(project) = project.map(str::to_owned) else {
            return CaptainAction::None;
        };
        if let Mode::Pick(picker) = &self.mode {
            match picker.purpose {
                PickPurpose::Model { .. } => self.open_model_picker(project, &choices),
                PickPurpose::Delegate { .. } => self.open_delegate_picker(project, &choices),
                PickPurpose::Adopt { .. } => self.open_adopt_picker(project, &choices),
                _ => {}
            }
        }
        CaptainAction::None
    }

    fn open_model_picker(&mut self, project: String, choices: &CaptainChoices) {
        let items = choices
            .models
            .iter()
            .map(|m| PickItem {
                label: format!(
                    "{} · {}",
                    m.provider,
                    if m.model.is_empty() {
                        "default model"
                    } else {
                        &m.model
                    }
                ),
                value: json!([m.provider, m.model]),
            })
            .collect();
        let mut picker = Picker::new("captain model", items, PickPurpose::Model { project });
        if let Some(p) = self.selected_project() {
            picker.select_value(&json!([p.provider, p.model]));
        }
        self.mode = Mode::Pick(picker);
    }

    fn open_delegate_picker(&mut self, project: String, choices: &CaptainChoices) {
        let mut items: Vec<PickItem> = choices
            .crew_models
            .iter()
            .map(|m| PickItem {
                label: format!("thread · {}", m.label),
                value: json!({"model": m.model}),
            })
            .collect();
        items.extend(choices.machines.iter().map(|name| PickItem {
            label: format!("factory · {name}"),
            value: json!({"machine": name}),
        }));
        items.extend(choices.project_factories.iter().map(|name| PickItem {
            label: format!("project factory · {name}"),
            value: json!({"machine": name}),
        }));
        self.mode = Mode::Pick(Picker::new(
            "assign work",
            items,
            PickPurpose::Delegate { project },
        ));
    }

    fn open_adopt_picker(&mut self, project: String, choices: &CaptainChoices) {
        let mut items: Vec<PickItem> = choices
            .adopt
            .iter()
            .map(|a| PickItem {
                label: clean(&a.label),
                value: json!(a.reference),
            })
            .collect();
        items.push(PickItem {
            label: "enter a thread ID or session path".into(),
            value: Value::Null,
        });
        self.mode = Mode::Pick(Picker::new(
            "add a thread",
            items,
            PickPurpose::Adopt { project },
        ));
    }

    /// Show the setup step `setup.step`; text steps go to pa's editor.
    fn setup_step(&mut self, setup: Setup) -> CaptainAction {
        let step = SETUP_STEPS.get(setup.step).copied().unwrap_or("confirm");
        let boxed = |s: &Setup| PickPurpose::Setup(Box::new(s.clone()));
        let title = |name: &str| {
            format!(
                "{} · {name}",
                if setup.reference.is_some() {
                    "promote"
                } else {
                    "new captain"
                }
            )
        };
        match step {
            "brief" => {
                let initial = setup.brief.clone();
                self.pending_setup = Some(setup);
                CaptainAction::Compose {
                    purpose: ComposePurpose::Brief,
                    label: "captain brief · what should this project achieve".into(),
                    initial,
                }
            }
            "model" => {
                let items = setup
                    .choices
                    .models
                    .iter()
                    .map(|m| PickItem {
                        label: format!(
                            "{} · {}",
                            m.provider,
                            if m.model.is_empty() {
                                "default model"
                            } else {
                                &m.model
                            }
                        ),
                        value: json!([m.provider, m.model]),
                    })
                    .collect();
                let mut picker = Picker::new(&title("model"), items, boxed(&setup));
                picker.select_value(&json!([setup.provider, setup.model]));
                self.mode = Mode::Pick(picker);
                CaptainAction::None
            }
            "thinking" => {
                let items = setup
                    .choices
                    .thinking
                    .iter()
                    .map(|level| PickItem {
                        label: level.clone(),
                        value: json!(if level == "default" {
                            ""
                        } else {
                            level.as_str()
                        }),
                    })
                    .collect();
                let mut picker = Picker::new(&title("thinking"), items, boxed(&setup));
                picker.select_value(&json!(setup.thinking));
                self.mode = Mode::Pick(picker);
                CaptainAction::None
            }
            "factory" => {
                let mut items = vec![PickItem {
                    label: "Design a factory for this project".into(),
                    value: json!({"design": true}),
                }];
                items.extend(setup.choices.machines.iter().map(|m| PickItem {
                    label: m.clone(),
                    value: json!(m),
                }));
                items.push(PickItem {
                    label: "No factory".into(),
                    value: Value::Null,
                });
                let mut picker = Picker::new(&title("factory"), items, boxed(&setup));
                if setup.factory_chosen {
                    picker.select_value(&setup.factory_value());
                }
                self.mode = Mode::Pick(picker);
                CaptainAction::None
            }
            "workers" => {
                let items = [1u32, 2, 3, 4, 6, 8, 12, 16]
                    .iter()
                    .map(|n| PickItem {
                        label: format!("{n} parallel crew turns"),
                        value: json!(n),
                    })
                    .collect();
                let mut picker = Picker::new(&title("worker limit"), items, boxed(&setup));
                picker.select_value(&json!(setup.parallel));
                self.mode = Mode::Pick(picker);
                CaptainAction::None
            }
            "threads" => {
                let items = setup
                    .choices
                    .adopt
                    .iter()
                    .map(|a| PickItem {
                        label: clean(&a.label),
                        value: json!(a.reference),
                    })
                    .collect::<Vec<_>>();
                let mut picker = Picker::new(&title("add existing threads"), items, boxed(&setup));
                picker.multi = true;
                picker.marked = picker
                    .items
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| {
                        item.value
                            .as_str()
                            .is_some_and(|r| setup.crew.iter().any(|c| c == r))
                    })
                    .map(|(i, _)| i)
                    .collect();
                self.mode = Mode::Pick(picker);
                CaptainAction::None
            }
            _ => {
                self.mode = Mode::Confirm(Confirm::Create(Box::new(setup)));
                CaptainAction::None
            }
        }
    }

    /// The factory page's new run: pick a machine, then enter its parameters in pa's editor.
    pub(crate) fn start_launch(&mut self) -> CaptainAction {
        self.pending_launch = true;
        self.mode = Mode::Pick(Picker::new(
            "launch a factory run",
            Vec::new(),
            PickPurpose::Launch,
        ));
        CaptainAction::LoadChoices {
            project: None,
            reference: None,
        }
    }

    /// The next launch step: a parameter in pa's editor, or the finished draft.
    fn launch_step(launch: Launch) -> CaptainAction {
        use std::fmt::Write as _;
        let Some((name, kind, required, description)) = launch.params.get(launch.index) else {
            let params: serde_json::Map<String, Value> = launch
                .values
                .into_iter()
                .map(|(name, value)| (name, Value::String(value)))
                .collect();
            return CaptainAction::Act {
                op: "launch".into(),
                args: json!({"machine": launch.machine, "params": params}),
                done: String::new(),
            };
        };
        let mut label = format!("{} · {name}", launch.machine);
        if kind != "text" {
            let _ = write!(label, " ({kind}, JSON)");
        }
        if !description.is_empty() {
            let _ = write!(label, " · {}", clean(description));
        }
        if !required {
            label.push_str(" · empty keeps the default");
        }
        CaptainAction::Compose {
            purpose: ComposePurpose::LaunchParam(Box::new(launch)),
            label,
            initial: String::new(),
        }
    }

    /// Text from pa's editor for a compose step; `None` when the user cancelled.
    pub(crate) fn compose_done(
        &mut self,
        purpose: ComposePurpose,
        text: Option<String>,
    ) -> CaptainAction {
        if let ComposePurpose::LaunchParam(launch) = &purpose {
            let mut launch = (**launch).clone();
            let Some(text) = text.map(|t| t.trim().to_owned()) else {
                self.notice = Some("Launch cancelled.".into());
                return CaptainAction::Close;
            };
            let required = launch.params[launch.index].2;
            if text.is_empty() && required {
                self.notice = Some(format!(
                    "{} requires a value.",
                    launch.params[launch.index].0
                ));
                return Self::launch_step(launch);
            }
            let name = launch.params[launch.index].0.clone();
            launch.values.push((name, text));
            launch.index += 1;
            return Self::launch_step(launch);
        }
        let Some(text) = text.map(|t| t.trim().to_owned()) else {
            if purpose == ComposePurpose::Brief {
                if let Some(mut setup) = self.pending_setup.take() {
                    if setup.step > 0 {
                        setup.step -= 1;
                        return self.setup_step(setup);
                    }
                }
                self.mode = Mode::Browse;
                self.notice = Some("Setup cancelled.".into());
            }
            return CaptainAction::None;
        };
        if text.is_empty() && !matches!(purpose, ComposePurpose::EditText { .. }) {
            self.notice = Some("Nothing entered.".into());
            if purpose == ComposePurpose::Brief {
                if let Some(setup) = self.pending_setup.take() {
                    return self.setup_step(setup);
                }
            }
            return CaptainAction::None;
        }
        match purpose {
            ComposePurpose::Brief => {
                let Some(mut setup) = self.pending_setup.take() else {
                    return CaptainAction::None;
                };
                setup.brief = text;
                setup.step = 1;
                self.setup_step(setup)
            }
            ComposePurpose::Message { project, member } => {
                let (op, args) = match &member {
                    Some(member) => (
                        "reply",
                        json!({"project": project, "member": member, "text": text}),
                    ),
                    None => ("lead", json!({"project": project, "text": text})),
                };
                CaptainAction::Act {
                    op: op.into(),
                    args,
                    done: "Message sent.".into(),
                }
            }
            ComposePurpose::Answer { project, member } => CaptainAction::Act {
                op: "answer".into(),
                args: json!({"project": project, "member": member, "text": text, "decision": "resume"}),
                done: "Answer sent. The run resumes.".into(),
            },
            ComposePurpose::EditText { project, field } => CaptainAction::Act {
                op: "edit".into(),
                args: json!({"project": project, "field": field, "value": text}),
                done: format!("Updated {}.", field.replace('_', " ")),
            },
            ComposePurpose::DelegateTask { project, worker } => {
                let args = match worker {
                    Worker::Model { model, write } => json!({
                        "project": project, "task": text, "provider": "prime",
                        "model": model, "write": write,
                    }),
                    Worker::Machine(machine) => {
                        json!({"project": project, "task": text, "machine": machine})
                    }
                };
                CaptainAction::Act {
                    op: "delegate".into(),
                    args,
                    done: "Assignment started.".into(),
                }
            }
            ComposePurpose::Consensus { project } => CaptainAction::Act {
                op: "consensus".into(),
                args: json!({"project": project, "task": text}),
                done: String::new(),
            },
            ComposePurpose::LaunchParam(_) => CaptainAction::None,
            ComposePurpose::AdoptRef { project } => CaptainAction::Act {
                op: "adopt".into(),
                args: json!({"project": project, "ref": text}),
                done: "Thread added to crew.".into(),
            },
        }
    }

    /// Open the read-and-reply view for a provider CLI thread.
    pub(crate) fn open_thread(&mut self, project: String, member: String, data: &Value) {
        let messages = data
            .get("messages")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| {
                        Some((
                            row.get(0)?.as_str()?.to_owned(),
                            row.get(1)?.as_str()?.to_owned(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let title = self
            .selected_project()
            .and_then(|p| p.crew.iter().find(|m| m.id == member))
            .map_or_else(|| "captain".into(), CaptainMember::label);
        // A refresh of the open thread keeps its scroll; a new thread starts at the end.
        let scroll = match &self.mode {
            Mode::Thread(open) if open.member == member => open.scroll,
            _ => {
                self.notice = None;
                0
            }
        };
        self.mode = Mode::Thread(ThreadView {
            project,
            member,
            title,
            provider: data["provider"].as_str().unwrap_or_default().to_owned(),
            messages,
            busy: data["busy"].as_bool().unwrap_or(false),
            scroll,
        });
    }

    /// The open thread view's member, for its refresh.
    pub(crate) fn open_thread_member(&self) -> Option<(String, String)> {
        match &self.mode {
            Mode::Thread(thread) => Some((thread.project.clone(), thread.member.clone())),
            _ => None,
        }
    }

    /// Open or refresh the review page of a project factory from its `factory-show` reply.
    /// `explicit` is a load the user asked for; a background refresh keeps the reviewed digest.
    pub(crate) fn open_factory(&mut self, project: String, data: &Value, explicit: bool) {
        let text = |key: &str| data[key].as_str().unwrap_or_default().to_owned();
        let name = text("name");
        let (scroll, reviewed) = match &self.mode {
            Mode::Factory(open) if open.name == name => {
                (open.scroll, (!explicit).then(|| open.reviewed.clone()))
            }
            _ if !explicit => return,
            _ => {
                self.notice = None;
                (0, None)
            }
        };
        let reviewed = reviewed.unwrap_or_else(|| text("digest"));
        self.mode = Mode::Factory(FactoryReview {
            project,
            state: text("state"),
            approved: data["approved"].as_bool().unwrap_or(false),
            path: text("path"),
            file: text("file"),
            digest: text("digest"),
            reviewed,
            errors: data["errors"]
                .as_array()
                .map(|rows| {
                    rows.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            graph: data.get("graph").filter(|g| g.is_object()).cloned(),
            graph_error: data["graph_error"].as_str().map(str::to_owned),
            live: data["live"].as_bool().unwrap_or(false),
            name,
            scroll,
        });
    }

    /// The open factory review page, for its refresh.
    pub(crate) fn open_factory_member(&self) -> Option<(String, String)> {
        match &self.mode {
            Mode::Factory(review) => Some((review.project.clone(), review.name.clone())),
            _ => None,
        }
    }

    pub(crate) fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> CaptainAction {
        let mode = std::mem::replace(&mut self.mode, Mode::Browse);
        let (mode, action) = match mode {
            Mode::Browse => (Mode::Browse, self.browse_key(key, kb)),
            Mode::Pick(picker) => self.picker_key(picker, key, kb),
            Mode::Confirm(confirm) => self.confirm_key(confirm, key, kb),
            Mode::Thread(thread) => self.thread_key(thread, key, kb),
            Mode::Factory(review) => self.factory_key(review, key, kb),
        };
        // An action may already have opened a new mode (a setup step).
        if matches!(self.mode, Mode::Browse) {
            self.mode = mode;
        }
        action
    }

    fn browse_key(&mut self, key: &str, kb: &KeybindingsManager) -> CaptainAction {
        self.notice = None;
        self.error = None;
        if kb.matches(key, "app.factory.toggle") || key == "q" || key == "ctrl+c" {
            return CaptainAction::Close;
        }
        if kb.matches(key, "tui.select.cancel") {
            if self.help {
                self.help = false;
                return CaptainAction::None;
            }
            return CaptainAction::Close;
        }
        match key {
            "?" => {
                self.help = !self.help;
                return CaptainAction::None;
            }
            "r" => return CaptainAction::Refresh,
            "tab" | "shift+tab" => {
                let has_crew = self.selected_project().is_some_and(|p| !p.crew.is_empty());
                self.focus = match self.focus {
                    Focus::Projects if has_crew => Focus::Crew,
                    _ => Focus::Projects,
                };
                return CaptainAction::None;
            }
            "x" => {
                self.expanded = !self.expanded;
                return CaptainAction::None;
            }
            "n" => return self.start_setup(None),
            "p" => return self.start_setup(Some("current".into())),
            _ => {}
        }
        if kb.matches(key, "tui.select.up") || key == "k" {
            self.move_selection(-1);
            return CaptainAction::None;
        }
        if kb.matches(key, "tui.select.down") || key == "j" {
            self.move_selection(1);
            return CaptainAction::None;
        }
        if key == "left" || key == "h" {
            self.focus = Focus::Projects;
            self.move_selection(-1);
            return CaptainAction::None;
        }
        if key == "right" || key == "l" {
            self.focus = Focus::Projects;
            self.move_selection(1);
            return CaptainAction::None;
        }
        let Some(project) = self.selected_project().cloned() else {
            if key == "f" {
                return CaptainAction::Act {
                    op: "factory".into(),
                    args: json!({}),
                    done: String::new(),
                };
            }
            return CaptainAction::None;
        };
        let member = self.selected_member().cloned();
        let pid = project.id.clone();
        match key {
            "s" => {
                if !project.is_active() {
                    self.notice = Some(format!("This Captain is {}.", project.display_state()));
                    return CaptainAction::None;
                }
                self.mode = Mode::Confirm(Confirm::Stop {
                    project: pid,
                    name: project.name.clone(),
                });
                CaptainAction::None
            }
            "R" => CaptainAction::Act {
                op: "resume".into(),
                args: json!({"project": pid}),
                done: "Captain resumed.".into(),
            },
            "D" => {
                let running = usize::from(project.lead_busy)
                    + project
                        .crew
                        .iter()
                        .filter(|m| m.busy || m.state == "running")
                        .count();
                self.mode = Mode::Confirm(Confirm::Delete {
                    project: pid,
                    name: project.name.clone(),
                    crew: project.crew.iter().filter(|m| !m.is_run()).count(),
                    running,
                });
                CaptainAction::None
            }
            "e" => {
                let items = [
                    "objective",
                    "plan",
                    "rules",
                    "crew_rules",
                    "digest",
                    "name",
                    "model",
                    "thinking",
                    "parallel",
                ]
                .iter()
                .map(|field| PickItem {
                    label: field.replace('_', " "),
                    value: json!(field),
                })
                .collect();
                self.mode = Mode::Pick(Picker::new(
                    "edit",
                    items,
                    PickPurpose::EditField { project: pid },
                ));
                CaptainAction::None
            }
            "d" => {
                if !project.is_active() {
                    self.notice = Some("Resume this Captain to assign work.".into());
                    return CaptainAction::None;
                }
                self.mode = Mode::Pick(Picker::new(
                    "assign work",
                    Vec::new(),
                    PickPurpose::Delegate {
                        project: pid.clone(),
                    },
                ));
                CaptainAction::LoadChoices {
                    project: Some(pid),
                    reference: None,
                }
            }
            "a" => {
                self.mode = Mode::Pick(Picker::new(
                    "add a thread",
                    Vec::new(),
                    PickPurpose::Adopt {
                        project: pid.clone(),
                    },
                ));
                CaptainAction::LoadChoices {
                    project: Some(pid),
                    reference: None,
                }
            }
            "m" => match &member {
                Some(m) if m.is_run() => {
                    if m.run_state.as_deref() == Some("paused") && m.headless {
                        CaptainAction::Compose {
                            purpose: ComposePurpose::Answer {
                                project: pid,
                                member: m.id.clone(),
                            },
                            label: format!("answer {} · the run resumes with this note", m.label()),
                            initial: String::new(),
                        }
                    } else {
                        self.notice = Some("Only a paused run takes an answer.".into());
                        CaptainAction::None
                    }
                }
                _ => {
                    if !project.is_active() {
                        self.notice = Some("Resume this Captain to send messages.".into());
                        return CaptainAction::None;
                    }
                    let to = member
                        .as_ref()
                        .map_or("captain".into(), CaptainMember::label);
                    CaptainAction::Compose {
                        purpose: ComposePurpose::Message {
                            project: pid,
                            member: member.as_ref().map(|m| m.id.clone()),
                        },
                        label: format!("message {to}"),
                        initial: String::new(),
                    }
                }
            },
            "F" | "shift+f" => {
                let mut items: Vec<PickItem> = project
                    .factories
                    .iter()
                    .map(|f| PickItem {
                        label: format!(
                            "{} · {}",
                            clean(&f.name),
                            factory_state_label(&f.state, f.approved)
                        ),
                        value: json!({"name": f.name}),
                    })
                    .collect();
                items.push(PickItem {
                    label: "Design a factory for this project".into(),
                    value: json!({"design": true}),
                });
                self.mode = Mode::Pick(Picker::new(
                    "project factories",
                    items,
                    PickPurpose::Factories {
                        project: pid,
                        request: uuid::Uuid::new_v4().to_string(),
                    },
                ));
                CaptainAction::None
            }
            "c" => CaptainAction::Compose {
                purpose: ComposePurpose::Consensus { project: pid },
                label: "consensus · the decision both models should vote on".into(),
                initial: String::new(),
            },
            "f" => CaptainAction::Act {
                op: "factory".into(),
                args: json!({"project": pid}),
                done: String::new(),
            },
            "u" => match member {
                Some(m) if !m.is_run() => {
                    self.mode = Mode::Confirm(Confirm::Release {
                        project: pid,
                        member: m.id.clone(),
                        title: m.label(),
                    });
                    CaptainAction::None
                }
                _ => CaptainAction::None,
            },
            "v" => match member {
                Some(m) if m.kind == "thread" => CaptainAction::Review {
                    project: pid,
                    member: m.id,
                },
                _ => {
                    self.notice = Some("Select a crew thread to review its checkout.".into());
                    CaptainAction::None
                }
            },
            "o" | "enter" => self.open_selected(&project, member.as_ref()),
            _ if kb.matches(key, "tui.select.confirm") => {
                self.open_selected(&project, member.as_ref())
            }
            _ => CaptainAction::None,
        }
    }

    fn open_selected(
        &mut self,
        project: &CaptainProject,
        member: Option<&CaptainMember>,
    ) -> CaptainAction {
        if let Some(m) = member.filter(|m| m.is_run() && m.headless) {
            // A Captain-started run has no chat; its control is the factory page.
            self.notice = Some(if m.run_ended() {
                format!(
                    "{} {}.",
                    m.label(),
                    m.run_state.as_deref().unwrap_or("ended")
                )
            } else {
                format!("{} runs headless. Press m to answer a pause.", m.label())
            });
            return CaptainAction::None;
        }
        CaptainAction::Open {
            project: project.id.clone(),
            member: member.map(|m| m.id.clone()),
        }
    }

    fn move_selection(&mut self, delta: isize) {
        match self.focus {
            Focus::Projects => {
                let count = self.snapshot.projects.len();
                if count == 0 {
                    return;
                }
                self.project =
                    (self.project as isize + delta).clamp(0, count as isize - 1) as usize;
                self.member = 0;
            }
            Focus::Crew => {
                let count = self.selected_project().map_or(0, |p| p.flat_crew().len());
                if count == 0 {
                    return;
                }
                self.member = (self.member as isize + delta).clamp(0, count as isize - 1) as usize;
            }
        }
    }

    fn picker_key(
        &mut self,
        mut picker: Picker,
        key: &str,
        kb: &KeybindingsManager,
    ) -> (Mode, CaptainAction) {
        if kb.matches(key, "tui.select.cancel") || key == "ctrl+c" {
            if picker.purpose == PickPurpose::Launch {
                return (Mode::Browse, CaptainAction::Close);
            }
            if let PickPurpose::Setup(setup) = &picker.purpose {
                let mut setup = (**setup).clone();
                if setup.step > 0 {
                    setup.step -= 1;
                    let action = self.setup_step(setup);
                    return (std::mem::replace(&mut self.mode, Mode::Browse), action);
                }
            }
            self.notice = Some("Cancelled.".into());
            return (Mode::Browse, CaptainAction::None);
        }
        let visible = picker.visible();
        if kb.matches(key, "tui.select.up") {
            picker.selected = picker.selected.saturating_sub(1);
            return (Mode::Pick(picker), CaptainAction::None);
        }
        if kb.matches(key, "tui.select.down") {
            picker.selected = (picker.selected + 1).min(visible.len().saturating_sub(1));
            return (Mode::Pick(picker), CaptainAction::None);
        }
        if picker.multi && key == "space" {
            if let Some(index) = picker.current() {
                if let Some(at) = picker.marked.iter().position(|&m| m == index) {
                    picker.marked.remove(at);
                } else {
                    picker.marked.push(index);
                }
            }
            return (Mode::Pick(picker), CaptainAction::None);
        }
        if key == "backspace" {
            picker.filter.pop();
            picker.selected = 0;
            return (Mode::Pick(picker), CaptainAction::None);
        }
        if kb.matches(key, "tui.select.confirm") || key == "enter" {
            let value = picker.current().map(|i| picker.items[i].value.clone());
            if value.is_none() && !picker.multi {
                return (Mode::Pick(picker), CaptainAction::None);
            }
            let value = value.unwrap_or(Value::Null);
            let action = self.pick(&picker, &value);
            return (std::mem::replace(&mut self.mode, Mode::Browse), action);
        }
        let mut chars = key.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if !c.is_control() {
                picker.filter.push(c);
                picker.selected = 0;
            }
        } else if let Some(rest) = key.strip_prefix("shift+") {
            if rest.chars().count() == 1 {
                picker.filter.push_str(&rest.to_uppercase());
                picker.selected = 0;
            }
        }
        (Mode::Pick(picker), CaptainAction::None)
    }

    /// The picker's Enter.
    fn pick(&mut self, picker: &Picker, value: &Value) -> CaptainAction {
        match picker.purpose.clone() {
            PickPurpose::EditField { project } => {
                let field = value.as_str().unwrap_or_default().to_owned();
                match field.as_str() {
                    "model" => {
                        self.mode = Mode::Pick(Picker::new(
                            "captain model",
                            Vec::new(),
                            PickPurpose::Model {
                                project: project.clone(),
                            },
                        ));
                        CaptainAction::LoadChoices {
                            project: Some(project),
                            reference: None,
                        }
                    }
                    "thinking" => {
                        let items = [
                            "default", "off", "minimal", "low", "medium", "high", "xhigh", "max",
                        ]
                        .iter()
                        .map(|l| PickItem {
                            label: (*l).into(),
                            value: json!(l),
                        })
                        .collect();
                        let mut next = Picker::new(
                            "captain thinking",
                            items,
                            PickPurpose::Thinking { project },
                        );
                        if let Some(p) = self.selected_project() {
                            next.select_value(&json!(if p.thinking.is_empty() {
                                "default"
                            } else {
                                &p.thinking
                            }));
                        }
                        self.mode = Mode::Pick(next);
                        CaptainAction::None
                    }
                    "parallel" => {
                        let items = [1u32, 2, 3, 4, 6, 8, 12, 16]
                            .iter()
                            .map(|n| PickItem {
                                label: format!("{n} parallel crew turns"),
                                value: json!(n.to_string()),
                            })
                            .collect();
                        let mut next =
                            Picker::new("worker limit", items, PickPurpose::Workers { project });
                        if let Some(p) = self.selected_project() {
                            next.select_value(&json!(p.parallel.to_string()));
                        }
                        self.mode = Mode::Pick(next);
                        CaptainAction::None
                    }
                    _ => {
                        let initial = self
                            .selected_project()
                            .map(|p| match field.as_str() {
                                "objective" => p.objective.clone(),
                                "plan" => p.plan.clone(),
                                "rules" => p.rules.clone(),
                                "crew_rules" => p.crew_rules.clone(),
                                "digest" => p.digest.clone(),
                                "name" => p.name.clone(),
                                _ => String::new(),
                            })
                            .unwrap_or_default();
                        CaptainAction::Compose {
                            label: format!("edit {}", field.replace('_', " ")),
                            purpose: ComposePurpose::EditText { project, field },
                            initial,
                        }
                    }
                }
            }
            PickPurpose::Model { project } => CaptainAction::Act {
                op: "edit".into(),
                args: json!({"project": project, "field": "model", "provider": value[0], "value": value[1]}),
                done: "Captain model updated.".into(),
            },
            PickPurpose::Thinking { project } => CaptainAction::Act {
                op: "edit".into(),
                args: json!({"project": project, "field": "thinking", "value": value}),
                done: "Captain thinking updated.".into(),
            },
            PickPurpose::Workers { project } => CaptainAction::Act {
                op: "edit".into(),
                args: json!({"project": project, "field": "parallel", "value": value}),
                done: "Worker limit updated.".into(),
            },
            PickPurpose::Delegate { project } => {
                if let Some(machine) = value.get("machine").and_then(Value::as_str) {
                    return CaptainAction::Compose {
                        label: format!("assignment for {machine}"),
                        purpose: ComposePurpose::DelegateTask {
                            project,
                            worker: Worker::Machine(machine.into()),
                        },
                        initial: String::new(),
                    };
                }
                let model = value["model"].as_str().unwrap_or_default().to_owned();
                let items = vec![
                    PickItem {
                        label: "read · works in the shared checkout".into(),
                        value: json!(false),
                    },
                    PickItem {
                        label: "write · works in its own worktree".into(),
                        value: json!(true),
                    },
                ];
                self.mode = Mode::Pick(Picker::new(
                    "access",
                    items,
                    PickPurpose::DelegateWrite { project, model },
                ));
                CaptainAction::None
            }
            PickPurpose::DelegateWrite { project, model } => CaptainAction::Compose {
                label: "assignment for the new crew thread".into(),
                purpose: ComposePurpose::DelegateTask {
                    project,
                    worker: Worker::Model {
                        model,
                        write: value.as_bool().unwrap_or(false),
                    },
                },
                initial: String::new(),
            },
            PickPurpose::Adopt { project } => match value.as_str() {
                Some(reference) => CaptainAction::Act {
                    op: "adopt".into(),
                    args: json!({"project": project, "ref": reference}),
                    done: "Thread added to crew.".into(),
                },
                None => CaptainAction::Compose {
                    purpose: ComposePurpose::AdoptRef { project },
                    label: "thread ID or native session path".into(),
                    initial: String::new(),
                },
            },
            PickPurpose::Launch => {
                let machine = value.as_str().unwrap_or_default().to_owned();
                let params = self
                    .machine_params
                    .get(&machine)
                    .cloned()
                    .unwrap_or_default();
                if let Some(error) = params.iter().find_map(|p| p.error.clone()) {
                    self.error = Some(error);
                    return CaptainAction::None;
                }
                Self::launch_step(Launch {
                    machine,
                    params: params
                        .into_iter()
                        .map(|p| (p.name, p.kind, p.required, p.description))
                        .collect(),
                    index: 0,
                    values: Vec::new(),
                })
            }
            PickPurpose::Factories { project, request } => {
                if value.get("design").is_some() {
                    return CaptainAction::Act {
                        op: "factory-design".into(),
                        args: json!({"project": project, "request_id": request}),
                        done: "Design requested. The Captain writes a draft for review.".into(),
                    };
                }
                let name = value["name"].as_str().unwrap_or_default().to_owned();
                self.notice = Some(format!("Loading {name}…"));
                CaptainAction::LoadFactory { project, name }
            }
            PickPurpose::Setup(setup) => {
                let mut setup = *setup;
                match SETUP_STEPS[setup.step] {
                    "model" => {
                        setup.provider = value[0].as_str().unwrap_or("prime").into();
                        setup.model = value[1].as_str().unwrap_or_default().into();
                    }
                    "thinking" => setup.thinking = value.as_str().unwrap_or_default().into(),
                    "factory" => {
                        setup.factory_chosen = true;
                        setup.design = value.get("design").and_then(Value::as_bool) == Some(true);
                        setup.machine = value.as_str().map(str::to_owned);
                    }
                    "workers" => setup.parallel = value.as_u64().unwrap_or(2) as u32,
                    "threads" => {
                        setup.crew = picker
                            .marked
                            .iter()
                            .filter_map(|&i| picker.items[i].value.as_str().map(str::to_owned))
                            .collect();
                    }
                    _ => {}
                }
                setup.step += 1;
                self.setup_step(setup)
            }
        }
    }

    fn confirm_key(
        &mut self,
        confirm: Confirm,
        key: &str,
        kb: &KeybindingsManager,
    ) -> (Mode, CaptainAction) {
        let yes = key == "y" || key == "enter" || kb.matches(key, "tui.select.confirm");
        let no = key == "n" || key == "ctrl+c" || kb.matches(key, "tui.select.cancel");
        if no {
            if let Confirm::DeleteFactory(review) = confirm {
                return (Mode::Factory(*review), CaptainAction::None);
            }
            if let Confirm::Create(setup) = confirm {
                let mut setup = *setup;
                setup.step = SETUP_STEPS.len() - 2;
                let action = self.setup_step(setup);
                return (std::mem::replace(&mut self.mode, Mode::Browse), action);
            }
            self.notice = Some("Cancelled.".into());
            return (Mode::Browse, CaptainAction::None);
        }
        if !yes {
            return (Mode::Confirm(confirm), CaptainAction::None);
        }
        let action = match confirm {
            Confirm::Stop { project, .. } => CaptainAction::Act {
                op: "stop".into(),
                args: json!({"project": project}),
                done: "Captain stopped. Press R to resume.".into(),
            },
            Confirm::Delete { project, .. } => CaptainAction::Act {
                op: "delete".into(),
                args: json!({"project": project}),
                done: "Captain deleted. Its threads stay in the thread list.".into(),
            },
            Confirm::Release {
                project, member, ..
            } => CaptainAction::Act {
                op: "release".into(),
                args: json!({"project": project, "member": member}),
                done: "Thread released to the thread list.".into(),
            },
            Confirm::DeleteFactory(review) => CaptainAction::Act {
                op: "factory-delete".into(),
                args: json!({"project": review.project, "name": review.name}),
                done: format!("Deleted the factory {}.", review.name),
            },
            Confirm::Create(setup) => {
                let op = if setup.reference.is_some() {
                    "promote"
                } else {
                    "init"
                };
                CaptainAction::Act {
                    op: op.into(),
                    args: setup.request(),
                    done: if setup.design {
                        "Captain created. It writes its plan, then a factory draft for review."
                    } else {
                        "Captain created. It is writing its plan."
                    }
                    .into(),
                }
            }
        };
        (Mode::Browse, action)
    }

    fn thread_key(
        &mut self,
        mut thread: ThreadView,
        key: &str,
        kb: &KeybindingsManager,
    ) -> (Mode, CaptainAction) {
        if kb.matches(key, "tui.select.cancel") || key == "q" || key == "left" {
            return (Mode::Browse, CaptainAction::Refresh);
        }
        if kb.matches(key, "tui.select.up") || key == "k" {
            thread.scroll += 1;
            return (Mode::Thread(thread), CaptainAction::None);
        }
        if kb.matches(key, "tui.select.down") || key == "j" {
            thread.scroll = thread.scroll.saturating_sub(1);
            return (Mode::Thread(thread), CaptainAction::None);
        }
        if key == "pageup" {
            thread.scroll += self.viewport_rows / 2;
            return (Mode::Thread(thread), CaptainAction::None);
        }
        if key == "pagedown" {
            thread.scroll = thread.scroll.saturating_sub(self.viewport_rows / 2);
            return (Mode::Thread(thread), CaptainAction::None);
        }
        if key == "r" || key == "m" || kb.matches(key, "tui.select.confirm") {
            if thread.busy {
                self.notice = Some(format!(
                    "{} is working. Reply when the turn ends.",
                    thread.title
                ));
                return (Mode::Thread(thread), CaptainAction::None);
            }
            let member = (thread.member != "lead").then(|| thread.member.clone());
            let action = CaptainAction::Compose {
                purpose: ComposePurpose::Message {
                    project: thread.project.clone(),
                    member,
                },
                label: format!("reply to {}", thread.title),
                initial: String::new(),
            };
            return (Mode::Thread(thread), action);
        }
        (Mode::Thread(thread), CaptainAction::None)
    }

    fn factory_key(
        &mut self,
        mut review: FactoryReview,
        key: &str,
        kb: &KeybindingsManager,
    ) -> (Mode, CaptainAction) {
        self.notice = None;
        self.error = None;
        if kb.matches(key, "tui.select.cancel") || key == "q" || key == "left" {
            return (Mode::Browse, CaptainAction::Refresh);
        }
        if kb.matches(key, "tui.select.up") || key == "k" {
            review.scroll = review.scroll.saturating_sub(1);
            return (Mode::Factory(review), CaptainAction::None);
        }
        if kb.matches(key, "tui.select.down") || key == "j" {
            review.scroll += 1;
            return (Mode::Factory(review), CaptainAction::None);
        }
        if key == "pageup" {
            review.scroll = review.scroll.saturating_sub(self.viewport_rows / 2);
            return (Mode::Factory(review), CaptainAction::None);
        }
        if key == "pagedown" {
            review.scroll += self.viewport_rows / 2;
            return (Mode::Factory(review), CaptainAction::None);
        }
        let action = match key {
            "a" if review.reviewed != review.digest => {
                self.notice = Some("Changed - press r to review again.".into());
                CaptainAction::None
            }
            "a" => match review.state.as_str() {
                "draft" | "changed" => CaptainAction::Act {
                    op: "factory-approve".into(),
                    args: json!({"project": review.project, "name": review.name, "digest": review.reviewed}),
                    done: format!("Approved this version of {}.", review.name),
                },
                "approved" => {
                    self.notice = Some("This version is approved.".into());
                    CaptainAction::None
                }
                "invalid" => {
                    self.notice = Some("Fix the errors before approval.".into());
                    CaptainAction::None
                }
                _ => {
                    self.notice = Some("This factory has no files.".into());
                    CaptainAction::None
                }
            },
            "e" if review.state != "missing" => {
                // The factory folder is <root>/.prime/machines/<name>.
                match std::path::Path::new(&review.path).ancestors().nth(3) {
                    Some(root) => CaptainAction::EditFactory {
                        root: root.to_string_lossy().into_owned(),
                        name: review.name.clone(),
                    },
                    None => CaptainAction::None,
                }
            }
            "m" => CaptainAction::Compose {
                purpose: ComposePurpose::Message {
                    project: review.project.clone(),
                    member: None,
                },
                label: format!("change request for the captain · {}", review.name),
                initial: format!("Change the project factory {}: ", review.name),
            },
            "D" | "shift+d" => {
                if review.live {
                    self.notice = Some(format!(
                        "A run of {} is live. Stop it before you delete the factory.",
                        review.name
                    ));
                    CaptainAction::None
                } else {
                    return (
                        Mode::Confirm(Confirm::DeleteFactory(Box::new(review))),
                        CaptainAction::None,
                    );
                }
            }
            "r" => CaptainAction::LoadFactory {
                project: review.project.clone(),
                name: review.name.clone(),
            },
            _ => CaptainAction::None,
        };
        (Mode::Factory(review), action)
    }

    // Rendering.

    pub(crate) fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let width = width.max(20);
        let rows = self.viewport_rows;
        let mut out: Vec<Line> = Vec::new();
        let dim = |t: &str| theme.fg_span(ThemeColor::Dim, t.to_string());
        let muted = |t: &str| theme.fg_span(ThemeColor::Muted, t.to_string());
        // Header: title, project counts, this checkout.
        let projects = &self.snapshot.projects;
        let mut summary = vec![format!(
            "{} project{}",
            projects.len(),
            if projects.len() == 1 { "" } else { "s" }
        )];
        for state in ["needs you", "running", "paused"] {
            let count = projects
                .iter()
                .filter(|p| p.summary_state() == state)
                .count();
            if count > 0 {
                summary.push(format!("{count} {state}"));
            }
        }
        out.push(truncate_line(
            &vec![
                Span::raw(" "),
                theme.bold(theme.fg_span(ThemeColor::Accent, "captain")),
                Span::raw("  "),
                muted(&summary.join(" · ")),
            ],
            width,
            "…",
        ));
        out.push(vec![
            Span::raw(" "),
            dim(&truncate_to_width(
                &self.snapshot.current_root,
                width.saturating_sub(2),
                "…",
            )),
        ]);
        out.push(rule_row(theme, width));
        let body_rows = rows.saturating_sub(out.len() + 3);
        let body = match &self.mode {
            Mode::Pick(picker) => Self::render_picker(theme, width, picker, body_rows),
            Mode::Confirm(confirm) => Self::render_confirm(theme, width, confirm),
            Mode::Thread(thread) => Self::render_thread(theme, width, thread, body_rows),
            Mode::Factory(review) => Self::render_factory(theme, width, review, body_rows),
            Mode::Browse => self.render_browse(theme, width, body_rows),
        };
        out.extend(body.into_iter().take(body_rows));
        while out.len() < rows.saturating_sub(3) {
            out.push(Vec::new());
        }
        // Status line, rule, key hints.
        let status = self
            .error
            .as_ref()
            .or(self.snapshot_error.as_ref())
            .map(|e| theme.fg_span(ThemeColor::Error, clean(e)))
            .or_else(|| {
                self.notice
                    .as_ref()
                    .map(|n| theme.fg_span(ThemeColor::Text, clean(n)))
            })
            .or_else(|| (!self.loaded).then(|| dim("Loading…")));
        out.push(truncate_line(
            &vec![Span::raw(" "), status.unwrap_or_else(|| Span::raw(""))],
            width,
            "…",
        ));
        out.push(rule_row(theme, width));
        out.push(truncate_line(
            &vec![Span::raw(" "), dim(&self.footer(width, kb))],
            width,
            "…",
        ));
        out
    }

    fn footer(&self, width: usize, kb: &KeybindingsManager) -> String {
        let f6 = crate::keybindings::format_key_text(&kb.get_keys("app.factory.toggle").join("/"));
        match &self.mode {
            Mode::Pick(picker) if picker.multi => {
                "↑↓ select · Space mark · Enter next · Esc back".into()
            }
            Mode::Pick(_) => "↑↓ select · Enter choose · Esc back".into(),
            Mode::Confirm(_) => "y confirm · n cancel".into(),
            Mode::Thread(_) => "↑↓ scroll · r reply · Esc back".into(),
            Mode::Factory(_) => "↑↓ scroll · a approve · e edit · m change · D delete · Esc back".into(),
            Mode::Browse if width < 90 => {
                format!("↑↓ · Tab · Enter open · s stop · D delete · ? help · {f6} close")
            }
            Mode::Browse if width < 114 => format!(
                "↑↓ · Tab crew · Enter open · m message · s stop · R resume · D delete · ? help · {f6} close"
            ),
            Mode::Browse => format!(
                "↑↓ select · Tab crew · Enter open · m message · d assign · n new · s stop · R resume · D delete · ? help · {f6} close"
            ),
        }
    }

    fn render_browse(&self, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        let rail = if width >= 72 {
            (width / 3).clamp(22, 36)
        } else {
            (width / 3).max(18)
        };
        let left = self.render_rail(theme, rail, rows);
        let right_width = width.saturating_sub(rail + 3);
        let right = if self.help {
            Self::render_help(theme, right_width)
        } else {
            self.render_details(theme, right_width, rows)
        };
        let border = theme.fg_span(ThemeColor::BorderMuted, "│");
        (0..rows)
            .map(|row| {
                let mut line = truncate_line(left.get(row).unwrap_or(&Vec::new()), rail, "…");
                let pad = rail.saturating_sub(line_width(&line));
                line.push(Span::raw(" ".repeat(pad)));
                line.push(border.clone());
                line.push(Span::raw(" "));
                line.extend(truncate_line(
                    right.get(row).unwrap_or(&Vec::new()),
                    right_width,
                    "…",
                ));
                line
            })
            .collect()
    }

    fn render_rail(&self, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        let mut out: Vec<Line> = vec![vec![
            Span::raw("   "),
            theme.fg_span(ThemeColor::Dim, "projects"),
        ]];
        let projects = &self.snapshot.projects;
        if projects.is_empty() {
            for text in [
                "",
                "No projects yet",
                "n  set up a Captain",
                "p  promote this chat",
                "f  open factory",
            ] {
                out.push(vec![Span::raw("   "), theme.fg_span(ThemeColor::Dim, text)]);
            }
            return out;
        }
        let slots = (rows.saturating_sub(1) / 2).max(1);
        let top = self.project.saturating_sub(slots - 1);
        for (index, project) in projects.iter().enumerate().skip(top).take(slots) {
            let selected = index == self.project;
            let state = project.summary_state();
            let (needs, running, done) = project.counts();
            let counts = [(needs, "!"), (running, "●"), (done, "✓")]
                .iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, g)| format!("{g}{n}"))
                .collect::<Vec<_>>()
                .join(" ");
            let marker = if selected && self.focus == Focus::Projects {
                "›"
            } else if selected {
                "·"
            } else {
                " "
            };
            let name_width = width.saturating_sub(6 + str_width(&counts) + 1);
            let name = truncate_to_width(&clean(&project.name), name_width, "…");
            let mut first = vec![
                Span::raw(" "),
                theme.bold(theme.fg_span(ThemeColor::Accent, marker)),
                Span::raw(" "),
                theme.fg_span(summary_color(state), "■"),
                Span::raw(" "),
                if selected {
                    theme.bold(theme.fg_span(ThemeColor::Text, name.clone()))
                } else {
                    theme.fg_span(ThemeColor::Text, name.clone())
                },
            ];
            let used = 5 + str_width(&name);
            let gap = width.saturating_sub(used + str_width(&counts) + 1);
            first.push(Span::raw(" ".repeat(gap)));
            first.push(theme.fg_span(ThemeColor::Dim, counts));
            out.push(first);
            let here = if project.root == self.snapshot.current_root {
                "  · here"
            } else {
                ""
            };
            out.push(vec![
                Span::raw("     "),
                theme.fg_span(summary_color(state), format!("{state}{here}")),
            ]);
        }
        out
    }

    fn render_details(&self, theme: &Theme, width: usize, rows: usize) -> Vec<Line> {
        let Some(project) = self.selected_project() else {
            return vec![vec![theme.fg_span(ThemeColor::Dim, "No project selected")]];
        };
        let dim = |t: String| theme.fg_span(ThemeColor::Dim, t);
        let mut out: Vec<Line> = Vec::new();
        out.push(vec![
            theme.bold(theme.fg_span(ThemeColor::Text, clean(&project.name))),
            Span::raw("  "),
            theme.fg_span(ThemeColor::MdLink, project.model_label()),
        ]);
        out.push(vec![dim(truncate_to_width(&project.root, width, "…"))]);
        // The Captain's own line: lead state, or the project's paused state.
        let mut lead: Line = Vec::new();
        if project.is_active() {
            let (glyph, color) = status_glyph(if project.lead_state.is_empty() {
                "ready"
            } else {
                &project.lead_state
            });
            lead.push(theme.fg_span(
                color,
                format!("captain  {glyph} {}", project.lead_state.replace('_', " ")),
            ));
        } else {
            lead.push(theme.bold(theme.fg_span(
                ThemeColor::Error,
                format!("captain  ■ {}", project.display_state()),
            )));
            if project.display_state() == "paused" {
                lead.push(dim("  R resume".into()));
            }
        }
        if let Some(edit) = project.edits.first() {
            lead.push(dim(format!(
                "   {} updated by {}",
                edit.field.replace('_', " "),
                edit.actor
            )));
        }
        out.push(lead);
        out.push(vec![dim(clean(
            project.objective.lines().next().unwrap_or(""),
        ))]);
        out.push(Vec::new());
        let digest = project.digest.trim();
        if !digest.is_empty() {
            out.push(vec![dim("digest".into())]);
            let lines = wrap(digest, width.saturating_sub(1));
            let room = if self.expanded { 2 } else { (rows / 3).max(2) };
            for line in lines.iter().take(room) {
                let heading = matches!(
                    line.trim().trim_end_matches(':').to_lowercase().as_str(),
                    "needs you" | "new" | "in progress"
                );
                let span = theme.fg_span(ThemeColor::Text, line.clone());
                out.push(vec![
                    Span::raw(" "),
                    if heading { theme.bold(span) } else { span },
                ]);
            }
            if lines.len() > room {
                out.push(vec![dim(format!(" … {} more lines", lines.len() - room))]);
            }
            out.push(Vec::new());
        } else if let Some(detail) = project.lead_detail.as_deref().filter(|d| !d.is_empty()) {
            out.push(vec![dim(format!("report {}", clean(detail)))]);
            out.push(Vec::new());
        }
        let sections = [
            ("rules", &project.rules),
            ("plan", &project.plan),
            ("crew rules", &project.crew_rules),
        ];
        if self.expanded {
            for (label, text) in sections {
                out.push(vec![dim(label.into())]);
                let body = if text.trim().is_empty() {
                    "empty"
                } else {
                    text.trim()
                };
                for line in wrap(body, width.saturating_sub(1))
                    .into_iter()
                    .take((rows / 6).max(2))
                {
                    out.push(vec![Span::raw(" "), theme.fg_span(ThemeColor::Text, line)]);
                }
            }
            out.push(vec![dim("x  collapse".into())]);
        } else {
            let counts = sections
                .iter()
                .map(|(label, text)| {
                    let n = if text.trim().is_empty() {
                        0
                    } else {
                        text.trim().lines().count()
                    };
                    format!("{label} {n}")
                })
                .collect::<Vec<_>>()
                .join(" · ");
            out.push(vec![dim(format!("x  {counts}"))]);
        }
        out.push(Vec::new());
        if !project.factories.is_empty() || project.factories_error.is_some() {
            let waiting = project
                .factories
                .iter()
                .filter(|f| f.needs_review())
                .count();
            let mut heading = vec![dim(format!("factories  {}", project.factories.len()))];
            if waiting > 0 {
                heading.push(theme.bold(theme.fg_span(
                    ThemeColor::Warning,
                    format!(
                        "  needs you: {waiting} factory draft{} need{} review",
                        if waiting == 1 { "" } else { "s" },
                        if waiting == 1 { "s" } else { "" }
                    ),
                )));
            }
            heading.push(dim("  F review".into()));
            out.push(heading);
            for factory in &project.factories {
                let (glyph, color) = factory_glyph(&factory.state);
                out.push(vec![
                    Span::raw("· "),
                    theme.fg_span(color, glyph),
                    Span::raw(" "),
                    theme.fg_span(ThemeColor::Text, clean(&factory.name)),
                    theme.fg_span(
                        ThemeColor::Dim,
                        format!(
                            "  {}",
                            factory_state_label(&factory.state, factory.approved)
                        ),
                    ),
                ]);
            }
            if let Some(error) = &project.factories_error {
                out.push(vec![theme.fg_span(ThemeColor::Error, clean(error))]);
            }
            out.push(Vec::new());
        }
        // The crew, grouped like the digest: two rows per member, the state on the second.
        let selected = self.selected_member().map(|m| m.id.clone());
        let mut crew_rows: Vec<(Line, bool)> = Vec::new();
        if project.crew.is_empty() {
            crew_rows.push((
                vec![dim("No crew yet · d assign work · a add a thread".into())],
                false,
            ));
        }
        for (heading, members) in project.ordered_crew() {
            if members.is_empty() {
                continue;
            }
            crew_rows.push((vec![dim(format!("{heading}  {}", members.len()))], false));
            for member in members {
                let is_selected = selected.as_deref() == Some(member.id.as_str());
                let [first, second] = Self::member_rows(theme, member, is_selected, width);
                crew_rows.push((first, is_selected));
                crew_rows.push((second, false));
            }
        }
        // A short pane with the crew in focus gives the crew the room: the project lines
        // (name, root, Captain state) stay, the digest and rules give way.
        let wanted = crew_rows.len().min(rows.saturating_sub(4));
        if self.focus == Focus::Crew && out.len() + wanted > rows {
            // Cut at whole sections: keep only the project lines, then one blank row.
            out.truncate(4);
            out.push(Vec::new());
        }
        let room = rows.saturating_sub(out.len());
        // Keep both rows of the selected member visible.
        let focus = crew_rows.iter().position(|(_, s)| *s).map_or(0, |i| i + 1);
        let start = if focus >= room { focus + 1 - room } else { 0 };
        out.extend(crew_rows.into_iter().skip(start).map(|(line, _)| line));
        out
    }

    /// One crew member as two rows: the state icon and name with its kind, then the state text
    /// and the latest detail. The selected member's rows carry the soft selection band.
    fn member_rows(
        theme: &Theme,
        member: &CaptainMember,
        selected: bool,
        width: usize,
    ) -> [Line; 2] {
        let (glyph, color) = status_glyph(&member.state);
        let label = member.label();
        let kind = if member.is_run() {
            "factory".to_owned()
        } else if member.session.is_some() {
            "chat".into()
        } else {
            member.provider.clone()
        };
        let mut first = menu_row(
            theme,
            width,
            vec![
                theme.fg_span(color, glyph),
                Span::raw(" "),
                theme.fg_span(ThemeColor::Text, label),
            ],
            &[MenuSegment::muted(&kind)],
            selected,
        );
        if selected {
            // The selection marker uses the accent, as in the project rail.
            if let Some(marker) = first.first_mut() {
                marker.style = marker.style.patch(theme.fg_style(ThemeColor::Accent));
            }
        }
        let mut facts: Vec<String> = Vec::new();
        if member.is_run() {
            facts.extend(
                [&member.progress, &member.step]
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            if let Some((_, task)) = member.title.split_once(" · ") {
                facts.push(clean(task));
            }
        }
        let detail = clean(&member.detail);
        if !detail.is_empty() {
            facts.push(detail);
        }
        let mut second = vec![Span::raw("    "), theme.fg_span(color, member.state_text())];
        if !facts.is_empty() {
            second.push(theme.fg_span(ThemeColor::Dim, format!(" · {}", facts.join(" · "))));
        }
        let second = crate::menu_panel::fill_row(
            &truncate_line(&second, width, "…"),
            selected,
            width,
            theme.soft_selection_style(),
        );
        [first, second]
    }

    fn render_help(theme: &Theme, width: usize) -> Vec<Line> {
        let rows = [
            "Tab focus crew · ↑↓ move · ←→ project",
            "Enter open the chat · m message · v review",
            "m on a paused run answers it",
            "n new · p promote this chat · e edit",
            "a add a thread · d assign work · u release",
            "f factory · c consensus · x rules and plan",
            "F project factories: review, approve, design",
            "s stop · R resume · D delete",
            "In a Captain chat: F7 switch chat · F8/Shift+F8 next/previous · Shift+F7 captain",
            "r refresh · ? close help · Esc close",
        ];
        let mut out = vec![
            Vec::new(),
            vec![theme.bold(theme.fg_span(ThemeColor::Text, "captain controls"))],
        ];
        out.extend(
            rows.iter().map(|row| {
                vec![theme.fg_span(ThemeColor::Muted, truncate_to_width(row, width, "…"))]
            }),
        );
        out
    }

    fn render_picker(theme: &Theme, width: usize, picker: &Picker, rows: usize) -> Vec<Line> {
        let mut out: Vec<Line> = Vec::new();
        if let PickPurpose::Setup(setup) = &picker.purpose {
            let mut steps: Line = vec![Span::raw(" ")];
            for (index, name) in SETUP_STEPS.iter().enumerate() {
                let color = if index == setup.step {
                    ThemeColor::Accent
                } else {
                    ThemeColor::Dim
                };
                steps.push(theme.fg_span(color, *name));
                steps.push(Span::raw("   "));
            }
            out.push(truncate_line(&steps, width, ""));
        }
        out.push(vec![
            Span::raw(" "),
            theme.bold(theme.fg_span(ThemeColor::Text, picker.title.clone())),
        ]);
        out.extend(crate::menu_panel::search_field_lines(
            theme,
            width,
            &picker.filter,
            picker.filter.chars().count(),
            true,
            "Type to filter",
        ));
        let visible = picker.visible();
        if picker.items.is_empty() {
            out.push(vec![
                Span::raw("   "),
                theme.fg_span(ThemeColor::Dim, "Loading…"),
            ]);
        } else if visible.is_empty() {
            out.push(crate::menu_panel::no_match_row(theme, width, "No match"));
        }
        let room = rows.saturating_sub(out.len()).max(1);
        let top = picker.selected.saturating_sub(room - 1);
        for (offset, &index) in visible.iter().enumerate().skip(top).take(room) {
            let selected = offset == picker.selected;
            let mark = if picker.multi {
                if picker.marked.contains(&index) {
                    "■ "
                } else {
                    "□ "
                }
            } else {
                ""
            };
            let label = format!("{mark}{}", picker.items[index].label);
            out.push(menu_row(
                theme,
                width,
                vec![theme.fg_span(ThemeColor::Text, label)],
                &[],
                selected,
            ));
        }
        out
    }

    fn render_confirm(theme: &Theme, width: usize, confirm: &Confirm) -> Vec<Line> {
        let (title, body): (String, Vec<String>) = match confirm {
            Confirm::Stop { name, .. } => (
                format!("stop the Captain of {name}?"),
                vec![
                    "Queued work is cancelled.".into(),
                    "Running crew turns and Captain-started factory runs stop.".into(),
                    "No new turns start, including digest wakeups, until you resume.".into(),
                    "Threads, transcripts, and worktrees stay.".into(),
                ],
            ),
            Confirm::Delete {
                name,
                crew,
                running,
                ..
            } => (
                format!("delete the Captain of {name}?"),
                vec![
                    if *running > 0 {
                        format!("{running} running turn(s) or run(s) stop first.")
                    } else {
                        "Nothing is running.".into()
                    },
                    "The Captain's project record, plan, rules, digest, and reports are removed."
                        .into(),
                    format!(
                        "The Captain thread and {crew} crew thread(s) return to the thread list."
                    ),
                    "Transcripts and worktrees stay.".into(),
                ],
            ),
            Confirm::DeleteFactory(review) => (
                format!("delete the project factory {}?", review.name),
                vec![
                    format!("Its folder {} is removed.", review.path),
                    "Its approval is removed. Runs that ended keep their records.".into(),
                ],
            ),
            Confirm::Release { title, .. } => (
                format!("release {title} from the crew?"),
                vec!["The thread returns to the thread list. Its transcript stays.".into()],
            ),
            Confirm::Create(setup) => (
                if setup.reference.is_some() {
                    "promote this chat to Captain?".into()
                } else {
                    "create this Captain?".into()
                },
                vec![
                    format!("brief: {}", clean(&setup.brief)),
                    format!(
                        "model: {} · {}{}",
                        setup.provider,
                        if setup.model.is_empty() {
                            "default model"
                        } else {
                            &setup.model
                        },
                        if setup.thinking.is_empty() {
                            String::new()
                        } else {
                            format!(" · {}", setup.thinking)
                        }
                    ),
                    format!("factory: {}", setup.factory_summary()),
                    format!("workers: {}", setup.parallel),
                    format!("threads: {}", setup.crew.len()),
                ],
            ),
        };
        let mut out = vec![
            Vec::new(),
            vec![
                Span::raw(" "),
                theme.bold(theme.fg_span(
                    if matches!(confirm, Confirm::Delete { .. } | Confirm::DeleteFactory(_)) {
                        ThemeColor::Error
                    } else {
                        ThemeColor::Text
                    },
                    title,
                )),
            ],
            Vec::new(),
        ];
        for line in body {
            for row in wrap(&line, width.saturating_sub(3)) {
                out.push(vec![
                    Span::raw("   "),
                    theme.fg_span(ThemeColor::Muted, row),
                ]);
            }
        }
        out.push(Vec::new());
        out.push(vec![
            Span::raw(" "),
            theme.fg_span(ThemeColor::Text, "y confirm · n cancel"),
        ]);
        out
    }

    fn render_factory(
        theme: &Theme,
        width: usize,
        review: &FactoryReview,
        rows: usize,
    ) -> Vec<Line> {
        let (glyph, color) = factory_glyph(&review.state);
        let mut head: Line = vec![
            Span::raw(" "),
            theme.fg_span(ThemeColor::Dim, "captain › factory "),
            theme.bold(theme.fg_span(ThemeColor::Text, clean(&review.name))),
            Span::raw("  "),
            theme.fg_span(
                color,
                format!(
                    "{glyph} {}",
                    factory_state_label(&review.state, review.approved)
                ),
            ),
        ];
        if review.live {
            head.push(theme.fg_span(ThemeColor::Accent, "  ● run live"));
        }
        if review.reviewed != review.digest {
            head.push(
                theme.bold(
                    theme.fg_span(ThemeColor::Warning, "  Changed - press r to review again"),
                ),
            );
        }
        let mut out = vec![head];
        let digest = review.digest.get(..12).unwrap_or(&review.digest);
        out.push(vec![
            Span::raw(" "),
            theme.fg_span(
                ThemeColor::Dim,
                truncate_to_width(
                    &clean(&if digest.is_empty() {
                        review.path.clone()
                    } else {
                        format!("{} · digest {digest}", review.path)
                    }),
                    width.saturating_sub(2),
                    "…",
                ),
            ),
        ]);
        if matches!(review.state.as_str(), "draft" | "changed") {
            out.push(vec![
                Span::raw(" "),
                theme.fg_span(
                    ThemeColor::Muted,
                    "Approval covers every file of this version. Python nodes run code.",
                ),
            ]);
        }
        let mut body: Vec<Line> = Vec::new();
        for error in &review.errors {
            for row in wrap(&clean(error), width.saturating_sub(3)) {
                body.push(vec![
                    Span::raw("   "),
                    theme.fg_span(ThemeColor::Error, row),
                ]);
            }
        }
        if let Some(error) = &review.graph_error {
            for row in wrap(
                &format!("No diagram: {}", clean(error)),
                width.saturating_sub(3),
            ) {
                body.push(vec![Span::raw("   "), theme.fg_span(ThemeColor::Dim, row)]);
            }
        }
        if let Some(lines) = review
            .graph
            .as_ref()
            .and_then(|spec| crate::factory_view::machine_graph_lines(theme, spec))
        {
            body.push(Vec::new());
            for line in lines {
                let mut row = vec![Span::raw(" ")];
                row.extend(line);
                body.push(truncate_line(&row, width, "…"));
            }
        }
        out.push(Vec::new());
        let room = rows.saturating_sub(out.len()).max(1);
        let start = review.scroll.min(body.len().saturating_sub(room));
        out.extend(body.into_iter().skip(start).take(room));
        out
    }

    fn render_thread(theme: &Theme, width: usize, thread: &ThreadView, rows: usize) -> Vec<Line> {
        let mut head: Line = vec![
            Span::raw(" "),
            theme.fg_span(ThemeColor::Dim, "captain › "),
            theme.bold(theme.fg_span(ThemeColor::Text, thread.title.clone())),
            theme.fg_span(ThemeColor::Dim, format!("  {}", thread.provider)),
        ];
        if thread.busy {
            head.push(theme.fg_span(ThemeColor::Accent, "  ● working"));
        }
        let mut body: Vec<Line> = Vec::new();
        let inner = width.saturating_sub(4);
        for (role, text) in &thread.messages {
            body.push(Vec::new());
            if role == "user" {
                for line in wrap(text, inner) {
                    body.push(vec![
                        Span::raw("  "),
                        theme.fg_span(ThemeColor::UserMessageText, line),
                    ]);
                }
            } else {
                let style = crate::markdown::MarkdownStyle::from_theme(theme);
                for line in crate::markdown::render_markdown(text, inner, &style) {
                    let mut row = vec![Span::raw("  ")];
                    row.extend(line);
                    body.push(row);
                }
            }
        }
        if thread.messages.is_empty() {
            body.push(vec![
                Span::raw("  "),
                theme.fg_span(ThemeColor::Dim, "No messages yet."),
            ]);
        }
        let room = rows.saturating_sub(1).max(1);
        let end = body
            .len()
            .saturating_sub(thread.scroll.min(body.len().saturating_sub(room)));
        let start = end.saturating_sub(room);
        let mut out = vec![head];
        out.extend(body[start..end].iter().cloned());
        out
    }
}

/// The chat switcher of a Captain project (F7, `/crew`): the Captain first, then its crew.
pub(crate) struct CrewPicker {
    project: String,
    current: String,
    chats: Vec<crate::session_ui::CrewChat>,
    selected: usize,
}

/// The crew picker's answer to a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CrewPick {
    None,
    Close,
    Open(crate::session_ui::CrewChat),
}

impl CrewPicker {
    pub(crate) fn new(
        project: String,
        current: String,
        chats: Vec<crate::session_ui::CrewChat>,
    ) -> Self {
        let selected = chats.iter().position(|c| c.label == current).unwrap_or(0);
        Self {
            project,
            current,
            chats,
            selected,
        }
    }

    pub(crate) fn set_chats(&mut self, chats: Vec<crate::session_ui::CrewChat>) {
        let label = self.chats.get(self.selected).map(|c| c.label.clone());
        self.chats = chats;
        self.selected = label
            .and_then(|l| self.chats.iter().position(|c| c.label == l))
            .unwrap_or(0)
            .min(self.chats.len().saturating_sub(1));
    }

    pub(crate) fn handle_key(&mut self, key: &str, kb: &KeybindingsManager) -> CrewPick {
        if kb.matches(key, "tui.select.cancel")
            || kb.matches(key, "app.captain.crew")
            || key == "ctrl+c"
        {
            return CrewPick::Close;
        }
        if kb.matches(key, "tui.select.up") {
            self.selected = self.selected.saturating_sub(1);
        } else if kb.matches(key, "tui.select.down") {
            self.selected = (self.selected + 1).min(self.chats.len().saturating_sub(1));
        } else if kb.matches(key, "tui.select.confirm") {
            if let Some(chat) = self.chats.get(self.selected) {
                if chat.label == self.current {
                    return CrewPick::Close;
                }
                return CrewPick::Open(chat.clone());
            }
        }
        CrewPick::None
    }

    pub(crate) fn render(&self, theme: &Theme, width: usize, kb: &KeybindingsManager) -> Vec<Line> {
        let mut out: Vec<Line> = vec![
            rule_row(theme, width),
            vec![
                Span::raw(" "),
                theme.bold(theme.fg_span(ThemeColor::Text, "switch chat")),
                theme.fg_span(ThemeColor::Dim, format!("  {}", clean(&self.project))),
            ],
        ];
        for (index, chat) in self.chats.iter().enumerate() {
            let (glyph, color) = if chat.busy {
                ("●", ThemeColor::Accent)
            } else {
                ("·", ThemeColor::Dim)
            };
            let mut trailing = Vec::new();
            if chat.label == self.current {
                trailing.push(MenuSegment::muted("open"));
            }
            if chat.session.is_none() && chat.member.is_some() {
                trailing.push(MenuSegment::muted("provider thread"));
            }
            trailing.push(if chat.busy {
                MenuSegment::themed(ThemeColor::Accent, "working")
            } else {
                MenuSegment::muted("idle")
            });
            out.push(menu_row(
                theme,
                width,
                vec![
                    theme.fg_span(color, glyph),
                    Span::raw(" "),
                    theme.fg_span(ThemeColor::Text, clean(&chat.label)),
                ],
                &trailing,
                index == self.selected,
            ));
        }
        let cancel =
            crate::keybindings::format_key_text(&kb.get_keys("tui.select.cancel").join("/"));
        out.push(crate::menu_panel::hint_row(
            theme,
            width,
            &format!("↑↓ select · Enter open · {cancel} close"),
        ));
        out
    }
}
