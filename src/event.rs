#[derive(Debug, Clone)]
pub struct TreeNodeView {
    pub id: String,
    pub parent_id: Option<String>,
    pub label: String,
    pub active: bool,
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
        state: String,
    },
    PendingMessages(Vec<String>),
    UserMessage(String),
    ThinkingStart,
    AssistantStart,
    AssistantDelta(String),
    ToolStart {
        name: String,
    },
    ToolOutput {
        name: String,
        output: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    TreeView(Vec<TreeNodeView>),
    Transcript(Vec<TranscriptItem>),
    Info(String),
    Error(String),
    Status(String),
}
