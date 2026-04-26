use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureFrame {
    pub ts: DateTime<Utc>,
    pub display_id: String,
    pub dirty: Rect,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub encoding: FrameEncoding,
    pub bytes: Vec<u8>,
    pub monotonic_ns: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrameEncoding {
    RawBgra,
    Jpeg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A11yNode {
    pub id: String,
    pub parent: Option<String>,
    pub role: String,
    pub name: String,
    pub description: Option<String>,
    pub bounds: Option<Rect>,
    pub state: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum A11yEventKind {
    Snapshot,
    ChildrenChanged,
    StateChanged,
    TextChanged,
    Focus,
    WindowActivate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A11yEvent {
    pub ts: DateTime<Utc>,
    pub display_id: String,
    pub kind: A11yEventKind,
    pub nodes: Vec<A11yNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActionKind {
    MovePointer {
        x: i32,
        y: i32,
        absolute: bool,
    },
    Click {
        button: MouseButton,
        count: u8,
        x: Option<i32>,
        y: Option<i32>,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    TypeText {
        text: String,
    },
    KeyCombo {
        keys: Vec<String>,
    },
    Drag {
        from: (i32, i32),
        to: (i32, i32),
    },
    Finish {
        success: bool,
        summary: String,
    },
    Noop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExpectedChange {
    FocusOnNode { node_name: String },
    TextPresent { text: String },
    WindowActivated { title: String },
    PixelChanged { region: Rect },
    AnyUiChange,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRequest {
    pub task_id: String,
    pub step_id: String,
    pub display_id: String,
    pub goal: String,
    pub rationale: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grounding_target: Option<String>,
    pub kind: ActionKind,
    pub expected: Vec<ExpectedChange>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub task_id: String,
    pub step_id: String,
    pub accepted: bool,
    pub injected_at_ns: Option<u128>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VerificationStatus {
    Verified,
    Timeout,
    UnexpectedState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationResult {
    pub task_id: String,
    pub step_id: String,
    pub status: VerificationStatus,
    pub detail: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningTurn {
    pub task_id: String,
    pub goal: String,
    pub display_id: String,
    pub frame: Option<CaptureFrame>,
    pub a11y_snapshot: Vec<A11yNode>,
    pub action_history: Vec<ActionHistoryEntry>,
    /// Title of the currently focused top-level window, captured by the
    /// supervisor right before the turn was assembled. Authoritative
    /// ground truth for the model when its own action_history rationales
    /// disagree with what it sees on screen.
    #[serde(default)]
    pub active_window_title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionHistoryEntry {
    pub action: ActionRequest,
    pub input_result: Option<ActionResult>,
    pub verification_result: Option<VerificationResult>,
    /// True when this action was followed by an active-window-title change.
    /// Lets the model distinguish clicks that actually navigated/opened a
    /// new surface from clicks that fired but produced no real progress.
    #[serde(default)]
    pub caused_navigation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelActionEnvelope {
    pub action: ActionRequest,
    pub reasoning_summary: String,
    pub raw_response: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TaskStatus {
    Succeeded,
    Failed,
    RetryBudgetExhausted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepReport {
    pub step_id: String,
    pub action: ActionKind,
    pub input_accepted: Option<bool>,
    pub verification_status: Option<VerificationStatus>,
    pub model_ms: Option<u128>,
    pub input_ms: Option<u128>,
    pub verification_ms: Option<u128>,
    pub duration_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskReport {
    pub task_id: String,
    pub goal: String,
    pub status: TaskStatus,
    pub summary: String,
    pub duration_ms: u128,
    pub steps: Vec<StepReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BusTopic {
    Capture,
    A11y,
    Action,
    Input,
    Verification,
    Reasoning,
    Health,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BusMessage {
    Subscribe {
        topics: Vec<BusTopic>,
    },
    Capture(CaptureFrame),
    A11y(A11yEvent),
    ActionRequest(ActionRequest),
    ActionResult(ActionResult),
    VerificationArmed {
        task_id: String,
        step_id: String,
    },
    VerificationResult(VerificationResult),
    ReasoningTurn(ReasoningTurn),
    ModelAction(ModelActionEnvelope),
    Health {
        service: String,
        ok: bool,
        detail: String,
    },
}

impl BusMessage {
    pub fn topic(&self) -> Option<BusTopic> {
        match self {
            BusMessage::Subscribe { .. } => None,
            BusMessage::Capture(_) => Some(BusTopic::Capture),
            BusMessage::A11y(_) => Some(BusTopic::A11y),
            BusMessage::ActionRequest(_) => Some(BusTopic::Action),
            BusMessage::ActionResult(_) => Some(BusTopic::Input),
            BusMessage::VerificationArmed { .. } => Some(BusTopic::Verification),
            BusMessage::VerificationResult(_) => Some(BusTopic::Verification),
            BusMessage::ReasoningTurn(_) | BusMessage::ModelAction(_) => Some(BusTopic::Reasoning),
            BusMessage::Health { .. } => Some(BusTopic::Health),
        }
    }
}
