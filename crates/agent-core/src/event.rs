#[derive(Debug, Clone)]
pub struct TreeNodeView {
    pub id: String,
    pub parent_id: Option<String>,
    pub label: String,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct SessionListItemView {
    pub id: String,
    pub label: String,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct ModelOptionView {
    pub model: String,
    pub active: bool,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub reasoning_efforts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CommandView {
    pub command: String,
    pub marker: Option<String>,
}

#[derive(Debug, Clone)]
pub enum TranscriptItem {
    User(String),
    Assistant(String),
    Tool { name: String, output: String },
    Branch(String),
}

#[derive(Debug)]
pub enum AgentEvent {
    Startup {
        version: String,
        profile_dir: String,
        config_path: String,
    },
    ModelStatus {
        provider: String,
        model: String,
        reasoning_effort: String,
        context_window: u64,
        max_output_tokens: u64,
        reasoning_efforts: Vec<String>,
        state: String,
    },
    PendingMessages(Vec<String>),
    UserMessage(String),
    ThinkingStart,
    AssistantStart,
    AssistantDelta(String),
    Retrying {
        attempt: usize,
    },
    ToolStart {
        call_id: String,
        name: String,
    },
    ToolUpdate {
        call_id: String,
        name: String,
        output: String,
    },
    ToolOutput {
        call_id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    TreeView(Vec<TreeNodeView>),
    ResumeView(Vec<SessionListItemView>),
    ModelView {
        models: Vec<ModelOptionView>,
        active_effort: String,
    },
    CommandList(Vec<CommandView>),
    Transcript(Vec<TranscriptItem>),
    Info(String),
    Error(String),
    Status(String),
}
