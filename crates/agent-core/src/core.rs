use crate::providers::CLIENT_NAME;
use crate::{
    actions::{action_digest, decision_message, DeferredAction},
    config::{
        models_for_provider, profile_dir, ApprovalMode, AuthStore, Config, JucodeTokens,
        LiveApprovalMode, ModelConfig,
    },
    event::{
        AgentEvent, CommandView, GoalView, LoginProviderView, ModelOptionView, PlanItem,
        SessionListItemView,
    },
    hooks::Hooks,
    llm::{
        ApprovalDecision, ApprovalRequest, GoalToolRequest, OpenAiClient, OpenAiClientConfig,
        StreamEvent, ToolGoalResponse,
    },
    mcp::McpManager,
    oauth::{self, OAuthLoginResult, OAuthModel},
    prompt::{
        build_system_prompt, discover_project_instructions, discover_skills, skill_commands,
        skill_message, skill_pin_message, PromptContext,
    },
    session::{
        compaction_summary_item, ContextStatistics, EntryKind, SessionLock, SessionStore,
        SessionSummary, ThreadGoal, ThreadGoalStatus,
    },
    skills,
    subagents::SubagentManager,
    trust::{self, TrustStore},
    update::{self, UpdateNotice},
};
use llm_provider_kit::auth::{self as provider_auth, LoginContext, LoginOutcome};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, io,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Recent context (in tokenizer-counted tokens) kept verbatim when compacting; older
/// turns are folded into the summary.
const COMPACTION_KEEP_RECENT_TOKENS: usize = 20_000;
const RESUME_SUMMARY_IDLE_SECONDS: u64 = 5 * 60;
const RESUME_SUMMARY_MODEL: &str = "gpt-5.4-mini";

/// A gated tool call parked until the client answers `/approve` (or the serve
/// `approve` op). `hunk_ids` are the selectable hunks of an edit call; empty
/// means only whole-call decisions are valid.
struct PendingApproval {
    response_tx: mpsc::Sender<ApprovalDecision>,
    name: String,
    hunk_ids: Vec<String>,
    summary: String,
    arguments: String,
    cwd: PathBuf,
    subagent_id: Option<String>,
}

/// Outcome of a deferred action approved and run on a background thread.
struct ActionOutcome {
    action: DeferredAction,
    output: String,
    is_error: bool,
}

#[derive(Debug)]
enum WorkerEvent {
    /// A steered user message reached the model mid-turn.
    Steered(String),
    /// A fragment of the propose_plan call's arguments as the model writes it.
    PlanDraft {
        call_id: String,
        delta: String,
    },
    CompactionStart,
    CompactionProgress {
        output_tokens: u64,
    },
    CompactionDone {
        summary: String,
        replaced_through: u64,
    },
    CompactionFailed(String),
    ResumeSummaryDone {
        summary: String,
        status: ThreadGoalStatus,
        summarized_at: u64,
    },
    ResumeSummaryFailed(String),
    CallStart,
    Connected,
    ReasoningDelta(String),
    Delta(String),
    Retrying {
        attempt: usize,
        max_attempts: usize,
        reason: String,
        delay_ms: u64,
    },
    ResponseItem(Value),
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
        model_output: String,
        is_error: bool,
    },
    Usage {
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
    },
    Done,
    Error(String),
}

pub struct AgentCore {
    config: Config,
    auth: AuthStore,
    /// Tag each turn's JuCode gateway requests (`X-JuCode-Turn`) so the
    /// gateway can put their cost on the turn the daemon records.
    tag_turns: bool,
    /// The current turn's tag, new for every turn.
    turn_tag: Option<String>,
    session: SessionStore,
    /// Held for the lifetime of the active session so a second process cannot
    /// resume it and interleave journal appends. Released on session switch.
    session_lock: Option<SessionLock>,
    profile_dir: PathBuf,
    cwd: PathBuf,
    queued: VecDeque<(String, Vec<String>)>,
    /// Image paths staged with `/image <path>`; attached to (and drained by)
    /// the next submitted user message.
    pending_images: Vec<String>,
    running: bool,
    receiver: Option<Receiver<WorkerEvent>>,
    /// Dedicated channel for the idle resume-summary worker; kept separate from
    /// `receiver` so a new turn / session switch cannot orphan the worker and
    /// leave `resume_summary_running` stuck.
    resume_summary_receiver: Option<Receiver<WorkerEvent>>,
    goal_tool_receiver: Option<Receiver<GoalToolRequest>>,
    approval_receiver: Option<Receiver<ApprovalRequest>>,
    pending_approvals: HashMap<String, PendingApproval>,
    /// Per-session "always allow" tool names, shared by the main agent and all
    /// subagents (their requests arrive on the same channel). It can only
    /// loosen the approval mode, never tighten it.
    approved_tools: HashSet<String>,
    /// Session approval mode; starts from config and is switched by /permissions
    /// or the serve `set_approval_mode` op (session-only, not persisted). The
    /// running turn and its subagents share it, so a switch applies at once.
    approval_mode: LiveApprovalMode,
    /// False when no client is watching: gated calls become deferred actions
    /// instead of blocking the turn on a prompt.
    attended: bool,
    /// Deferred actions awaiting a decision, by id.
    deferred_actions: HashMap<String, DeferredAction>,
    /// Decisions by action digest, reused for identical calls in this session.
    action_decisions: HashMap<String, bool>,
    action_tx: Sender<ActionOutcome>,
    action_rx: Receiver<ActionOutcome>,
    update_receiver: Option<Receiver<UpdateNotice>>,
    login_receiver: Option<Receiver<Result<OAuthLoginResult, String>>>,
    omp_login_receiver: Option<Receiver<OmpLoginEvent>>,
    /// Feeds pasted redirect URLs/codes into a manual-callback login worker.
    omp_login_code_tx: Option<Sender<String>>,
    total_input_tokens: u64,
    total_cached_input_tokens: u64,
    total_output_tokens: u64,
    total_cost: f64,
    turn_started_at: Option<SystemTime>,
    turn_goal_tokens: u64,
    goal_continuation_running: bool,
    /// Set when the upstream rejected a turn as over the context window: the
    /// next spawn compacts first (`force_compaction`) and the turn is retried
    /// once (`overflow_retry_pending`). `overflow_retried` stops a second
    /// retry until the user sends a new message.
    force_compaction: bool,
    overflow_retry_pending: bool,
    overflow_retried: bool,
    /// The next spawn continues a turn stopped for mid-turn compaction: it
    /// keeps that turn's subagents and unread steers.
    keep_subagents: bool,
    /// The propose_plan call being written: its call id, its arguments so
    /// far and how much of the plan text clients were sent.
    plan_draft: Option<PlanDraft>,
    resume_summary_running: bool,
    interrupt_flag: Arc<AtomicBool>,
    subagent_manager: SubagentManager,
    /// Messages steered into the running turn, until the model reads them.
    steered_pending: Vec<String>,
    /// Agents of earlier turns: (agent_runs row, transcript), newest last.
    past_subagents: Vec<(Value, Vec<Value>)>,
    /// What the last `agent_runs` showed, and when it went out (throttle).
    agent_runs_revision: u64,
    agent_runs_sent_at: Option<Instant>,
    trust: TrustStore,
    project_trusted: bool,
    hooks: Hooks,
    /// Files read and shells started by this engine (see `tools::ToolState`).
    tool_state: crate::tools::ToolState,
    /// Tools and prompt text added by a host process (the daemon).
    host: Option<crate::host::HostExtensions>,
    /// Chat session (cwd under `~/.jucode/chats/`): chat prompt, no project
    /// instructions or project skills.
    chat: bool,
    plan: Vec<PlanItem>,
    mcp: McpManager,
    /// Version reported in the startup event and used by the update check.
    /// Binaries override it with their own via `with_version`; the default is
    /// the agent-core crate version, which may differ.
    version: &'static str,
}

/// Messages from an omp provider login worker: interim notices (browser URL,
/// device code) then the final outcome keyed by provider id.
enum OmpLoginEvent {
    Notice(String),
    Done {
        provider: String,
        result: Result<LoginOutcome, String>,
    },
}

impl AgentCore {
    pub fn new() -> io::Result<Self> {
        Self::open(env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    /// Opens an engine rooted at `cwd` without touching the process working
    /// directory, so one process can host engines for several projects.
    pub fn open(cwd: PathBuf) -> io::Result<Self> {
        let trust = TrustStore::load_or_create()?;
        let project_trusted = if trust::project_has_local_resources(&cwd) {
            trust.decision_for(&cwd).unwrap_or(false)
        } else {
            true
        };
        let hooks = Hooks::load(&profile_dir()?, &cwd, project_trusted);
        let chat = crate::chat::is_chat_dir(&cwd);
        let config = Config::load_or_create().inspect_err(|error| {
            crate::log_error!("config", "failed to load config", error = error.to_string());
        })?;
        let approval_mode = config.approval_mode;
        // Enabled MCP servers connect on background threads; the agent is
        // usable immediately and their tools appear once connected.
        let mcp = McpManager::default();
        mcp.start(&config.mcp_servers, &cwd);
        let auth = AuthStore::load_or_create(config.encrypt_secrets).inspect_err(|error| {
            crate::log_error!("auth", "failed to load auth", error = error.to_string());
        })?;
        let tool_state = crate::tools::ToolState::default();
        tool_state.set_sandbox(Some(config.sandbox.clone()));
        let session = SessionStore::new();
        // A fresh session id is unique, so this only fails on IO problems.
        let session_lock = SessionLock::acquire(&profile_dir()?, &cwd, session.session_id()).ok();
        let (action_tx, action_rx) = mpsc::channel();
        Ok(Self {
            config,
            auth,
            session,
            session_lock,
            profile_dir: profile_dir()?,
            cwd,
            queued: VecDeque::new(),
            pending_images: Vec::new(),
            running: false,
            receiver: None,
            resume_summary_receiver: None,
            goal_tool_receiver: None,
            update_receiver: None,
            login_receiver: None,
            omp_login_receiver: None,
            omp_login_code_tx: None,
            tag_turns: false,
            turn_tag: None,
            total_input_tokens: 0,
            total_cached_input_tokens: 0,
            total_output_tokens: 0,
            total_cost: 0.0,
            turn_started_at: None,
            turn_goal_tokens: 0,
            goal_continuation_running: false,
            force_compaction: false,
            overflow_retry_pending: false,
            overflow_retried: false,
            keep_subagents: false,
            plan_draft: None,
            resume_summary_running: false,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            subagent_manager: SubagentManager::default(),
            steered_pending: Vec::new(),
            past_subagents: Vec::new(),
            agent_runs_revision: 0,
            agent_runs_sent_at: None,
            trust,
            project_trusted,
            hooks,
            tool_state,
            host: None,
            chat,
            plan: Vec::new(),
            approval_receiver: None,
            pending_approvals: HashMap::new(),
            approved_tools: HashSet::new(),
            approval_mode: LiveApprovalMode::new(approval_mode),
            attended: true,
            deferred_actions: HashMap::new(),
            action_decisions: HashMap::new(),
            action_tx,
            action_rx,
            mcp,
            version: env!("CARGO_PKG_VERSION"),
        })
    }

    /// Sets the version reported by `startup_events` and the update check.
    /// Front-end binaries should pass their own package version.
    pub fn with_version(mut self, version: &'static str) -> Self {
        self.version = version;
        self
    }

    pub fn session_id(&self) -> &str {
        self.session.session_id()
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    pub fn is_chat(&self) -> bool {
        self.chat
    }

    /// Runs this engine's shell commands in `sandbox` and checks file writes
    /// against it (None: no sandbox). Applies to the next tool call.
    pub fn set_sandbox(&mut self, sandbox: Option<crate::sandbox::SandboxPolicy>) {
        self.tool_state.set_sandbox(sandbox);
    }

    /// Counts `dirs` as workspace: file tools may write them, and sandboxed
    /// commands too.
    pub fn add_writable_dirs(&mut self, dirs: &[PathBuf]) {
        let mut sandbox = self
            .tool_state
            .sandbox()
            .unwrap_or_else(|| self.config.sandbox.clone());
        sandbox.writable_dirs.extend_from_slice(dirs);
        self.tool_state.set_sandbox(Some(sandbox));
    }

    /// Adds host tools and prompt text; they apply from the next turn.
    pub fn set_host_extensions(&mut self, host: crate::host::HostExtensions) {
        self.host = Some(host);
    }

    /// Writes the session to disk now, even before its first message, so a
    /// host can reopen it by id at any time.
    pub fn save_session(&mut self) -> io::Result<()> {
        self.session.save_for_cwd(&self.profile_dir, &self.cwd)
    }

    /// The current branch as a transcript, for a client that attaches to a
    /// running session and needs the conversation so far.
    pub fn transcript_event(&self) -> AgentEvent {
        AgentEvent::Transcript(self.session.transcript_items())
    }

    /// Startup batch: the session state plus the trust prompt and the
    /// output of session_start hooks, which run here.
    pub fn startup_events(&self) -> Vec<AgentEvent> {
        let mut events = self.state_events();
        if let Some(Err(error)) = self
            .tool_state
            .sandbox()
            .map(|sandbox| sandbox.check_available())
        {
            events.push(AgentEvent::Error(format!(
                "sandbox unavailable, shell commands will fail: {error}"
            )));
        }
        for message in self.hooks.session_start(&self.cwd) {
            events.push(AgentEvent::Info(message));
        }
        events
    }

    /// The session state a client needs to show it (identity, model,
    /// commands, approval mode, MCP servers, a pending trust prompt), with
    /// no side effects, so it can be sent again to a client that attaches.
    pub fn state_events(&self) -> Vec<AgentEvent> {
        let model_config = self.config.current_model_config();
        let mut events = vec![
            AgentEvent::Startup {
                version: self.version.to_string(),
                session_id: self.session.session_id().to_string(),
                profile_dir: self.config.profile_dir().display().to_string(),
                config_path: self.config.path().display().to_string(),
                cwd: self.cwd.display().to_string(),
                model: self.config.model.clone(),
                context_window: model_config.context_window,
            },
            self.model_status_event(),
            self.command_list_event(),
            self.approval_mode_event(),
            // Servers still connecting report state "connecting"; a follow-up
            // event is emitted from poll_events when their state settles.
            self.mcp_servers_event(),
        ];
        if trust::project_has_local_resources(&self.cwd)
            && self.trust.decision_for(&self.cwd).is_none()
        {
            events.push(AgentEvent::TrustPrompt {
                cwd: self.cwd.display().to_string(),
                repo_root: trust::repo_root(&self.cwd).map(|path| path.display().to_string()),
            });
        }
        events
    }

    pub fn start_update_check(&mut self) {
        if self.update_receiver.is_none() {
            self.update_receiver = Some(update::spawn_update_check(self.version));
        }
    }

    /// Token count at which auto-compaction triggers — the honest denominator for
    /// the UI context gauge. Matches `should_auto_compact`.
    fn effective_context_limit(&self) -> u64 {
        target_context_budget(
            &self.config.current_model_config(),
            self.config.compaction_threshold_percent,
        ) as u64
    }

    /// The configured effort, or a valid fallback when it isn't one the current
    /// model supports (e.g. a stale "medium" after switching to a high/max model).
    fn effective_reasoning_effort(&self) -> String {
        let efforts = self.config.current_model_config().reasoning_efforts;
        if efforts.is_empty() || efforts.iter().any(|e| e == &self.config.reasoning_effort) {
            return self.config.reasoning_effort.clone();
        }
        efforts
            .iter()
            .find(|e| e.as_str() == "high")
            .cloned()
            .unwrap_or_else(|| efforts[efforts.len() / 2].clone())
    }

    pub fn model_status_event(&self) -> AgentEvent {
        let state = if self.running {
            "streaming".to_string()
        } else if self.queued.is_empty() {
            "ready".to_string()
        } else {
            format!("queued: {}", self.queued.len())
        };

        let model_config = self.config.current_model_config();
        AgentEvent::ModelStatus {
            provider: self.config.provider.clone(),
            model: self.config.model.clone(),
            model_label: model_config.display_name.clone(),
            reasoning_effort: self.effective_reasoning_effort(),
            context_window: model_config.context_window,
            context_limit: self.effective_context_limit(),
            max_output_tokens: model_config.max_output_tokens,
            reasoning_efforts: model_config.reasoning_efforts,
            state,
        }
    }

    fn command_list_event(&self) -> AgentEvent {
        let mut commands = crate::commands::COMMANDS
            .iter()
            // Advanced commands still dispatch when typed; they just stay out
            // of the completion menu and `/help` so the common set reads clean.
            .filter(|spec| !spec.advanced)
            .map(|spec| CommandView {
                command: spec.name.to_string(),
                marker: None,
                args: spec.args.to_string(),
                description: spec.description.to_string(),
            })
            .collect::<Vec<_>>();
        if let Ok(skill_commands) =
            skill_commands(self.config.profile_dir(), &self.cwd, self.project_trusted)
        {
            commands.extend(skill_commands.into_iter().map(|entry| CommandView {
                command: entry.command,
                marker: Some("SKILL".to_string()),
                args: String::new(),
                description: entry.skill.description,
            }));
        }
        commands.extend(
            self.mcp
                .prompt_commands()
                .into_iter()
                .map(|entry| CommandView {
                    command: entry.command,
                    marker: Some("MCP".to_string()),
                    args: entry.args,
                    description: entry.description,
                }),
        );
        if let Ok(custom) = crate::custom_commands::discover_custom_commands(
            self.config.profile_dir(),
            &self.cwd,
            self.project_trusted,
        ) {
            // Built-ins, skills, and MCP prompts win on name collisions; only add the rest.
            let taken = commands
                .iter()
                .map(|existing| existing.command.clone())
                .collect::<HashSet<_>>();
            commands.extend(
                custom
                    .into_iter()
                    .filter(|entry| !taken.contains(&entry.command))
                    .map(|entry| CommandView {
                        command: entry.command,
                        marker: Some(if entry.project_scoped { "PROJ" } else { "CMD" }.to_string()),
                        args: "[args]".to_string(),
                        description: entry.description,
                    }),
            );
        }
        AgentEvent::CommandList(commands)
    }

    fn trust_command_events(&mut self, arg: &str) -> Vec<AgentEvent> {
        let (path, trusted) = match arg.split_whitespace().next().unwrap_or("") {
            "" => {
                let status = if self.project_trusted {
                    "trusted"
                } else {
                    "not trusted"
                };
                let mut events = vec![AgentEvent::Info(format!(
                    "project {}: {status}",
                    self.cwd.display()
                ))];
                if trust::project_has_local_resources(&self.cwd) {
                    events.push(AgentEvent::TrustPrompt {
                        cwd: self.cwd.display().to_string(),
                        repo_root: trust::repo_root(&self.cwd).map(|p| p.display().to_string()),
                    });
                }
                return events;
            }
            "yes" => (self.cwd.clone(), true),
            "no" => (self.cwd.clone(), false),
            "repo" => (
                trust::repo_root(&self.cwd).unwrap_or_else(|| self.cwd.clone()),
                true,
            ),
            other => {
                return vec![AgentEvent::Error(format!(
                    "usage: /trust [yes|no|repo] (got '{other}')"
                ))]
            }
        };
        if let Err(error) = self.trust.set(&path, trusted) {
            return vec![AgentEvent::Error(format!(
                "failed to save trust decision: {error}"
            ))];
        }
        self.project_trusted = self.trust.decision_for(&self.cwd).unwrap_or(trusted);
        self.hooks = Hooks::load(self.config.profile_dir(), &self.cwd, self.project_trusted);
        vec![
            AgentEvent::Status(format!(
                "{} {}",
                if trusted { "trusted" } else { "untrusted" },
                path.display()
            )),
            self.command_list_event(),
        ]
    }

    pub fn submit_user_message(&mut self, message: String) -> Vec<AgentEvent> {
        self.submit_user_message_with_images(message, Vec::new())
    }

    pub fn submit_user_message_with_images(
        &mut self,
        message: String,
        images: Vec<String>,
    ) -> Vec<AgentEvent> {
        // Images staged with /image ride along with the next message from any
        // front-end (TUI, serve, acp), merged before explicit attachments.
        let mut merged = std::mem::take(&mut self.pending_images);
        merged.extend(images);
        let (images, mut events) = self.validate_image_attachments(merged);
        if self.running {
            self.queued.push_back((message, images));
            events.push(AgentEvent::PendingMessages(self.pending_texts()));
            events.push(AgentEvent::Status(format!("queued: {}", self.queued.len())));
            return events;
        }
        events.push(AgentEvent::UserMessage(message.clone()));
        events.extend(self.start_hooked_turn(message, images));
        events
    }

    /// Runs a turn on the conversation as it stands, with no new user message:
    /// after a turn that failed (a dropped connection), the model picks up
    /// where it stopped, so a retry leaves no "continue" in the history.
    pub fn continue_turn(&mut self) -> Vec<AgentEvent> {
        if self.running {
            return vec![AgentEvent::Error("a turn is already running".to_string())];
        }
        if self.session.user_turns().is_empty() {
            return vec![AgentEvent::Error("nothing to continue".to_string())];
        }
        self.overflow_retried = false;
        self.start_turn_from_existing_context()
    }

    /// Splits attachment paths into valid ones (kept) and a warning event per
    /// unattachable path. Reads no file contents.
    fn validate_image_attachments(&self, images: Vec<String>) -> (Vec<String>, Vec<AgentEvent>) {
        let mut valid = Vec::new();
        let mut events = Vec::new();
        for path in images {
            match crate::tools::image_attachment_error(std::path::Path::new(&path)) {
                Some(error) => events.push(AgentEvent::Info(format!("skipped attachment {error}"))),
                None => valid.push(path),
            }
        }
        (valid, events)
    }

    fn pending_texts(&self) -> Vec<String> {
        self.queued.iter().map(|(text, _)| text.clone()).collect()
    }

    /// Sends the next queued message into the running turn: the model reads
    /// it before its next request (after the current tool calls finish), and
    /// running tools and subagents keep going. A message with images, or one
    /// sent while nothing runs, starts a turn of its own as before.
    pub fn steer(&mut self) -> Vec<AgentEvent> {
        if !self.running || self.queued.is_empty() {
            return Vec::new();
        }
        if self
            .queued
            .front()
            .is_some_and(|(_, images)| images.is_empty())
        {
            let Some((next, _)) = self.queued.pop_front() else {
                return Vec::new();
            };
            if let Err(reason) = self.hooks.user_prompt_submit(&next, &self.cwd) {
                return vec![
                    AgentEvent::Error(format!("blocked by user_prompt_submit hook: {reason}")),
                    AgentEvent::PendingMessages(self.pending_texts()),
                ];
            }
            self.subagent_manager.steer_main(&next);
            self.steered_pending.push(next);
            return vec![
                AgentEvent::Status("steering".to_string()),
                AgentEvent::PendingMessages(self.pending_texts()),
            ];
        }
        self.stop_current_turn();
        let Some((next, images)) = self.queued.pop_front() else {
            return Vec::new();
        };
        let mut events = vec![
            AgentEvent::Status("steering".to_string()),
            AgentEvent::PendingMessages(self.pending_texts()),
            AgentEvent::UserMessage(next.clone()),
        ];
        events.extend(self.start_hooked_turn(next, images));
        events
    }

    /// Stops the in-flight turn: signals the worker to abort, closes subagents,
    /// and clears all per-turn channels and state. Shared by interrupt and steer
    /// so the old turn's tools never run concurrently with a new one.
    fn stop_current_turn(&mut self) {
        self.interrupt_flag.store(true, Ordering::SeqCst);
        self.subagent_manager.close_all();
        self.requeue_unread_steers();
        self.receiver = None;
        self.running = false;
        self.goal_tool_receiver = None;
        self.approval_receiver = None;
        self.pending_approvals.clear();
        self.goal_continuation_running = false;
        self.turn_started_at = None;
        self.turn_goal_tokens = 0;
    }

    /// Runs the user_prompt_submit hook before starting a turn. Every path that
    /// starts a turn from user-authored input must go through this.
    fn start_hooked_turn(&mut self, message: String, images: Vec<String>) -> Vec<AgentEvent> {
        if let Err(reason) = self.hooks.user_prompt_submit(&message, &self.cwd) {
            return vec![
                AgentEvent::Error(format!("blocked by user_prompt_submit hook: {reason}")),
                AgentEvent::Status("ready".to_string()),
            ];
        }
        self.start_turn(message, images)
    }

    pub fn interrupt(&mut self) -> Vec<AgentEvent> {
        if !self.running {
            return Vec::new();
        }
        self.stop_current_turn();
        let mut events = vec![
            AgentEvent::Info("request interrupted".to_string()),
            AgentEvent::Status("interrupted".to_string()),
        ];
        if !self.queued.is_empty() {
            events.push(AgentEvent::PendingMessages(self.pending_texts()));
        }
        events.extend(self.drain_subagent_events());
        events
    }

    pub fn handle_command(&mut self, input: &str) -> (bool, Vec<AgentEvent>) {
        let (command, args) = split_command_line(input);
        let mut parts = args.split_whitespace();
        if let Some(events) = self.mcp_prompt_command_events(command, args.trim()) {
            return (false, events);
        }
        if let Some(events) = self.skill_command_events(command, args.trim()) {
            return (false, events);
        }
        if !crate::commands::is_known(command) {
            if let Some(events) = self.custom_command_events(command, args.trim()) {
                return (false, events);
            }
            return (
                false,
                vec![AgentEvent::Error(format!("unknown command: {command}"))],
            );
        }

        let events = match command {
            "/quit" | "/exit" => return (true, Vec::new()),
            "/help" | "/" => vec![AgentEvent::Info(crate::commands::help_line())],
            "/login" => self.login_events(args.trim()),
            "/login-paste" => self.login_paste_events(args.trim()),
            "/usage" => self.usage_events(),
            "/new" => self.new_session_events(),
            "/config" => vec![AgentEvent::Info(format!(
                "provider={} model={} reasoning_effort={} base_url={} jucode_web_url={} jucode_api_url={} auth_key={} api_key_env={} retry_attempts={}",
                self.config.provider,
                self.config.model,
                self.config.reasoning_effort,
                self.config.base_url,
                self.config.jucode_web_url,
                self.config.jucode_api_url,
                mask_key(self.provider_api_key().as_deref()),
                self.config.api_key_env,
                self.config.retry_attempts
            ))],
            "/model" => self.model_command_events(parts.collect()),
            "/tree" => vec![AgentEvent::TreeView(self.session.tree_view())],
            "/trust" => self.trust_command_events(args.trim()),
            "/checkout" => {
                let label = args.trim();
                if label.is_empty() {
                    vec![AgentEvent::TreeView(self.session.tree_view())]
                } else {
                    let fill = self.session.user_content(label);
                    match self.session.checkout(label) {
                        Ok(()) => {
                            let save_event = self.save_session_event();
                            let mut events =
                                vec![AgentEvent::Transcript(self.session.transcript_items())];
                            if let Some(content) = fill {
                                events.push(AgentEvent::FillInput(content));
                            }
                            events.push(AgentEvent::Status(format!("checked out {label}")));
                            events.into_iter().chain(save_event).collect()
                        }
                        Err(error) => vec![AgentEvent::Error(error)],
                    }
                }
            }
            "/fork" => {
                let label = args.trim();
                match self.session.fork(label) {
                    Ok(id) => {
                        let save_event = self.save_session_event();
                        vec![
                            AgentEvent::Transcript(self.session.transcript_items()),
                            AgentEvent::TreeView(self.session.tree_view()),
                            AgentEvent::Status(format!("forked {label}: {}", id.display())),
                        ]
                        .into_iter()
                        .chain(save_event)
                        .collect()
                    }
                    Err(error) => vec![AgentEvent::Error(error)],
                }
            }
            "/delete" => {
                let label = args.trim();
                match self.session.delete_branch(label) {
                    Ok(()) => {
                        let save_event = self.save_session_event();
                        vec![
                            AgentEvent::Transcript(self.session.transcript_items()),
                            AgentEvent::TreeView(self.session.tree_view()),
                            AgentEvent::Status(format!("deleted branch {label}")),
                        ]
                        .into_iter()
                        .chain(save_event)
                        .collect()
                    }
                    Err(error) => vec![AgentEvent::Error(error)],
                }
            }
            "/resume" => match parts.next() {
                None => self.resume_list_events(),
                Some(session_id) => self.resume_session_events(session_id),
            },
            "/rewind" | "/undo" => match parts.next() {
                None => self.checkpoint_list_events(),
                Some(id) => self.checkpoint_restore_events(id),
            },
            "/approve" => match parse_approve_args(args.trim()) {
                Ok((call_id, allow, always, hunks)) => self.approve(&call_id, allow, always, hunks),
                Err(error) => vec![AgentEvent::Error(error)],
            },
            "/plan" => self.plan_command_events(args.trim()),
            "/permissions" => self.permissions_command_events(args.trim()),
            "/sandbox" => self.sandbox_command_events(args.trim()),
            "/effort" => self.effort_command_events(args.trim()),
            "/subagents" => self.subagents_command_events(args.trim()),
            "/mcp" => self.mcp_command_events(args.trim()),
            "/context" => self.context_events(),
            "/stats" => self.stats_events(),
            "/goal" => self.goal_command_events(args.trim()),
            "/doctor" => self.doctor_events(),
            "/skills" => self.skills_events(args.trim()),
            "/pin" => self.pin_skill_events(args.trim()),
            "/image" => self.image_command_events(args.trim()),
            "/compact" => self.compact_command_events(),
            // Reached only if a command is registered in `commands::COMMANDS` but
            // has no dispatch arm here — a wiring bug, surfaced explicitly.
            _ => vec![AgentEvent::Error(format!(
                "command not implemented: {command}"
            ))],
        };
        (false, events)
    }

    /// `/image <path>`: stage an image so it is attached to the next submitted
    /// user message. Without an argument, lists what is currently staged.
    fn image_command_events(&mut self, arg: &str) -> Vec<AgentEvent> {
        let path = arg.trim().trim_matches('"').trim_matches('\'');
        if path.is_empty() {
            return if self.pending_images.is_empty() {
                vec![AgentEvent::Info(
                    "usage: /image <path> — attach an image to your next message".to_string(),
                )]
            } else {
                vec![AgentEvent::Info(format!(
                    "staged images (sent with your next message):\n{}",
                    self.pending_images.join("\n")
                ))]
            };
        }
        match crate::tools::image_attachment_error(std::path::Path::new(path)) {
            Some(error) => vec![AgentEvent::Error(format!("cannot attach {error}"))],
            None => {
                self.pending_images.push(path.to_string());
                vec![AgentEvent::Info(format!(
                    "attached {path}; it will be sent with your next message ({} staged)",
                    self.pending_images.len()
                ))]
            }
        }
    }

    fn skills_events(&mut self, arg: &str) -> Vec<AgentEvent> {
        let mut parts = arg.split_whitespace();
        match parts.next().unwrap_or("list") {
            "list" => self.list_marketplace_skills_events(),
            "install" => match parts.next() {
                Some(id) => self.install_marketplace_skill_events(id, "installed"),
                None => vec![AgentEvent::Error("usage: /skills install <id>".to_string())],
            },
            "update" => match parts.next() {
                Some(id) => self.install_marketplace_skill_events(id, "updated"),
                None => vec![AgentEvent::Error("usage: /skills update <id>".to_string())],
            },
            "uninstall" => match parts.next() {
                Some(id) => self.uninstall_skill_events(id),
                None => vec![AgentEvent::Error(
                    "usage: /skills uninstall <id>".to_string(),
                )],
            },
            action @ ("enable" | "disable") => match parts.next() {
                Some(id) => self.set_skill_enabled_events(id, action == "enable"),
                None => vec![AgentEvent::Error(format!("usage: /skills {action} <id>"))],
            },
            "sync" => self.sync_default_skills_events(),
            other => vec![AgentEvent::Error(format!(
                "unknown /skills action: {other}; use list, install, update, uninstall, enable, disable, or sync"
            ))],
        }
    }

    fn list_marketplace_skills_events(&self) -> Vec<AgentEvent> {
        let installed = match skills::installed_skill_ids(self.config.profile_dir()) {
            Ok(skills) if skills.is_empty() => "Installed skills: none".to_string(),
            Ok(skills) => format!("Installed skills:\n{}", skills.join("\n")),
            Err(error) => format!("Installed skills: failed to read ({error})"),
        };
        let project_roots = [
            self.cwd.join(".jucode").join("skills"),
            self.cwd.join(".agents").join("skills"),
        ];
        let user_agents_root = crate::secrets::home_dir().map(|home| home.join(".agents"));
        let discovered =
            discover_skills(self.config.profile_dir(), &self.cwd, self.project_trusted);
        let project = if !self.project_trusted && project_roots.iter().any(|root| root.exists()) {
            "Project skills: hidden until project is trusted".to_string()
        } else {
            match &discovered {
                Ok(found) => {
                    let names = found
                        .iter()
                        .filter(|skill| {
                            project_roots
                                .iter()
                                .any(|root| skill.path.starts_with(root))
                        })
                        .map(|skill| skill.name.clone())
                        .collect::<Vec<_>>();
                    if names.is_empty() {
                        "Project skills: none".to_string()
                    } else {
                        format!("Project skills:\n{}", names.join("\n"))
                    }
                }
                Err(error) => format!("Project skills: failed to read ({error})"),
            }
        };
        let user_agents = match (user_agents_root.as_deref(), &discovered) {
            (Some(root), Ok(found)) => {
                let names = found
                    .iter()
                    .filter(|skill| skill.path.starts_with(root))
                    .map(|skill| skill.name.clone())
                    .collect::<Vec<_>>();
                if names.is_empty() {
                    String::new()
                } else {
                    format!("~/.agents skills:\n{}", names.join("\n"))
                }
            }
            _ => String::new(),
        };
        let marketplace = match self.fetch_marketplace() {
            Ok(marketplace) if marketplace.skills.is_empty() => {
                "Source: JuCode marketplace\nNo skills available".to_string()
            }
            Ok(marketplace) => {
                let defaults = marketplace
                    .default_skill_ids
                    .iter()
                    .map(String::as_str)
                    .collect::<std::collections::BTreeSet<_>>();
                let mut lines = Vec::new();
                for skill in marketplace.skills {
                    let marker = if defaults.contains(skill.id.as_str()) {
                        " default"
                    } else {
                        ""
                    };
                    lines.push(format!("{}{} — {}", skill.id, marker, skill.description));
                }
                format!("Source: JuCode marketplace\n{}", lines.join("\n"))
            }
            Err(error) => format!("Source: JuCode marketplace (unavailable: {error})"),
        };
        let extra = match self.fetch_extra_skill_source() {
            Ok(Some(source)) => {
                let (offered, excluded): (Vec<_>, Vec<_>) = source
                    .skills
                    .iter()
                    .partition(|skill| skill.redistributable);
                let mut lines = offered
                    .iter()
                    .map(|skill| skill.id.clone())
                    .collect::<Vec<_>>();
                if !excluded.is_empty() {
                    lines.push(format!(
                        "Not offered: {}",
                        excluded
                            .iter()
                            .map(|skill| format!("{} ({})", skill.id, not_offered_reason(skill)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                Some(format!(
                    "Source: {} ({})\n{}",
                    source.name,
                    source.repository,
                    if lines.is_empty() {
                        "No skills available".to_string()
                    } else {
                        lines.join("\n")
                    }
                ))
            }
            Ok(None) => None,
            Err(error) => Some(format!("Extra skills source unavailable: {error}")),
        };
        let mut sections = vec![installed, project];
        if !user_agents.is_empty() {
            sections.push(user_agents);
        }
        sections.push(marketplace);
        sections.extend(extra);
        sections.push(
            "Install with /skills install <id>; update with /skills update <id>; sync JuCode defaults with /skills sync."
                .to_string(),
        );
        vec![AgentEvent::Info(sections.join("\n\n"))]
    }

    fn install_marketplace_skill_events(&mut self, id: &str, verb: &str) -> Vec<AgentEvent> {
        let profile = self.config.profile_dir().to_path_buf();
        let skills_dir = profile.join("skills");
        if verb == "updated" && !skills::skill_installed(&skills_dir, id) {
            return vec![AgentEvent::Error(format!(
                "installed skill not found: {id}"
            ))];
        }
        let marketplace = self.fetch_marketplace();
        if let Ok(marketplace) = &marketplace {
            if let Some(skill) = marketplace.skills.iter().find(|skill| skill.id == id) {
                let installed = skills::install_marketplace_skill(&skills_dir, skill)
                    .and_then(|_| skills::set_skill_enabled(&profile, &skill.id, true));
                return match installed {
                    Ok(()) => vec![
                        AgentEvent::Status(format!(
                            "{verb} skill {} from JuCode marketplace",
                            skill.id
                        )),
                        self.command_list_event(),
                    ],
                    Err(error) => vec![AgentEvent::Error(format!(
                        "failed to install skill {}: {error}",
                        skill.id
                    ))],
                };
            }
        }
        match self.fetch_extra_skill_source() {
            Ok(Some(source)) => {
                if let Some(skill) = source.skills.iter().find(|skill| skill.id == id) {
                    if !skill.redistributable {
                        return vec![AgentEvent::Error(format!(
                            "skill {} is not offered by {}: {}",
                            skill.id,
                            source.name,
                            not_offered_reason(skill)
                        ))];
                    }
                    let installed = skills::install_source_skill(&skills_dir, &source, skill)
                        .and_then(|_| skills::set_skill_enabled(&profile, &skill.id, true));
                    return match installed {
                        Ok(()) => vec![
                            AgentEvent::Status(format!(
                                "{verb} skill {} from {}",
                                skill.id, source.name
                            )),
                            self.command_list_event(),
                        ],
                        Err(error) => vec![AgentEvent::Error(format!(
                            "failed to install skill {} from {}: {error}",
                            skill.id, source.name
                        ))],
                    };
                }
            }
            Ok(None) => {}
            Err(error) => {
                return vec![AgentEvent::Error(format!(
                    "failed to load extra skills source: {error}"
                ))];
            }
        }
        match marketplace {
            Ok(_) => vec![AgentEvent::Error(format!("skill not found in configured sources: {id}"))],
            Err(error) => vec![AgentEvent::Error(format!(
                "skill not found in configured extra source and JuCode marketplace is unavailable: {error}"
            ))],
        }
    }

    fn uninstall_skill_events(&mut self, id: &str) -> Vec<AgentEvent> {
        match skills::uninstall_skill(self.config.profile_dir(), id) {
            Ok(true) => vec![
                AgentEvent::Status(format!("uninstalled skill {id}")),
                self.command_list_event(),
            ],
            Ok(false) => vec![AgentEvent::Error(format!(
                "installed skill not found: {id}"
            ))],
            Err(error) => vec![AgentEvent::Error(format!(
                "failed to uninstall skill {id}: {error}"
            ))],
        }
    }

    fn set_skill_enabled_events(&mut self, id: &str, enabled: bool) -> Vec<AgentEvent> {
        if !skills::skill_installed(&self.config.profile_dir().join("skills"), id) {
            return vec![AgentEvent::Error(format!(
                "installed skill not found: {id}"
            ))];
        }
        match skills::set_skill_enabled(self.config.profile_dir(), id, enabled) {
            Ok(()) => vec![
                AgentEvent::Status(format!(
                    "{} skill {id}",
                    if enabled { "enabled" } else { "disabled" }
                )),
                self.command_list_event(),
            ],
            Err(error) => vec![AgentEvent::Error(format!(
                "failed to {} skill {id}: {error}",
                if enabled { "enable" } else { "disable" }
            ))],
        }
    }

    /// Runs in the background after login. A failure here does not affect the
    /// session, so it is reported as a notice, not an error in the transcript.
    fn sync_default_skills_events(&mut self) -> Vec<AgentEvent> {
        match self.fetch_marketplace() {
            Ok(marketplace) => {
                match skills::install_default_skills(self.config.profile_dir(), &marketplace) {
                    Ok(0) => vec![AgentEvent::Info(
                        "no default marketplace skills configured".to_string(),
                    )],
                    Ok(count) => vec![
                        AgentEvent::Status(format!("synced {count} default skill(s)")),
                        self.command_list_event(),
                    ],
                    Err(error) => vec![AgentEvent::Info(format!(
                        "could not sync default skills: {error}"
                    ))],
                }
            }
            Err(error) => vec![AgentEvent::Info(format!(
                "could not fetch the skills marketplace: {error}"
            ))],
        }
    }

    fn fetch_marketplace(&self) -> Result<skills::Marketplace, String> {
        skills::fetch_marketplace(&self.config.jucode_api_url, self.auth.jucode_access_token())
    }

    fn fetch_extra_skill_source(&self) -> Result<Option<skills::SkillSource>, String> {
        skills::fetch_extra_skill_source(
            self.config
                .extra_skills_source
                .as_deref()
                .unwrap_or_default(),
        )
    }

    /// Returns the bearer token for the active provider: the JuCode OAuth
    /// access token for the jucode provider, a stored omp OAuth access token
    /// when logged in via the catalog, otherwise the raw provider key.
    fn provider_api_key(&self) -> Option<String> {
        provider_api_key(&self.config, &self.auth)
    }

    fn model_headers(&self) -> HashMap<String, Vec<(String, String)>> {
        let mut headers = model_headers(&self.config);
        if let (true, Some(tag)) = (self.config.provider == "jucode", &self.turn_tag) {
            // Headers go per model; a turn may call any of these.
            let config = &self.config;
            let names = config
                .models
                .iter()
                .chain(&config.jucode_models)
                .map(|m| m.name.clone())
                .chain(config.subagent_models.iter().map(|m| m.name.clone()))
                .chain([
                    config.model.clone(),
                    config.compact().0,
                    config.safety_model.clone(),
                    config.image_model.clone(),
                ]);
            for name in names {
                let entry = headers.entry(name).or_default();
                if !entry.iter().any(|(header, _)| header == "X-JuCode-Turn") {
                    entry.push(("X-JuCode-Turn".to_string(), tag.clone()));
                }
            }
        }
        headers
    }

    /// Tag this engine's turns (see `tag_turns`); the daemon records usage
    /// per turn.
    pub fn set_tag_turns(&mut self, on: bool) {
        self.tag_turns = on;
    }

    /// The running (or last) turn's tag, when turns are tagged.
    pub fn turn_tag(&self) -> Option<&str> {
        self.turn_tag.as_deref()
    }

    fn new_turn_tag(&mut self) {
        if !self.tag_turns {
            return;
        }
        let mut bytes = [0u8; 12];
        if getrandom::getrandom(&mut bytes).is_ok() {
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            self.turn_tag = Some(format!("t-{hex}"));
        }
    }

    /// Refreshes the active provider's bearer when it's near expiry so the
    /// inference call carries a valid token. Handles the JuCode session and
    /// omp OAuth credentials; BYOK providers return early.
    fn ensure_provider_credentials(&mut self) -> Result<(), String> {
        if self.config.provider != "jucode" {
            return self.ensure_omp_credentials();
        }
        self.auth =
            oauth::ensure_session(&self.config.jucode_api_url, self.config.encrypt_secrets)?;
        Ok(())
    }

    /// Refresh path for omp OAuth credentials (see
    /// [`Self::ensure_provider_credentials`]). No-op when the active provider
    /// has no stored credential — BYOK keys don't expire here.
    fn ensure_omp_credentials(&mut self) -> Result<(), String> {
        let catalog = llm_provider_kit::omp::catalog();
        let provider = &self.config.provider;
        let store_id = catalog
            .auth_provider(provider)
            .and_then(|p| p.store_as.as_deref())
            .unwrap_or(provider)
            .to_string();
        // Pick up tokens written by another process first (same rationale as
        // the jucode reload above).
        self.auth = AuthStore::load_or_create(self.config.encrypt_secrets)
            .map_err(|error| format!("failed to reload auth.json: {error}"))?;
        let Some(credential) = self.auth.oauth_credential(&store_id) else {
            return Ok(());
        };
        let now_ms = llm_provider_kit::oauth::unix_now().saturating_mul(1000);
        // Refresh slightly ahead of expiry; the credential's declared skew is
        // already baked into expires_at_ms.
        if credential.expires_at_ms > now_ms + 120_000 {
            return Ok(());
        }
        if credential.refresh.is_empty() {
            return Err(format!(
                "{provider} session expired and cannot be refreshed. Run /login {provider}."
            ));
        }
        let context = LoginContext {
            profile_dir: &self.profile_dir,
            client_name: CLIENT_NAME,
        };
        match provider_auth::refresh(&store_id, credential, &context) {
            Ok(refreshed) => {
                crate::log_info!("oauth", "refreshed provider token");
                self.auth.set_oauth_credential(&store_id, refreshed);
                self.auth.save().map_err(|error| error.to_string())
            }
            Err(error) => {
                crate::log_error!(
                    "oauth",
                    "provider token refresh failed",
                    error = error.clone()
                );
                // The stored credential is dead (e.g. revoked refresh token);
                // drop it so we don't retry the same failing refresh forever.
                self.auth.clear_oauth(&store_id);
                let _ = self.auth.save();
                Err(format!("failed to refresh {provider}: {error}"))
            }
        }
    }

    /// `/usage` — query the JuCode account via the OAuth read endpoints and
    /// print 套餐 / 余额 / 用量 / 最近调用详情.
    fn usage_events(&mut self) -> Vec<AgentEvent> {
        if self.config.provider != "jucode" {
            return vec![AgentEvent::Error(
                "/usage requires the jucode provider. Run /login.".to_string(),
            )];
        }
        if let Err(error) = self.ensure_provider_credentials() {
            return vec![AgentEvent::Error(error)];
        }
        let Some(token) = self.auth.jucode_access_token().map(str::to_string) else {
            return vec![AgentEvent::Error(
                "not logged in to JuCode. Run /login.".to_string(),
            )];
        };
        let api = self.config.jucode_api_url.clone();
        let mut lines: Vec<String> = Vec::new();

        match oauth::get_json(&api, "/v1/oauth/userinfo", &token) {
            Ok(v) => {
                let email = v.get("email").and_then(Value::as_str).unwrap_or("-");
                let balance = v.get("balance").and_then(Value::as_str).unwrap_or("0");
                let currency = v.get("currency").and_then(Value::as_str).unwrap_or("");
                lines.push(format!("账户: {email}"));
                lines.push(format!("余额: {balance} {currency}"));
                match v
                    .get("active_plan")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                {
                    Some(name) => lines.push(format!("套餐: {name}")),
                    None => lines.push("套餐: 无活跃套餐".to_string()),
                }
            }
            Err(error) => return vec![AgentEvent::Error(format!("查询账户失败: {error}"))],
        }

        if let Ok(v) = oauth::get_json(&api, "/v1/oauth/usage", &token) {
            if v.get("has_active_plan")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let used5 = v.get("used_5h").and_then(Value::as_str).unwrap_or("0");
                let quota5 = v.get("quota_5h").and_then(Value::as_str).unwrap_or("0");
                let usedm = v.get("used_monthly").and_then(Value::as_str).unwrap_or("0");
                let quotam = v
                    .get("quota_monthly")
                    .and_then(Value::as_str)
                    .unwrap_or("0");
                lines.push(format!("5h 用量: {used5} / {quota5}"));
                lines.push(format!("月度用量: {usedm} / {quotam}"));
            }
        }

        if let Ok(v) = oauth::get_json(&api, "/v1/oauth/usage-logs?limit=5", &token) {
            let logs = v
                .get("logs")
                .or_else(|| v.get("items"))
                .and_then(Value::as_array);
            if let Some(logs) = logs.filter(|l| !l.is_empty()) {
                lines.push("最近调用:".to_string());
                for l in logs {
                    let model = l.get("model").and_then(Value::as_str).unwrap_or("-");
                    let tin = l.get("tokens_in").and_then(Value::as_u64).unwrap_or(0);
                    let tout = l.get("tokens_out").and_then(Value::as_u64).unwrap_or(0);
                    let cost = l.get("cost_final").and_then(Value::as_str).unwrap_or("0");
                    lines.push(format!("  {model}  in {tin} / out {tout}  cost {cost}"));
                }
            }
        }

        vec![AgentEvent::Info(lines.join("\n"))]
    }

    fn pin_skill_events(&mut self, name: &str) -> Vec<AgentEvent> {
        if self.running {
            return vec![AgentEvent::Error(
                "cannot pin a skill while a response is running".to_string(),
            )];
        }
        let wanted = name.trim().trim_start_matches('/');
        if wanted.is_empty() {
            return vec![AgentEvent::Error("usage: /pin <skill>".to_string())];
        }
        let commands =
            match skill_commands(self.config.profile_dir(), &self.cwd, self.project_trusted) {
                Ok(commands) => commands,
                Err(error) => {
                    return vec![AgentEvent::Error(format!(
                        "failed to discover skills: {error}"
                    ))]
                }
            };
        let Some(skill) = commands
            .into_iter()
            .find(|entry| {
                entry.command.trim_start_matches('/') == wanted || entry.skill.name == wanted
            })
            .map(|entry| entry.skill)
        else {
            return vec![AgentEvent::Error(format!("skill not found: {wanted}"))];
        };
        let content = match skill_pin_message(&skill) {
            Ok(content) => content,
            Err(error) => return vec![AgentEvent::Error(format!("failed to read skill: {error}"))],
        };
        self.session.append(EntryKind::PinnedSkill {
            name: skill.name.clone(),
            content,
        });
        let mut events = vec![AgentEvent::Status(format!("pinned skill {}", skill.name))];
        events.extend(self.save_session_event());
        events.push(self.context_usage_event());
        events
    }

    fn skill_command_events(&mut self, command: &str, request: &str) -> Option<Vec<AgentEvent>> {
        let commands =
            skill_commands(self.config.profile_dir(), &self.cwd, self.project_trusted).ok()?;
        let skill = commands
            .into_iter()
            .find(|entry| entry.command == command)?
            .skill;
        let message = match skill_message(&skill, request) {
            Ok(message) => message,
            Err(error) => {
                return Some(vec![AgentEvent::Error(format!(
                    "failed to read skill: {error}"
                ))])
            }
        };
        if self.running {
            self.queued.push_back((message, Vec::new()));
            return Some(vec![
                AgentEvent::PendingMessages(self.pending_texts()),
                AgentEvent::Status(format!("queued: {}", self.queued.len())),
            ]);
        }
        let display = if request.is_empty() {
            command.to_string()
        } else {
            format!("{command} {request}")
        };
        let mut events = vec![AgentEvent::UserMessage(display)];
        events.extend(self.start_hooked_turn(message, Vec::new()));
        Some(events)
    }

    fn mcp_prompt_command_events(
        &mut self,
        command: &str,
        arguments: &str,
    ) -> Option<Vec<AgentEvent>> {
        let message = match self.mcp.run_prompt(command, arguments)? {
            Ok(message) => message,
            Err(error) => return Some(vec![AgentEvent::Error(error)]),
        };
        if self.running {
            self.queued.push_back((message, Vec::new()));
            return Some(vec![
                AgentEvent::PendingMessages(self.pending_texts()),
                AgentEvent::Status(format!("queued: {}", self.queued.len())),
            ]);
        }
        let display = if arguments.is_empty() {
            command.to_string()
        } else {
            format!("{command} {arguments}")
        };
        let mut events = vec![AgentEvent::UserMessage(display)];
        events.extend(self.start_hooked_turn(message, Vec::new()));
        Some(events)
    }

    /// Dispatch a user-defined command from `~/.jucode/commands` or a trusted
    /// project's `.jucode/commands`: the Markdown body becomes the user prompt.
    fn custom_command_events(&mut self, command: &str, request: &str) -> Option<Vec<AgentEvent>> {
        let commands = crate::custom_commands::discover_custom_commands(
            self.config.profile_dir(),
            &self.cwd,
            self.project_trusted,
        )
        .ok()?;
        let custom = commands
            .into_iter()
            .find(|entry| entry.command == command)?;
        let message = match crate::custom_commands::command_message(&custom, request) {
            Ok(message) => message,
            Err(error) => {
                return Some(vec![AgentEvent::Error(format!(
                    "failed to read command file {}: {error}",
                    custom.path.display()
                ))])
            }
        };
        if self.running {
            self.queued.push_back((message, Vec::new()));
            return Some(vec![
                AgentEvent::PendingMessages(self.pending_texts()),
                AgentEvent::Status(format!("queued: {}", self.queued.len())),
            ]);
        }
        let display = if request.is_empty() {
            command.to_string()
        } else {
            format!("{command} {request}")
        };
        let mut events = vec![AgentEvent::UserMessage(display)];
        events.extend(self.start_hooked_turn(message, Vec::new()));
        Some(events)
    }

    pub fn poll_events(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let mut disconnected = false;

        if let Some(rx) = self.receiver.take() {
            while let Ok(event) = rx.try_recv() {
                match event {
                    WorkerEvent::PlanDraft { call_id, delta } => {
                        let draft = self.plan_draft.get_or_insert_with(PlanDraft::default);
                        if let Some(event) = draft.push(&call_id, &delta) {
                            events.push(event);
                        }
                    }
                    WorkerEvent::CompactionStart => events.push(AgentEvent::CompactionStart),
                    WorkerEvent::CompactionProgress { output_tokens } => {
                        events.push(AgentEvent::CompactionProgress { output_tokens });
                    }
                    WorkerEvent::CompactionDone {
                        summary,
                        replaced_through,
                    } => {
                        self.session.apply_compaction(summary, replaced_through);
                        events.extend(self.save_session_event());
                        events.push(AgentEvent::CompactionEnd);
                        events.push(self.context_usage_event());
                    }
                    WorkerEvent::CompactionFailed(error) => {
                        events.push(AgentEvent::CompactionFailed(error));
                    }
                    // Resume-summary events arrive only on resume_summary_receiver.
                    WorkerEvent::ResumeSummaryDone { .. } | WorkerEvent::ResumeSummaryFailed(_) => {
                    }
                    WorkerEvent::CallStart => events.push(AgentEvent::Connecting),
                    WorkerEvent::Connected => events.push(AgentEvent::ThinkingStart),
                    WorkerEvent::ReasoningDelta(delta) => {
                        events.push(AgentEvent::ReasoningDelta(delta))
                    }
                    WorkerEvent::Delta(delta) => events.push(AgentEvent::AssistantDelta(delta)),
                    WorkerEvent::Retrying {
                        attempt,
                        max_attempts,
                        reason,
                        delay_ms,
                    } => {
                        events.push(AgentEvent::Retrying {
                            attempt,
                            max_attempts,
                            reason,
                            delay_ms,
                        });
                    }
                    WorkerEvent::Steered(message) => {
                        if let Some(at) = self.steered_pending.iter().position(|m| *m == message) {
                            self.steered_pending.remove(at);
                        }
                        self.session.append(EntryKind::User {
                            content: message.clone(),
                        });
                        events.extend(self.save_session_event());
                        events.push(AgentEvent::UserMessage(message));
                        events.push(AgentEvent::PendingMessages(self.pending_texts()));
                    }
                    WorkerEvent::ResponseItem(item) => {
                        // The request went through: a later overflow in this
                        // turn may compact and retry again.
                        self.overflow_retried = false;
                        self.session.append(EntryKind::ResponseItem { item });
                        events.extend(self.save_session_event());
                        events.push(self.context_usage_event());
                    }
                    WorkerEvent::ToolStart { call_id, name } => {
                        events.push(AgentEvent::ToolStart { call_id, name });
                    }
                    WorkerEvent::ToolUpdate {
                        call_id,
                        name,
                        output,
                    } => {
                        events.push(AgentEvent::ToolUpdate {
                            call_id,
                            name,
                            output,
                        });
                    }
                    WorkerEvent::ToolOutput {
                        call_id,
                        name,
                        output,
                        model_output,
                        is_error,
                    } => {
                        self.session.append(EntryKind::ToolOutput {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            output: model_output.clone(),
                            // The base64 payload is stripped from model_output;
                            // keep the image item so next-turn projection can
                            // re-attach the pixels the model saw this turn.
                            image: crate::tools::image_content_item(&output),
                        });
                        events.extend(self.save_session_event());
                        events.push(self.context_usage_event());
                        events.push(AgentEvent::ToolOutput {
                            call_id,
                            name,
                            output,
                            is_error,
                        });
                    }
                    WorkerEvent::Usage {
                        input_tokens,
                        cached_input_tokens,
                        output_tokens,
                        reasoning_tokens,
                    } => {
                        // Both providers deliver OpenAI subset semantics here
                        // (cached_input_tokens ⊆ input_tokens); the Anthropic
                        // parser normalizes its disjoint counts in llm.rs.
                        self.total_input_tokens += input_tokens;
                        self.total_cached_input_tokens += cached_input_tokens;
                        self.total_output_tokens += output_tokens;
                        self.total_cost += self.config.current_model_config().cost_for(
                            input_tokens,
                            cached_input_tokens,
                            output_tokens,
                        );
                        let non_cached_input_tokens =
                            input_tokens.saturating_sub(cached_input_tokens);
                        self.turn_goal_tokens = self
                            .turn_goal_tokens
                            .saturating_add(non_cached_input_tokens.saturating_add(output_tokens));
                        events.push(AgentEvent::Usage {
                            input_tokens,
                            cached_input_tokens,
                            output_tokens,
                            reasoning_tokens,
                        });
                    }
                    WorkerEvent::Done => {
                        self.subagent_manager
                            .close_all_with_message("parent turn finished");
                        // Steered after the model's last request: run next.
                        self.requeue_unread_steers();
                        events.extend(self.finish_goal_turn());
                        self.running = false;
                        disconnected = true;
                        self.goal_tool_receiver = None;
                        events.push(self.context_usage_event());
                        for message in self.hooks.stop(&self.cwd) {
                            events.push(AgentEvent::Info(message));
                        }
                        if !self.queued.is_empty() {
                            events.push(AgentEvent::PendingMessages(self.pending_texts()));
                        }
                        events.push(AgentEvent::Status(if self.queued.is_empty() {
                            "ready".to_string()
                        } else {
                            format!("queued: {}", self.queued.len())
                        }));
                    }
                    WorkerEvent::Error(error)
                        if error == crate::llm::MID_TURN_COMPACTION
                            || is_context_overflow(&error)
                                && !self.overflow_retried
                                && self
                                    .session
                                    .plan_compaction(
                                        COMPACTION_KEEP_RECENT_TOKENS,
                                        &self.config.model,
                                    )
                                    .is_some() =>
                    {
                        // Over the window (unknown, or raised past what this
                        // route serves): compact and retry once instead of
                        // failing the turn. The retry spawns from the idle
                        // branch below, once this worker is drained. A
                        // mid-turn compaction continues the same turn, so its
                        // subagents keep running.
                        if error == crate::llm::MID_TURN_COMPACTION {
                            self.keep_subagents = true;
                        } else {
                            self.subagent_manager
                                .close_all_with_message("parent turn hit the context window");
                        }
                        self.running = false;
                        disconnected = true;
                        self.goal_tool_receiver = None;
                        self.overflow_retried = true;
                        self.overflow_retry_pending = true;
                        self.force_compaction = true;
                        events.push(AgentEvent::Info(
                            if error == crate::llm::MID_TURN_COMPACTION {
                                "context reached the compaction threshold; compacting and continuing"
                            } else {
                                "request exceeded the model's context window; compacting and retrying"
                            }
                            .to_string(),
                        ));
                    }
                    WorkerEvent::Error(error) => {
                        self.subagent_manager
                            .close_all_with_message("parent turn failed");
                        self.requeue_unread_steers();
                        events.extend(self.finish_goal_turn());
                        self.running = false;
                        disconnected = true;
                        self.goal_tool_receiver = None;
                        events.push(AgentEvent::Error(error));
                        events.push(self.context_usage_event());
                        for message in self.hooks.stop(&self.cwd) {
                            events.push(AgentEvent::Info(message));
                        }
                        if !self.queued.is_empty() {
                            events.push(AgentEvent::PendingMessages(self.pending_texts()));
                        }
                        events.push(AgentEvent::Status(if self.queued.is_empty() {
                            "ready".to_string()
                        } else {
                            format!("queued: {}", self.queued.len())
                        }));
                    }
                }
            }
            if !disconnected {
                self.receiver = Some(rx);
            }
        }
        events.extend(self.drain_subagent_events());
        events.extend(self.fold_subagent_usage());
        events.extend(self.poll_resume_summary_events());
        if let Some(rx) = self.update_receiver.take() {
            match rx.try_recv() {
                Ok(notice) => events.push(AgentEvent::Info(notice.message())),
                Err(mpsc::TryRecvError::Empty) => self.update_receiver = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
        }
        if let Some(rx) = self.login_receiver.take() {
            match rx.try_recv() {
                Ok(Ok(result)) => events.extend(self.apply_login_result(result)),
                Ok(Err(error)) => {
                    events.push(AgentEvent::Error(format!("JuCode login failed: {error}")))
                }
                Err(mpsc::TryRecvError::Empty) => self.login_receiver = Some(rx),
                Err(mpsc::TryRecvError::Disconnected) => {}
            }
        }
        if let Some(rx) = self.omp_login_receiver.take() {
            let mut finished = None;
            let mut disconnected = false;
            loop {
                match rx.try_recv() {
                    Ok(OmpLoginEvent::Notice(text)) => events.push(AgentEvent::Info(text)),
                    Ok(OmpLoginEvent::Done { provider, result }) => {
                        finished = Some((provider, result));
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if let Some((provider, result)) = finished {
                self.omp_login_code_tx = None;
                events.extend(self.apply_omp_login_result(provider, result));
            } else if disconnected {
                // The worker died without a Done (panic or channel drop) —
                // never leave the login invisible.
                self.omp_login_code_tx = None;
                events.push(AgentEvent::Error(
                    "provider login ended unexpectedly — try /login again".to_string(),
                ));
            } else {
                self.omp_login_receiver = Some(rx);
            }
        }
        events.extend(self.poll_goal_tool_requests());
        events.extend(self.poll_approval_requests());
        events.extend(self.poll_action_outcomes());
        self.mcp.refresh_changed();
        for message in self.mcp.drain_messages() {
            events.push(AgentEvent::Info(message));
        }
        if self.mcp.take_dirty() {
            events.push(self.mcp_servers_event());
            events.push(self.command_list_event());
        }
        if self.should_generate_resume_summary() {
            self.start_resume_summary();
        }

        if !self.running {
            if std::mem::take(&mut self.overflow_retry_pending) {
                let save_event = self.save_session_event();
                events.extend(self.spawn_current_context_turn(save_event));
            } else if let Some((next, images)) = self.queued.pop_front() {
                self.goal_continuation_running = false;
                events.push(AgentEvent::PendingMessages(self.pending_texts()));
                events.push(AgentEvent::UserMessage(next.clone()));
                events.extend(self.start_hooked_turn(next, images));
            } else if self.should_continue_goal() {
                events.extend(self.start_goal_continuation());
            }
        }

        events
    }

    /// Drains the idle resume-summary worker. Its result is terminal, so the
    /// receiver is dropped after the first event (or on disconnect).
    fn poll_resume_summary_events(&mut self) -> Vec<AgentEvent> {
        let Some(rx) = self.resume_summary_receiver.take() else {
            return Vec::new();
        };
        match rx.try_recv() {
            Ok(WorkerEvent::ResumeSummaryDone {
                summary,
                status,
                summarized_at,
            }) => {
                self.resume_summary_running = false;
                self.session
                    .set_resume_summary(Some(summary), Some(status), summarized_at);
                self.save_session_event()
            }
            Ok(WorkerEvent::ResumeSummaryFailed(_error)) => {
                self.resume_summary_running = false;
                self.session.set_resume_summary(
                    None,
                    self.session
                        .goal()
                        .map(|goal| normalize_resume_status(goal.status)),
                    now_secs(),
                );
                self.save_session_event()
            }
            // The resume worker sends no other events.
            Ok(_) => {
                self.resume_summary_running = false;
                Vec::new()
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.resume_summary_receiver = Some(rx);
                Vec::new()
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.resume_summary_running = false;
                Vec::new()
            }
        }
    }

    fn drain_subagent_events(&mut self) -> Vec<AgentEvent> {
        let mut events: Vec<AgentEvent> = self
            .subagent_manager
            .drain_events()
            .into_iter()
            .map(|event| {
                let (label, model, tool_use_id) = self
                    .subagent_manager
                    .describe(&event.path)
                    .unwrap_or_default();
                AgentEvent::SubagentLifecycle {
                    path: event.path,
                    status: event.status,
                    message: event.message,
                    label,
                    model,
                    tool_use_id,
                }
            })
            .collect();
        // The agent trace: at most two refreshes a second while agents work;
        // a lifecycle change goes out at once.
        let revision = self.subagent_manager.trace_revision();
        if revision != self.agent_runs_revision {
            let due = !events.is_empty()
                || self
                    .agent_runs_sent_at
                    .is_none_or(|sent| sent.elapsed() >= Duration::from_millis(500));
            if due {
                self.agent_runs_revision = revision;
                self.agent_runs_sent_at = Some(Instant::now());
                events.push(self.agent_runs_event());
            }
        }
        events
    }

    /// A turn that ended before reading what was steered into it: those
    /// messages run next, ahead of anything queued after them, instead of
    /// being lost. `steered_pending` is what the session has not recorded,
    /// including a message the worker read whose event the turn's end
    /// dropped; the inbox is emptied with it.
    fn requeue_unread_steers(&mut self) {
        self.subagent_manager.drain_user_inbox();
        for message in self.steered_pending.drain(..).rev() {
            self.queued.push_front((message, Vec::new()));
        }
    }

    /// `agent_runs`: this session's subagents, earlier turns' first.
    pub fn agent_runs_event(&self) -> AgentEvent {
        let current = self.subagent_manager.runs_json();
        let mut agents: Vec<Value> = self
            .past_subagents
            .iter()
            .map(|(row, _)| row.clone())
            .filter(|row| !current.iter().any(|now| now["id"] == row["id"]))
            .collect();
        agents.extend(current);
        AgentEvent::AgentRuns(agents)
    }

    /// `subagent_transcript`: one agent's work, this turn's agents first.
    pub fn subagent_transcript_event(&self, agent_id: &str) -> AgentEvent {
        let items = self.subagent_manager.transcript_json(agent_id).or_else(|| {
            self.past_subagents
                .iter()
                .rev()
                .find(|(row, _)| row["id"] == agent_id)
                .map(|(_, items)| items.clone())
        });
        AgentEvent::SubagentTranscript {
            agent_id: agent_id.to_string(),
            items,
        }
    }

    /// Keeps the agents of the turn that ended (the manager starts afresh
    /// each turn), bounded to the most recent ones.
    fn archive_subagents(&mut self) {
        const MAX_PAST_SUBAGENTS: usize = 24;
        for row in self.subagent_manager.runs_json() {
            let id = row["id"].as_str().unwrap_or_default().to_string();
            let items = self
                .subagent_manager
                .transcript_json(&id)
                .unwrap_or_default();
            self.past_subagents
                .retain(|(known, _)| known["id"] != row["id"]);
            self.past_subagents.push((row, items));
        }
        let excess = self.past_subagents.len().saturating_sub(MAX_PAST_SUBAGENTS);
        self.past_subagents.drain(..excess);
    }

    /// Folds finished subagents' token usage into the parent's cumulative totals,
    /// priced at each child's own model. Subagent context is separate, so this
    /// only affects usage/cost accounting, never the context gauge.
    fn fold_subagent_usage(&mut self) -> Vec<AgentEvent> {
        let finished = self.subagent_manager.drain_finished_usage();
        if finished.is_empty() {
            return Vec::new();
        }
        for usage in finished {
            self.total_input_tokens += usage.input_tokens;
            self.total_cached_input_tokens += usage.cached_input_tokens;
            self.total_output_tokens += usage.output_tokens;
            self.total_cost += self.config.model_config(&usage.model).cost_for(
                usage.input_tokens,
                usage.cached_input_tokens,
                usage.output_tokens,
            );
        }
        vec![self.context_usage_event()]
    }

    fn start_turn(&mut self, message: String, images: Vec<String>) -> Vec<AgentEvent> {
        self.overflow_retried = false;
        self.session.append(EntryKind::User { content: message });
        if !images.is_empty() {
            self.session.append(EntryKind::UserImage { paths: images });
        }
        self.turn_started_at = Some(SystemTime::now());
        self.new_turn_tag();
        self.turn_goal_tokens = 0;
        let save_event = self.save_session_event();

        self.spawn_current_context_turn(save_event)
    }

    fn spawn_current_context_turn(&mut self, save_event: Vec<AgentEvent>) -> Vec<AgentEvent> {
        let keep_subagents = std::mem::take(&mut self.keep_subagents);
        if let Err(error) = self.ensure_provider_credentials() {
            self.subagent_manager
                .close_all_with_message("parent turn failed");
            self.requeue_unread_steers();
            let mut events = save_event;
            events.push(AgentEvent::Error(error));
            return events;
        }
        if !keep_subagents {
            self.archive_subagents();
            self.requeue_unread_steers();
            self.subagent_manager = SubagentManager::default();
        }
        let base_prompt = match self.config.system_prompt() {
            Ok(_) if self.chat => crate::chat::CHAT_SYSTEM_PROMPT.to_string(),
            Ok(prompt) => prompt,
            Err(error) => {
                let mut events = save_event;
                events.push(AgentEvent::Error(format!(
                    "failed to read prompt.txt: {error}"
                )));
                return events;
            }
        };
        // A chat directory holds no project skills; user skills still apply.
        let project_skills = self.project_trusted && !self.chat;
        let skills = match discover_skills(self.config.profile_dir(), &self.cwd, project_skills) {
            Ok(skills) => skills,
            Err(error) => {
                let mut events = save_event;
                events.push(AgentEvent::Error(format!(
                    "failed to discover skills: {error}"
                )));
                return events;
            }
        };
        let extra_read_roots = crate::prompt::skill_read_roots(&skills);
        // Chat directories live under the home directory, where discovery
        // would pick up unrelated AGENTS.md / CLAUDE.md files.
        let project_instructions = if self.config.include_project_instructions && !self.chat {
            match discover_project_instructions(&self.cwd) {
                Ok(instructions) => instructions,
                Err(error) => {
                    let mut events = save_event;
                    events.push(AgentEvent::Error(format!(
                        "failed to discover project instructions: {error}"
                    )));
                    return events;
                }
            }
        } else {
            Vec::new()
        };
        // Read the session from disk: Desktop or another engine may have
        // logged in or out since this engine last loaded auth.json.
        let signed_in = AuthStore::load_or_create(self.config.encrypt_secrets)
            .is_ok_and(|auth| auth.jucode_tokens().is_some());
        self.tool_state.set_web(Some(crate::web::WebTools {
            api_url: self.config.jucode_api_url.clone(),
            encrypt_secrets: self.config.encrypt_secrets,
            search_engine: self.config.web_search_engine.clone(),
            fetch_engine: self.config.web_fetch_engine.clone(),
            signed_in,
        }));
        if let Ok(image_model) = crate::config::read_image_model_at(self.config.path()) {
            self.config.image_model = image_model;
        }
        let model_headers = self.model_headers();
        let images = crate::images::ImageTools::from_config(
            &self.config,
            self.provider_api_key(),
            &model_headers,
        );
        let images_enabled = images.is_ok();
        self.tool_state.set_images(images);
        let prompt_tools = crate::tools::prompt_tool_names(
            &self.config.edit_tools,
            true,
            signed_in,
            images_enabled,
        );
        let mut system_prompt = build_system_prompt(
            &base_prompt,
            &PromptContext {
                date: current_utc_date(),
                cwd: self.cwd.clone(),
                tools: prompt_tools,
                edit_tools: self.config.edit_tools.clone(),
                project_instructions,
                skills,
                chat: self.chat,
            },
        );
        if let Some(sandbox) = self.tool_state.sandbox() {
            let note = sandbox.prompt(&self.cwd);
            if !note.is_empty() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(&note);
            }
        }
        if let Some(host) = &self.host {
            let extra = (host.prompt)();
            if !extra.trim().is_empty() {
                system_prompt.push_str("\n\n");
                system_prompt.push_str(extra.trim_end());
            }
        }
        if self.approval_mode.get() == ApprovalMode::Plan && !self.chat {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(crate::plan_mode::PROMPT_ADDENDUM);
        }

        // Desktop edits subagent_models in config.json while engines run. An
        // unreadable file keeps the list this engine already has.
        if let Ok(models) = crate::config::read_subagent_models_at(self.config.path()) {
            self.config.subagent_models = models;
        }
        // Same for the hand-set context windows (model settings in Desktop).
        if let Ok(overrides) = crate::config::read_context_window_overrides_at(self.config.path()) {
            self.config.context_window_overrides = overrides;
        }
        // And the group picked per model, which the window follows.
        if let Ok(groups) = crate::config::read_jucode_groups_at(self.config.path()) {
            self.config.jucode_groups = groups;
        }
        let (goal_tool_tx, goal_tool_rx) = mpsc::channel();
        self.goal_tool_receiver = Some(goal_tool_rx);
        let (approval_tx, approval_rx) = mpsc::channel();
        self.approval_receiver = Some(approval_rx);
        self.pending_approvals.clear();
        let Ok(mut client) = OpenAiClient::from_config(OpenAiClientConfig {
            model: self.config.model.clone(),
            provider: self.config.provider.clone(),
            protocol: self.config.protocol.clone(),
            reasoning_effort: self.effective_reasoning_effort(),
            models: self.config.models.clone(),
            subagent_models: self.config.subagent_models.clone(),
            system_prompt,
            prompt_cache_key: self.session.session_id().to_string(),
            mcp: self.mcp.clone(),
            base_url: self.config.base_url.clone(),
            max_output_tokens: self.config.current_model_config().max_output_tokens,
            api_key: self.provider_api_key().as_deref(),
            api_key_env: &self.config.api_key_env,
            retry_attempts: self.config.retry_attempts,
            connect_timeout: Duration::from_secs(self.config.connect_timeout_seconds),
            read_timeout: Duration::from_secs(self.config.read_timeout_seconds),
            goal_tool_tx: Some(goal_tool_tx),
            approval_tx: Some(approval_tx),
            approval_mode: self.approval_mode.clone(),
            safety_model: Some(self.config.safety().0).filter(|model| !model.trim().is_empty()),
            safety_reasoning_effort: self.config.safety().1,
            model_headers,
            edit_tools: self.config.edit_tools.clone(),
            extra_read_roots,
            tool_state: self.tool_state.clone(),
            host: self.host.clone(),
            subagent_manager: Some(self.subagent_manager.clone()),
            hooks: self.hooks.clone(),
        }) else {
            let mut events = save_event;
            events.push(AgentEvent::Error(
                "missing API key in auth.json or env".to_string(),
            ));
            return events;
        };

        let request_items = self.session.request_context_items();
        let (context_tokens, context_tokenizer) =
            self.session.context_token_usage(&self.config.model);
        let model_context_budget = target_context_budget(
            &self.config.current_model_config(),
            self.config.compaction_threshold_percent,
        );
        let forced = std::mem::take(&mut self.force_compaction);
        let compaction = if forced || should_auto_compact(context_tokens, model_context_budget) {
            self.session
                .plan_compaction(COMPACTION_KEEP_RECENT_TOKENS, &self.config.model)
        } else {
            None
        };
        // Mid-turn compaction: only for a turn that starts uncompacted with
        // earlier history to fold. A turn that compacts here (including the
        // continuation of a mid-turn compaction) runs on to the real limit,
        // so it never compacts again and again.
        if compaction.is_none()
            && model_context_budget > context_tokens
            && self
                .session
                .plan_compaction(COMPACTION_KEEP_RECENT_TOKENS, &self.config.model)
                .is_some()
        {
            client.set_context_headroom((model_context_budget - context_tokens) as u64);
        }
        let mut events = save_event;
        events.push(AgentEvent::ContextUsage {
            tokens: context_tokens as u64,
            tokenizer: context_tokenizer,
            cost: self.total_cost,
        });
        let compaction_client = if compaction.is_some() {
            match self.compaction_client() {
                Ok(client) => Some(client),
                Err(error) => {
                    events.push(AgentEvent::CompactionFailed(error));
                    None
                }
            }
        } else {
            None
        };
        let cwd = self.cwd.clone();
        let (tx, rx) = mpsc::channel();
        self.interrupt_flag = Arc::new(AtomicBool::new(false));
        let interrupt_flag = Arc::clone(&self.interrupt_flag);
        self.receiver = Some(rx);
        self.running = true;

        thread::spawn(move || {
            let input =
                if let (Some(plan), Some(compaction_client)) = (compaction, compaction_client) {
                    let _ = tx.send(WorkerEvent::CompactionStart);
                    match compaction_client.summarize_with_progress(
                        &plan.folded_text,
                        |output_tokens| {
                            tx.send(WorkerEvent::CompactionProgress { output_tokens })
                                .map_err(|error| error.to_string())
                        },
                    ) {
                        Ok(summary) => {
                            let _ = tx.send(WorkerEvent::CompactionDone {
                                summary: summary.clone(),
                                replaced_through: plan.replaced_through,
                            });
                            let mut items = vec![compaction_summary_item(&summary)];
                            items.extend(plan.kept_items);
                            items
                        }
                        Err(error) => {
                            let _ = tx.send(WorkerEvent::CompactionFailed(error));
                            request_items
                        }
                    }
                } else {
                    request_items
                };
            let result = client.run_turn_events(input, &cwd, |event| {
                if interrupt_flag.load(Ordering::SeqCst) {
                    return Err("interrupted".to_string());
                }
                let mapped = match event {
                    StreamEvent::CallStart => WorkerEvent::CallStart,
                    StreamEvent::Connected => WorkerEvent::Connected,
                    StreamEvent::ReasoningDelta(delta) => WorkerEvent::ReasoningDelta(delta),
                    StreamEvent::Delta(delta) => WorkerEvent::Delta(delta),
                    StreamEvent::Retrying {
                        attempt,
                        max_attempts,
                        reason,
                        delay_ms,
                    } => WorkerEvent::Retrying {
                        attempt,
                        max_attempts,
                        reason,
                        delay_ms,
                    },
                    StreamEvent::ResponseItem(item) => WorkerEvent::ResponseItem(item),
                    StreamEvent::Steered(message) => WorkerEvent::Steered(message),
                    // Only the plan is shown as it is written; every other
                    // call's arguments arrive whole with its ResponseItem.
                    StreamEvent::ToolArgumentsDelta {
                        call_id,
                        name,
                        delta,
                    } => {
                        if name != crate::plan_mode::TOOL_NAME {
                            return Ok(());
                        }
                        WorkerEvent::PlanDraft { call_id, delta }
                    }
                    StreamEvent::ToolStart { call_id, name } => {
                        WorkerEvent::ToolStart { call_id, name }
                    }
                    StreamEvent::ToolUpdate {
                        call_id,
                        name,
                        output,
                    } => WorkerEvent::ToolUpdate {
                        call_id,
                        name,
                        output,
                    },
                    StreamEvent::ToolOutput {
                        call_id,
                        name,
                        output,
                        model_output,
                        is_error,
                    } => WorkerEvent::ToolOutput {
                        call_id,
                        name,
                        output,
                        model_output,
                        is_error,
                    },
                    StreamEvent::Usage {
                        input_tokens,
                        cached_input_tokens,
                        output_tokens,
                        reasoning_tokens,
                    } => WorkerEvent::Usage {
                        input_tokens,
                        cached_input_tokens,
                        output_tokens,
                        reasoning_tokens,
                    },
                };
                tx.send(mapped).map_err(|error| error.to_string())
            });

            match result {
                Ok(()) => {
                    if !interrupt_flag.load(Ordering::SeqCst) {
                        let _ = tx.send(WorkerEvent::Done);
                    }
                }
                Err(error) => {
                    if !interrupt_flag.load(Ordering::SeqCst) && error != "interrupted" {
                        let _ = tx.send(WorkerEvent::Error(error));
                    }
                }
            }
        });

        events.extend([
            AgentEvent::AssistantStart,
            AgentEvent::Status("streaming".to_string()),
        ]);
        events
    }

    fn compact_command_events(&mut self) -> Vec<AgentEvent> {
        if self.running {
            return vec![AgentEvent::Error(
                "cannot compact while a response is running".to_string(),
            )];
        }

        let Some(plan) = self
            .session
            .plan_compaction(COMPACTION_KEEP_RECENT_TOKENS, &self.config.model)
        else {
            return vec![AgentEvent::Info(
                "nothing old enough to compact".to_string(),
            )];
        };
        if let Err(error) = self.ensure_provider_credentials() {
            return vec![AgentEvent::Error(error)];
        }
        let client = match self.compaction_client() {
            Ok(client) => client,
            Err(error) => return vec![AgentEvent::Error(error)],
        };

        self.turn_started_at = None;
        self.turn_goal_tokens = 0;
        self.goal_continuation_running = false;
        let (tx, rx) = mpsc::channel();
        self.interrupt_flag = Arc::new(AtomicBool::new(false));
        self.receiver = Some(rx);
        self.running = true;

        thread::spawn(move || {
            let _ = tx.send(WorkerEvent::CompactionStart);
            match client.summarize_with_progress(&plan.folded_text, |output_tokens| {
                tx.send(WorkerEvent::CompactionProgress { output_tokens })
                    .map_err(|error| error.to_string())
            }) {
                Ok(summary) => {
                    let _ = tx.send(WorkerEvent::CompactionDone {
                        summary,
                        replaced_through: plan.replaced_through,
                    });
                    let _ = tx.send(WorkerEvent::Done);
                }
                Err(error) => {
                    let _ = tx.send(WorkerEvent::CompactionFailed(error));
                    let _ = tx.send(WorkerEvent::Done);
                }
            }
        });

        vec![
            self.context_usage_event(),
            AgentEvent::Status("compacting".to_string()),
        ]
    }

    fn compaction_client(&self) -> Result<OpenAiClient, String> {
        let (model, reasoning_effort) = self.config.compact();
        OpenAiClient::from_config(OpenAiClientConfig {
            model,
            provider: self.config.provider.clone(),
            protocol: self.config.protocol.clone(),
            reasoning_effort,
            models: Vec::new(),
            subagent_models: Vec::new(),
            system_prompt: String::new(),
            prompt_cache_key: self.session.session_id().to_string(),
            mcp: McpManager::default(),
            base_url: self.config.base_url.clone(),
            max_output_tokens: self.config.compact_model_config().max_output_tokens,
            api_key: self.provider_api_key().as_deref(),
            api_key_env: &self.config.api_key_env,
            retry_attempts: self.config.retry_attempts,
            connect_timeout: Duration::from_secs(self.config.connect_timeout_seconds),
            read_timeout: Duration::from_secs(self.config.read_timeout_seconds),
            goal_tool_tx: None,
            approval_tx: None,
            approval_mode: self.approval_mode.clone(),
            safety_model: None,
            safety_reasoning_effort: String::new(),
            model_headers: self.model_headers(),
            // Summarization clients never expose or execute tools.
            edit_tools: Vec::new(),
            extra_read_roots: Vec::new(),
            tool_state: crate::tools::ToolState::default(),
            host: None,
            subagent_manager: None,
            hooks: Hooks::default(),
        })
    }

    fn resume_summary_client(&self) -> Result<OpenAiClient, String> {
        let model = self
            .config
            .models
            .iter()
            .find(|entry| entry.name == RESUME_SUMMARY_MODEL)
            .map(|entry| entry.name.clone())
            .unwrap_or_else(|| self.config.compact().0);
        let summary_model = model == RESUME_SUMMARY_MODEL;
        let max_output_tokens = self
            .config
            .models
            .iter()
            .find(|entry| entry.name == model)
            .map(|entry| entry.max_output_tokens)
            .unwrap_or_else(|| self.config.compact_model_config().max_output_tokens);
        OpenAiClient::from_config(OpenAiClientConfig {
            model,
            provider: self.config.provider.clone(),
            protocol: self.config.protocol.clone(),
            reasoning_effort: if summary_model {
                self.config.compact_reasoning_effort.clone()
            } else {
                self.config.compact().1
            },
            models: Vec::new(),
            subagent_models: Vec::new(),
            system_prompt: String::new(),
            prompt_cache_key: self.session.session_id().to_string(),
            mcp: McpManager::default(),
            base_url: self.config.base_url.clone(),
            max_output_tokens,
            api_key: self.provider_api_key().as_deref(),
            api_key_env: &self.config.api_key_env,
            retry_attempts: self.config.retry_attempts,
            connect_timeout: Duration::from_secs(self.config.connect_timeout_seconds),
            read_timeout: Duration::from_secs(self.config.read_timeout_seconds),
            goal_tool_tx: None,
            approval_tx: None,
            approval_mode: self.approval_mode.clone(),
            safety_model: None,
            safety_reasoning_effort: String::new(),
            model_headers: self.model_headers(),
            // Summarization clients never expose or execute tools.
            edit_tools: Vec::new(),
            extra_read_roots: Vec::new(),
            tool_state: crate::tools::ToolState::default(),
            host: None,
            subagent_manager: None,
            hooks: Hooks::default(),
        })
    }

    fn start_goal_continuation(&mut self) -> Vec<AgentEvent> {
        let Some(goal) = self.session.goal().cloned() else {
            return Vec::new();
        };
        let message = format!(
            "<goal_context>\nContinue working toward the active session goal.\n\nObjective: {}\n\nBefore doing more work, decide whether the objective is already satisfied. If all required work is done, call update_goal with status \"complete\" and stop. If the objective lacks a verifiable stopping condition, ask the user to clarify instead of continuing indefinitely. If progress cannot continue without user input or an external change, call update_goal with status \"blocked\".\n</goal_context>",
            goal.objective
        );
        self.session.append_goal_context(message);
        self.goal_continuation_running = true;
        let mut events = vec![AgentEvent::Info(format!(
            "Continuing goal: {}",
            goal.objective
        ))];
        events.extend(self.start_turn_from_existing_context());
        events
    }

    fn start_turn_from_existing_context(&mut self) -> Vec<AgentEvent> {
        self.turn_started_at = Some(SystemTime::now());
        self.new_turn_tag();
        self.turn_goal_tokens = 0;
        let save_event = self.save_session_event();

        self.spawn_current_context_turn(save_event)
    }

    fn finish_goal_turn(&mut self) -> Vec<AgentEvent> {
        let elapsed_seconds = self
            .turn_started_at
            .take()
            .and_then(|started| started.elapsed().ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let tokens = std::mem::take(&mut self.turn_goal_tokens);
        let mut events = Vec::new();
        if let Some(goal) = self.session.account_goal_usage(elapsed_seconds, tokens) {
            events.push(AgentEvent::Goal(Some(goal_view(&goal))));
            events.extend(self.save_session_event());
        }
        self.goal_continuation_running = false;
        events
    }

    /// Drain pending tool-approval requests from the worker (main agent and
    /// subagents share the channel). Requests are auto-approved when the tool
    /// is allowlisted or the current approval mode no longer gates it (a mode
    /// loosened mid-run applies immediately); the rest surface an
    /// ApprovalRequest and park their responder until the client decides.
    fn poll_approval_requests(&mut self) -> Vec<AgentEvent> {
        let Some(rx) = self.approval_receiver.take() else {
            return Vec::new();
        };
        let mut events = Vec::new();
        while let Ok(request) = rx.try_recv() {
            if self.approved_tools.contains(&request.name)
                || !self.approval_mode.get().requires_approval(&request.name)
            {
                let _ = request.response_tx.send(ApprovalDecision::allow_all());
                continue;
            }
            if !self.attended {
                let (decision, event) = self.defer_call(
                    request.call_id,
                    request.name,
                    request.summary,
                    request.arguments,
                    request.cwd,
                    request.subagent_id,
                );
                let _ = request.response_tx.send(decision);
                events.extend(event);
                continue;
            }
            let hunk_ids = request
                .hunks
                .as_ref()
                .map(|hunks| hunks.iter().map(|hunk| hunk.id.clone()).collect())
                .unwrap_or_default();
            events.push(AgentEvent::ApprovalRequest {
                call_id: request.call_id.clone(),
                name: request.name.clone(),
                summary: request.summary.clone(),
                subagent_id: request.subagent_id.clone(),
                hunks: request.hunks,
            });
            self.pending_approvals.insert(
                request.call_id,
                PendingApproval {
                    response_tx: request.response_tx,
                    name: request.name,
                    hunk_ids,
                    summary: request.summary,
                    arguments: request.arguments,
                    cwd: request.cwd,
                    subagent_id: request.subagent_id,
                },
            );
        }
        self.approval_receiver = Some(rx);
        events
    }

    /// Records a gated call as a deferred action, or reuses the decision (or
    /// the still-open action) of an identical call made earlier.
    fn defer_call(
        &mut self,
        call_id: String,
        name: String,
        summary: String,
        arguments: String,
        cwd: PathBuf,
        subagent_id: Option<String>,
    ) -> (ApprovalDecision, Option<AgentEvent>) {
        let digest = action_digest(&name, &arguments, &cwd);
        match self.action_decisions.get(&digest) {
            Some(true) => return (ApprovalDecision::allow_all(), None),
            Some(false) => return (ApprovalDecision::deny(), None),
            None => {}
        }
        if let Some(open) = self
            .deferred_actions
            .values()
            .find(|action| action.digest == digest)
        {
            return (ApprovalDecision::deferred(open.id.clone()), None);
        }
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default();
        let action = DeferredAction {
            id: format!("act-{created_at}-{}", &digest[..8]),
            session_id: self.session.session_id().to_string(),
            cwd,
            call_id,
            name,
            arguments,
            summary,
            subagent_id,
            digest,
            created_at,
        };
        self.deferred_actions
            .insert(action.id.clone(), action.clone());
        (
            ApprovalDecision::deferred(action.id.clone()),
            Some(AgentEvent::ActionDeferred(action)),
        )
    }

    /// Puts back deferred actions recorded by a host before a restart, so
    /// they can still be decided. Actions of other sessions are ignored.
    pub fn restore_deferred_actions(&mut self, actions: Vec<DeferredAction>) {
        for action in actions {
            if action.session_id == self.session.session_id() {
                self.deferred_actions.insert(action.id.clone(), action);
            }
        }
    }

    pub fn attended(&self) -> bool {
        self.attended
    }

    /// Marks whether a client is watching. Going unattended also converts
    /// calls already waiting on a prompt into deferred actions, so a client
    /// that disconnects mid-prompt never leaves the turn blocked.
    pub fn set_attended(&mut self, attended: bool) -> Vec<AgentEvent> {
        self.attended = attended;
        let mut events = Vec::new();
        if !attended {
            let parked = std::mem::take(&mut self.pending_approvals);
            for (call_id, pending) in parked {
                let (decision, event) = self.defer_call(
                    call_id,
                    pending.name,
                    pending.summary,
                    pending.arguments,
                    pending.cwd,
                    pending.subagent_id,
                );
                let _ = pending.response_tx.send(decision);
                events.extend(event);
            }
        }
        events.push(AgentEvent::Attended(attended));
        events
    }

    /// Decides a deferred action. An approved action runs with its original
    /// arguments on a background thread; either way the outcome is sent back
    /// to the session as a message, which starts (or queues) a turn.
    pub fn decide_action(&mut self, id: &str, allow: bool) -> Vec<AgentEvent> {
        match self.deferred_actions.get(id) {
            None => return vec![AgentEvent::Error(format!("no deferred action {id}"))],
            // The outcome is delivered as a message to the current session,
            // so an action from a session this engine has switched away from
            // must not land here.
            Some(action) if action.session_id != self.session.session_id() => {
                return vec![AgentEvent::Error(format!(
                    "deferred action {id} belongs to session {}",
                    action.session_id
                ))]
            }
            Some(_) => {}
        }
        let action = self
            .deferred_actions
            .remove(id)
            .expect("deferred action checked above");
        self.action_decisions.insert(action.digest.clone(), allow);
        if !allow {
            let mut events = vec![AgentEvent::ActionDecided {
                id: action.id.clone(),
                allow: false,
                output: None,
                is_error: false,
            }];
            events.extend(self.submit_user_message(decision_message(&action, None)));
            return events;
        }
        let tx = self.action_tx.clone();
        let mcp = self.mcp.clone();
        let hooks = self.hooks.clone();
        let tool_state = self.tool_state.clone();
        thread::spawn(move || {
            let result = if action.name.starts_with("mcp__") {
                match mcp.run_tool(&action.name, &action.arguments) {
                    Some((output, is_error)) => crate::tools::ToolExecutionResult {
                        model_output: crate::tools::project_model_output(
                            &action.name,
                            &output,
                            &action.cwd,
                        ),
                        output,
                        is_error,
                    },
                    None => {
                        let output =
                            json!({ "error": format!("unknown MCP tool: {}", action.name) })
                                .to_string();
                        crate::tools::ToolExecutionResult {
                            model_output: output.clone(),
                            output,
                            is_error: true,
                        }
                    }
                }
            } else {
                crate::tools::run_tool_with_events(
                    &action.name,
                    &action.arguments,
                    &action.cwd,
                    &[],
                    &tool_state,
                    |_| Ok(()),
                )
            };
            hooks.post_tool(&action.name, &result.output, &action.cwd);
            let _ = tx.send(ActionOutcome {
                action,
                output: result.model_output,
                is_error: result.is_error,
            });
        });
        Vec::new()
    }

    fn poll_action_outcomes(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        while let Ok(outcome) = self.action_rx.try_recv() {
            let action = outcome.action;
            events.push(AgentEvent::ActionDecided {
                id: action.id.clone(),
                allow: true,
                output: Some(outcome.output.clone()),
                is_error: outcome.is_error,
            });
            events.extend(self.submit_user_message(decision_message(
                &action,
                Some((&outcome.output, outcome.is_error)),
            )));
        }
        events
    }

    fn approval_mode_event(&self) -> AgentEvent {
        AgentEvent::ApprovalMode {
            mode: self.approval_mode.get().as_str().to_string(),
        }
    }

    /// Switch the session approval mode (also used by the serve
    /// `set_approval_mode` op and the `--approval-mode` flag).
    pub fn set_approval_mode(&mut self, mode: ApprovalMode) -> Vec<AgentEvent> {
        self.approval_mode.set(mode);
        // Calls parked for a decision the new mode no longer needs run now.
        let freed: Vec<String> = self
            .pending_approvals
            .iter()
            .filter(|(_, pending)| !mode.requires_approval(&pending.name))
            .map(|(call, _)| call.clone())
            .collect();
        for call in freed {
            if let Some(pending) = self.pending_approvals.remove(&call) {
                let _ = pending.response_tx.send(ApprovalDecision::allow_all());
            }
        }
        vec![
            AgentEvent::Status(format!("approval mode: {}", mode.as_str())),
            self.approval_mode_event(),
        ]
    }

    /// `/sandbox [mode]` — show the sandbox, or switch its mode for this
    /// session (config.json `sandbox` sets the default).
    fn sandbox_command_events(&mut self, arg: &str) -> Vec<AgentEvent> {
        let mut policy = self
            .tool_state
            .sandbox()
            .unwrap_or_else(|| self.config.sandbox.clone());
        if !arg.is_empty() {
            match crate::sandbox::SandboxMode::parse(arg) {
                Ok(mode) => {
                    policy.mode = mode;
                    if let Err(error) = policy.check_available() {
                        return vec![AgentEvent::Error(format!("sandbox unavailable: {error}"))];
                    }
                    self.tool_state.set_sandbox(Some(policy.clone()));
                }
                Err(error) => {
                    return vec![AgentEvent::Error(format!(
                        "usage: /sandbox [read-only|workspace-write|full-access] ({error})"
                    ))]
                }
            }
        }
        let list = |dirs: &[PathBuf]| {
            if dirs.is_empty() {
                "none".to_string()
            } else {
                dirs.iter()
                    .map(|dir| dir.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        let rules = policy
            .rules
            .iter()
            .map(|rule| {
                format!(
                    "{} → {}",
                    rule.prefix,
                    format!("{:?}", rule.action).to_lowercase()
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        vec![AgentEvent::Info(format!(
            "sandbox: {}\n\
             read-only       - commands cannot write anything\n\
             workspace-write - the working directory, temp and package caches are writable; .git, .jucode and .agents stay read-only\n\
             full-access     - no sandbox\n\
             network: {} · read-write dirs: {} · read-only dirs: {}\n\
             command rules: {}\n\
             Inside the sandbox commands need no approval (except in manual mode); a command that must leave it asks per the approval mode. Set defaults in config.json (sandbox, sandbox_network, sandbox_directories, command_rules).",
            policy.mode.as_str(),
            if policy.network { "on" } else { "off" },
            list(&policy.writable_dirs),
            list(&policy.readable_dirs),
            if rules.is_empty() { "none".to_string() } else { rules }
        ))]
    }

    /// `/permissions [mode]` — show the current mode, or switch it for this session.
    fn permissions_command_events(&mut self, arg: &str) -> Vec<AgentEvent> {
        if arg.is_empty() {
            return vec![
                AgentEvent::Info(format!(
                    "approval mode: {}\n\
                     manual      - file edits and shell commands ask for approval (default)\n\
                     auto-edit   - file edits run freely; shell commands still ask\n\
                     auto        - a safety model auto-approves safe shell commands; the rest ask\n\
                     full-access - everything runs without asking\n\
                     Switch with /permissions <mode>; a change applies at once, to the\n\
                     running turn too.",
                    self.approval_mode.get().as_str()
                )),
                self.approval_mode_event(),
            ];
        }
        match ApprovalMode::parse(arg) {
            Ok(mode) => self.set_approval_mode(mode),
            Err(error) => vec![AgentEvent::Error(format!(
                "usage: /permissions [manual|plan|auto-edit|auto|full-access] ({error})"
            ))],
        }
    }

    /// `/effort [level]` — with no argument cycle to the next effort the
    /// current model supports; with one, set it explicitly.
    /// `/subagents`: list, add (or re-describe), or remove the models
    /// `spawn_agent` may choose. The main model is always available.
    fn subagents_command_events(&mut self, args: &str) -> Vec<AgentEvent> {
        const USAGE: &str = "usage: /subagents [add <model> <when to use>|remove <model>]";
        let (action, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        let (model, description) = rest
            .trim()
            .split_once(char::is_whitespace)
            .map_or((rest.trim(), ""), |(model, description)| {
                (model, description.trim())
            });
        match (action, model) {
            ("", _) => {
                self.reload_model_list();
                vec![AgentEvent::Info(self.subagent_models_lines())]
            }
            ("add", model) if !model.is_empty() => {
                self.reload_model_list();
                if !self.config.models.iter().any(|entry| entry.name == model) {
                    return vec![AgentEvent::Error(format!(
                        "unknown model: {model} (see /model for configured models)"
                    ))];
                }
                let entry = crate::config::SubagentModel {
                    name: model.to_string(),
                    description: description.to_string(),
                };
                let result = self.change_config(|config| {
                    match config
                        .subagent_models
                        .iter_mut()
                        .find(|existing| existing.name == entry.name)
                    {
                        Some(existing) => *existing = entry.clone(),
                        None => config.subagent_models.push(entry.clone()),
                    }
                });
                match result {
                    Ok(()) => vec![AgentEvent::Status(format!("subagents may use {model}"))],
                    Err(error) => {
                        vec![AgentEvent::Error(format!("failed to save config: {error}"))]
                    }
                }
            }
            ("remove", model) if !model.is_empty() && description.is_empty() => {
                if !self
                    .config
                    .subagent_models
                    .iter()
                    .any(|entry| entry.name == model)
                {
                    return vec![AgentEvent::Error(format!(
                        "{model} is not a subagent model"
                    ))];
                }
                match self.change_config(|config| {
                    config.subagent_models.retain(|entry| entry.name != model)
                }) {
                    Ok(()) => vec![AgentEvent::Status(format!(
                        "subagents no longer use {model}"
                    ))],
                    Err(error) => {
                        vec![AgentEvent::Error(format!("failed to save config: {error}"))]
                    }
                }
            }
            _ => vec![AgentEvent::Error(USAGE.to_string())],
        }
    }

    fn subagent_models_lines(&self) -> String {
        let mut lines = vec![format!(
            "subagent models (the main model {} is always available):",
            self.config.model
        )];
        if self.config.subagent_models.is_empty() {
            lines.push("  none — subagents run on the main model".to_string());
        }
        for entry in &self.config.subagent_models {
            let configured = self.config.models.iter().any(|m| m.name == entry.name);
            let mut line = format!("  {}", entry.name);
            if !entry.description.is_empty() {
                line.push_str(&format!(" — {}", entry.description));
            }
            if !configured {
                line.push_str(" (not in the current model list; ignored)");
            }
            lines.push(line);
        }
        lines.push(
            "usage: /subagents add <model> <when to use> | /subagents remove <model>".to_string(),
        );
        lines.join("\n")
    }

    fn effort_command_events(&mut self, arg: &str) -> Vec<AgentEvent> {
        let model = self.config.model.clone();
        let efforts = self.reasoning_efforts_for_model(&model);
        if efforts.is_empty() {
            return vec![AgentEvent::Error(format!(
                "{model} does not support reasoning effort"
            ))];
        }
        let next = if arg.is_empty() {
            let index = efforts
                .iter()
                .position(|effort| effort == &self.config.reasoning_effort)
                .map(|index| (index + 1) % efforts.len())
                .unwrap_or(0);
            efforts[index].clone()
        } else {
            arg.to_string()
        };
        self.set_model_config(model, next)
    }

    /// Forward the client's allow/deny decision to the parked tool call. With
    /// `always`, the tool is added to the per-session allowlist. `hunks`
    /// restricts an allow to a subset of an edit call's hunk ids (also used by
    /// the serve `approve` op). On a validation error the request stays
    /// pending so the client can retry with a corrected command.
    pub fn approve(
        &mut self,
        call_id: &str,
        allow: bool,
        always: bool,
        hunks: Option<Vec<String>>,
    ) -> Vec<AgentEvent> {
        match resolve_approval_decision(
            &mut self.pending_approvals,
            &mut self.approved_tools,
            call_id,
            allow,
            always,
            hunks,
        ) {
            Ok(()) => Vec::new(),
            Err(error) => vec![AgentEvent::Error(error)],
        }
    }

    fn poll_goal_tool_requests(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let Some(rx) = self.goal_tool_receiver.take() else {
            return events;
        };
        while let Ok(request) = rx.try_recv() {
            let (response, event) =
                self.handle_goal_tool_request(&request.name, &request.arguments);
            let _ = request.response_tx.send(response);
            if let Some(event) = event {
                events.push(event);
            }
            events.extend(self.save_session_event());
        }
        self.goal_tool_receiver = Some(rx);
        events
    }

    fn handle_goal_tool_request(
        &mut self,
        name: &str,
        arguments: &str,
    ) -> (ToolGoalResponse, Option<AgentEvent>) {
        let args = serde_json::from_str::<Value>(arguments)
            .unwrap_or_else(|error| json!({ "error": format!("invalid JSON arguments: {error}") }));
        if name == "update_plan" {
            return self.handle_update_plan(&args);
        }
        if name == crate::plan_mode::TOOL_NAME {
            return self.handle_propose_plan(&args);
        }
        let result = match name {
            "get_goal" => Ok(self.session.goal().cloned()),
            "create_goal" => {
                let objective = args
                    .get("objective")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let token_budget = args.get("token_budget").and_then(Value::as_u64);
                self.session.create_goal(objective, token_budget).map(Some)
            }
            "update_goal" => {
                let status = match args.get("status").and_then(Value::as_str) {
                    Some("complete") => Ok(ThreadGoalStatus::Complete),
                    Some("blocked") => Ok(ThreadGoalStatus::Blocked),
                    Some(_) => {
                        Err("update_goal can only set status to complete or blocked".to_string())
                    }
                    None => Err("update_goal requires status".to_string()),
                };
                status.and_then(|status| self.session.set_goal_status(status).map(Some))
            }
            _ => Err(format!("unknown goal tool: {name}")),
        };
        match result {
            Ok(goal) => {
                let output = json!({ "goal": goal.as_ref().map(goal_tool_json) }).to_string();
                (
                    ToolGoalResponse {
                        output,
                        is_error: false,
                    },
                    Some(AgentEvent::Goal(goal.as_ref().map(goal_view))),
                )
            }
            Err(error) => {
                let output = json!({ "error": error }).to_string();
                (
                    ToolGoalResponse {
                        output,
                        is_error: true,
                    },
                    None,
                )
            }
        }
    }

    /// `propose_plan` (plan mode): records the plan as pending and shows it;
    /// the turn ends there and the user answers with `approve_plan`.
    fn handle_propose_plan(&mut self, args: &Value) -> (ToolGoalResponse, Option<AgentEvent>) {
        self.plan_draft = None;
        let field = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_string()
        };
        let (title, markdown) = (field("title"), field("plan"));
        if self.approval_mode.get() != ApprovalMode::Plan {
            let output =
                json!({ "error": "propose_plan is only available in plan mode" }).to_string();
            return (
                ToolGoalResponse {
                    output,
                    is_error: true,
                },
                None,
            );
        }
        if title.is_empty() || markdown.is_empty() {
            let output =
                json!({ "error": "propose_plan requires a title and the plan" }).to_string();
            return (
                ToolGoalResponse {
                    output,
                    is_error: true,
                },
                None,
            );
        }
        let id = self.next_plan_id();
        let event = self.record_plan(&id, &title, &markdown, "pending");
        let output = json!({
            "status": "proposed",
            "id": id,
            "note": "The plan is shown to the user. Stop now and wait for their approval or feedback."
        })
        .to_string();
        (
            ToolGoalResponse {
                output,
                is_error: false,
            },
            Some(event),
        )
    }

    fn next_plan_id(&self) -> String {
        let mut bytes = [0u8; 8];
        let _ = getrandom::getrandom(&mut bytes);
        format!(
            "plan-{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    }

    fn record_plan(&mut self, id: &str, title: &str, markdown: &str, status: &str) -> AgentEvent {
        self.session.append(EntryKind::ProposedPlan {
            id: id.to_string(),
            title: title.to_string(),
            markdown: markdown.to_string(),
            status: status.to_string(),
        });
        AgentEvent::ProposedPlan {
            id: id.to_string(),
            title: title.to_string(),
            markdown: markdown.to_string(),
            status: status.to_string(),
        }
    }

    /// The latest record of plan `id` on this branch: (title, markdown, status).
    fn find_plan(&self, id: &str) -> Option<(String, String, String)> {
        self.session
            .branch()
            .into_iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                EntryKind::ProposedPlan {
                    id: known,
                    title,
                    markdown,
                    status,
                } if known == id => Some((title.clone(), markdown.clone(), status.clone())),
                _ => None,
            })
    }

    /// `approve_plan`: approve a pending plan and run it in `mode`, or ask
    /// for a revision with `feedback` (plan mode stays on).
    pub fn approve_plan(
        &mut self,
        id: &str,
        approve: bool,
        mode: Option<ApprovalMode>,
        feedback: &str,
    ) -> Vec<AgentEvent> {
        let Some((title, markdown, status)) = self.find_plan(id) else {
            return vec![AgentEvent::Error(format!("unknown plan: {id}"))];
        };
        if status == "approved" {
            return vec![AgentEvent::Error(
                "this plan was already approved".to_string(),
            )];
        }
        let mut events = Vec::new();
        if approve {
            let mode = mode
                .filter(|mode| *mode != ApprovalMode::Plan)
                .unwrap_or(ApprovalMode::AutoEdit);
            events.push(self.record_plan(id, &title, &markdown, "approved"));
            events.extend(self.set_approval_mode(mode));
            let mut message = format!(
                "The user approved the plan \"{title}\". Implement it now, step by step. Keep the checklist current with update_plan as you go, and verify the result as the plan describes."
            );
            if !feedback.trim().is_empty() {
                message.push_str("\n\nTheir notes: ");
                message.push_str(feedback.trim());
            }
            events.extend(self.submit_user_message(message));
        } else {
            if feedback.trim().is_empty() {
                return vec![AgentEvent::Error(
                    "revising a plan needs feedback".to_string(),
                )];
            }
            events.push(self.record_plan(id, &title, &markdown, "revising"));
            if self.approval_mode.get() != ApprovalMode::Plan {
                events.extend(self.set_approval_mode(ApprovalMode::Plan));
            }
            events.extend(self.submit_user_message(format!(
                "Revise the plan \"{title}\" with this feedback, then propose the complete revised plan with propose_plan:\n\n{}",
                feedback.trim()
            )));
        }
        events.extend(self.save_session_event());
        events
    }

    /// `/plan <id> approve [mode] [notes]` or `/plan <id> revise <feedback>`:
    /// the text twin of the `approve_plan` op, for the TUI. Bare `/plan`
    /// shows the latest plan again while it waits for the user.
    fn plan_command_events(&mut self, args: &str) -> Vec<AgentEvent> {
        const USAGE: &str =
            "usage: /plan <plan-id> approve [mode] [notes] | /plan <plan-id> revise <feedback>";
        if args.is_empty() {
            let latest =
                self.session
                    .branch()
                    .into_iter()
                    .rev()
                    .find_map(|entry| match &entry.kind {
                        EntryKind::ProposedPlan { id, .. } => Some(id.clone()),
                        _ => None,
                    });
            return match latest.and_then(|id| self.find_plan(&id).map(|plan| (id, plan))) {
                Some((id, (title, markdown, status))) if status == "pending" => {
                    vec![AgentEvent::ProposedPlan {
                        id,
                        title,
                        markdown,
                        status,
                    }]
                }
                _ => vec![AgentEvent::Info(
                    "no plan is waiting for approval".to_string(),
                )],
            };
        }
        let (id, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
        let (decision, rest) = rest
            .trim()
            .split_once(char::is_whitespace)
            .unwrap_or((rest.trim(), ""));
        match (id, decision) {
            ("", _) => vec![AgentEvent::Error(USAGE.to_string())],
            (id, "approve") => {
                let (first, notes) = rest
                    .trim()
                    .split_once(char::is_whitespace)
                    .unwrap_or((rest.trim(), ""));
                match ApprovalMode::parse(first) {
                    Ok(mode) => self.approve_plan(id, true, Some(mode), notes),
                    Err(_) => self.approve_plan(id, true, None, rest),
                }
            }
            (id, "revise") => self.approve_plan(id, false, None, rest),
            _ => vec![AgentEvent::Error(USAGE.to_string())],
        }
    }

    fn handle_update_plan(&mut self, args: &Value) -> (ToolGoalResponse, Option<AgentEvent>) {
        let Some(items) = args.get("plan").and_then(Value::as_array) else {
            let output = json!({ "error": "update_plan requires a plan array" }).to_string();
            return (
                ToolGoalResponse {
                    output,
                    is_error: true,
                },
                None,
            );
        };
        let mut plan = Vec::new();
        for item in items {
            let step = item.get("step").and_then(Value::as_str).unwrap_or_default();
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("pending");
            if step.is_empty() {
                continue;
            }
            let status = match status {
                "in_progress" | "completed" => status,
                _ => "pending",
            };
            plan.push(PlanItem {
                step: step.to_string(),
                status: status.to_string(),
            });
        }
        self.plan = plan;
        let output = json!({ "ok": true, "steps": self.plan.len() }).to_string();
        (
            ToolGoalResponse {
                output,
                is_error: false,
            },
            Some(AgentEvent::Plan(self.plan.clone())),
        )
    }

    fn should_continue_goal(&self) -> bool {
        if self.goal_continuation_running || self.running || !self.queued.is_empty() {
            return false;
        }
        self.session
            .goal()
            .is_some_and(|goal| goal.status == ThreadGoalStatus::Active)
    }

    fn should_generate_resume_summary(&self) -> bool {
        if self.running || self.resume_summary_running || !self.queued.is_empty() {
            return false;
        }
        let idle_for = now_secs().saturating_sub(self.session.updated_at());
        if idle_for < RESUME_SUMMARY_IDLE_SECONDS {
            return false;
        }
        self.session
            .resume_summary_updated_at()
            .is_none_or(|updated| updated < self.session.updated_at())
    }

    fn start_resume_summary(&mut self) {
        let input = self.session.resume_summary_input();
        if input.trim().is_empty() {
            self.session.set_resume_summary(
                None,
                self.session
                    .goal()
                    .map(|goal| normalize_resume_status(goal.status)),
                now_secs(),
            );
            return;
        }
        let status = self
            .session
            .goal()
            .map(|goal| normalize_resume_status(goal.status))
            .unwrap_or(ThreadGoalStatus::Active);
        if self.ensure_provider_credentials().is_err() {
            return;
        }
        let client = match self.resume_summary_client() {
            Ok(client) => client,
            Err(_) => return,
        };
        let (tx, rx) = mpsc::channel();
        self.resume_summary_receiver = Some(rx);
        self.resume_summary_running = true;
        thread::spawn(move || {
            let system = "Summarize the current latest task in one sentence. Focus only on the most recent work. Start with either 'Working:' or 'Completed:'. Mention the concrete task or result, not background context. Ignore older finished tasks unless they matter to the current state. Keep it concise. Output only that one sentence.";
            let user = format!("Recent session activity:\n\n{input}");
            let result = client.summarize_text(system, &user, |_| Ok(()));
            let _ = match result {
                Ok(summary) => tx.send(WorkerEvent::ResumeSummaryDone {
                    summary,
                    status,
                    summarized_at: now_secs(),
                }),
                Err(error) => tx.send(WorkerEvent::ResumeSummaryFailed(error)),
            };
        });
    }

    fn save_session_event(&mut self) -> Vec<AgentEvent> {
        match self.session.save_for_cwd(&self.profile_dir, &self.cwd) {
            Ok(()) => Vec::new(),
            Err(error) => {
                crate::log_error!(
                    "session",
                    "failed to save session",
                    session = self.session.session_id(),
                    error = error.to_string()
                );
                vec![AgentEvent::Error(format!(
                    "failed to save session: {error}"
                ))]
            }
        }
    }

    fn context_usage_event(&self) -> AgentEvent {
        let (tokens, tokenizer) = self.session.context_token_usage(&self.config.model);
        AgentEvent::ContextUsage {
            tokens: tokens as u64,
            tokenizer,
            cost: self.total_cost,
        }
    }

    fn new_session_events(&mut self) -> Vec<AgentEvent> {
        if self.running {
            return vec![AgentEvent::Error(
                "cannot start a new session while a response is running".to_string(),
            )];
        }
        self.queued.clear();
        self.receiver = None;
        // Any in-flight resume summary belongs to the old session; drop it.
        self.resume_summary_receiver = None;
        self.resume_summary_running = false;
        self.session = SessionStore::new();
        // Release the old session's lock and hold the new one.
        self.session_lock = None;
        self.session_lock =
            SessionLock::acquire(&self.profile_dir, &self.cwd, self.session.session_id()).ok();
        let session_id = self.session.session_id().to_string();
        let save_event = self.save_session_event();
        vec![
            AgentEvent::Transcript(self.session.transcript_items()),
            AgentEvent::PendingMessages(Vec::new()),
            self.model_status_event(),
            self.context_usage_event(),
            AgentEvent::Status(format!("new session {session_id}")),
        ]
        .into_iter()
        .chain(save_event)
        .collect()
    }

    /// `/login <provider>` — run the omp catalog's declared flow for one
    /// upstream provider (browser OAuth, device code, or api-key guidance).
    fn login_events(&mut self, arg: &str) -> Vec<AgentEvent> {
        if self.running {
            return vec![AgentEvent::Error(
                "cannot login while a response is running".to_string(),
            )];
        }
        let mut parts = arg.split_whitespace();
        let first = parts.next().unwrap_or_default();
        if first.is_empty() {
            return vec![self.login_picker_event()];
        }
        if first == "list" {
            return self.omp_login_list_events();
        }
        if !first.is_empty()
            && first != "jucode"
            && llm_provider_kit::omp::catalog()
                .auth_provider(first)
                .is_some()
        {
            return match parts.next() {
                Some(key) => self.omp_api_key_events(first, key),
                None => self.omp_login_events(first),
            };
        }
        let web_url = if first == "jucode" {
            self.config.jucode_web_url.clone()
        } else {
            first.to_string()
        };
        let api_url = parts.next().map(str::to_string).unwrap_or_else(|| {
            if first.is_empty() || first == "jucode" {
                self.config.jucode_api_url.clone()
            } else {
                web_url.clone()
            }
        });
        if self.login_receiver.is_some() || self.omp_login_receiver.is_some() {
            return vec![AgentEvent::Error(
                "a login is already in progress".to_string(),
            )];
        }
        let (tx, rx) = mpsc::channel();
        let web = web_url.clone();
        let api = api_url.clone();
        thread::spawn(move || {
            let _ = tx.send(oauth::login(&web, &api));
        });
        self.login_receiver = Some(rx);
        vec![AgentEvent::Info(
            "Opening your browser to sign in to JuCode. Complete the authorization there — waiting for it to finish (up to 5 min)…".to_string(),
        )]
    }

    /// Starts the declared login flow for an omp provider on a worker
    /// thread; interim auth notices (device codes, browser URLs) arrive as
    /// `OmpLoginEvent::Notice` on the receiver.
    fn omp_login_events(&mut self, provider: &str) -> Vec<AgentEvent> {
        if self.login_receiver.is_some() || self.omp_login_receiver.is_some() {
            return vec![AgentEvent::Error(
                "a login is already in progress".to_string(),
            )];
        }
        let catalog = llm_provider_kit::omp::catalog();
        let name = catalog
            .auth_provider(provider)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| provider.to_string());
        let provider_id = provider.to_string();
        let profile_dir = self.profile_dir.clone();
        // Manual/native callbacks take the pasted-code path: the worker blocks
        // on this channel until `/login-paste` feeds it a redirect URL or code.
        let manual = matches!(
            catalog
                .auth_provider(&provider_id)
                .and_then(|p| p.login.as_ref()),
            Some(llm_provider_kit::omp::LoginRule::OauthCode(rule))
                if rule.callback.manual_only || rule.callback.native_scheme
        );
        let (code_tx, code_rx) = mpsc::channel();
        self.omp_login_code_tx = manual.then_some(code_tx);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let on_auth = |url: &str, instructions: Option<&str>| {
                let text = match instructions {
                    Some(instructions) => format!("{instructions}\n{url}"),
                    None => url.to_string(),
                };
                let _ = tx.send(OmpLoginEvent::Notice(text));
            };
            let on_notice = |text: &str| {
                let _ = tx.send(OmpLoginEvent::Notice(text.to_string()));
            };
            let result = provider_auth::login(
                &provider_id,
                &LoginContext {
                    profile_dir: &profile_dir,
                    client_name: CLIENT_NAME,
                },
                &on_auth,
                &on_notice,
                manual.then_some(code_rx),
            );
            let _ = tx.send(OmpLoginEvent::Done {
                provider: provider_id,
                result,
            });
        });
        self.omp_login_receiver = Some(rx);
        let mut events = vec![AgentEvent::Info(format!("starting {name} login…"))];
        if manual {
            events.push(AgentEvent::LoginPastePrompt { provider: name });
        }
        events
    }

    /// `/login-paste <redirect-url-or-code>` — completes a manual-callback
    /// provider login (e.g. zai-coding-plan's zcode:// redirect).
    fn login_paste_events(&mut self, arg: &str) -> Vec<AgentEvent> {
        let text = arg.trim();
        if text.is_empty() {
            return vec![AgentEvent::Error(
                "usage: /login-paste <redirect-url-or-code>".to_string(),
            )];
        }
        match &self.omp_login_code_tx {
            Some(tx) if tx.send(text.to_string()).is_ok() => {
                vec![AgentEvent::Info(
                    "code received — finishing login…".to_string(),
                )]
            }
            _ => vec![AgentEvent::Error(
                "no login is waiting for a pasted code".to_string(),
            )],
        }
    }

    /// Rows for the interactive provider picker emitted by bare `/login`.
    /// JuCode's own gateway stays first; the rest follow catalog order with
    /// their flow kind and sign-in state.
    fn login_picker_event(&self) -> AgentEvent {
        let catalog = llm_provider_kit::omp::catalog();
        let mut rows = vec![LoginProviderView {
            id: "jucode".to_string(),
            label: "JuCode".to_string(),
            detail: if self.auth.jucode_tokens().is_some() {
                "oauth · signed in".to_string()
            } else {
                "oauth".to_string()
            },
            active: self.config.provider == "jucode",
            wants_key: false,
        }];
        for provider in catalog.auth_providers() {
            let Some(login) = &provider.login else {
                continue;
            };
            if !catalog.provider_usable(&provider.id) {
                continue;
            }
            let store_id = provider.store_as.as_deref().unwrap_or(&provider.id);
            let signed_in = self.auth.oauth_credential(store_id).is_some()
                || self.auth.key_for(&provider.id).is_some();
            let Some((kind, wants_key)) = login_kind(login) else {
                continue;
            };
            let detail = if signed_in {
                format!("{kind} · signed in")
            } else {
                kind.to_string()
            };
            rows.push(LoginProviderView {
                id: provider.id.clone(),
                label: provider.name.clone(),
                detail,
                active: self.config.provider == provider.id,
                wants_key,
            });
        }
        AgentEvent::LoginPicker(rows)
    }

    /// `/login list` — the catalog's login-capable providers grouped by flow.
    fn omp_login_list_events(&self) -> Vec<AgentEvent> {
        let catalog = llm_provider_kit::omp::catalog();
        let mut lines = vec![format!(
            "provider logins (omp catalog {}) — /login <id>:",
            catalog.omp_version
        )];
        for provider in catalog.auth_providers() {
            let Some(login) = &provider.login else {
                continue;
            };
            let Some((kind, _)) = login_kind(login) else {
                continue;
            };
            if !catalog.provider_usable(&provider.id) {
                continue;
            }
            lines.push(format!("  {} — {} ({kind})", provider.id, provider.name));
        }
        vec![AgentEvent::Info(lines.join("\n"))]
    }

    fn apply_omp_login_result(
        &mut self,
        provider_id: String,
        result: Result<LoginOutcome, String>,
    ) -> Vec<AgentEvent> {
        let catalog = llm_provider_kit::omp::catalog();
        let name = catalog
            .auth_provider(&provider_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| provider_id.clone());
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => return vec![AgentEvent::Error(format!("{name} login failed: {error}"))],
        };
        let LoginOutcome::Credentials(credential) = outcome else {
            let LoginOutcome::ApiKeyInstructions {
                auth_url,
                instructions,
                prompt,
            } = outcome
            else {
                unreachable!()
            };
            let mut lines = Vec::new();
            if let Some(instructions) = instructions {
                lines.push(instructions);
            }
            if let Some(url) = auth_url {
                lines.push(format!("create a key: {url}"));
            }
            if let Some(prompt) = prompt {
                lines.push(prompt);
            }
            lines.push(format!(
                "then store it as \"{provider_id}\" under \"providers\" in ~/.jucode/auth.json"
            ));
            return vec![AgentEvent::Info(lines.join("\n"))];
        };
        let store_id = catalog
            .auth_provider(&provider_id)
            .and_then(|p| p.store_as.clone())
            .unwrap_or_else(|| provider_id.clone());
        self.auth.set_oauth_credential(&store_id, *credential);
        self.adopt_provider(&provider_id);
        match self.auth.save().and_then(|_| self.config.save()) {
            Ok(()) => vec![
                AgentEvent::Info(format!(
                    "{name} connected; provider switched to {provider_id}"
                )),
                self.model_status_event(),
            ],
            Err(error) => vec![AgentEvent::Error(format!("failed to save login: {error}"))],
        }
    }

    /// `/login <provider> <api-key>` — BYOK shortcut: stores the pasted key
    /// under `providers.<id>` and switches the active provider, mirroring
    /// what a successful OAuth login does.
    fn omp_api_key_events(&mut self, provider: &str, key: &str) -> Vec<AgentEvent> {
        let catalog = llm_provider_kit::omp::catalog();
        let Some(auth_provider) = catalog.auth_provider(provider) else {
            return vec![AgentEvent::Error(format!(
                "unknown provider \"{provider}\""
            ))];
        };
        let mut key = key.trim();
        if matches!(
            &auth_provider.login,
            Some(llm_provider_kit::omp::LoginRule::ApiKey(rule))
                if rule.normalize.as_deref() == Some("strip-bearer")
        ) {
            key = key
                .strip_prefix("Bearer ")
                .or_else(|| key.strip_prefix("bearer "))
                .unwrap_or(key)
                .trim();
        }
        if key.is_empty() {
            return vec![AgentEvent::Error("empty api key".to_string())];
        }
        let name = auth_provider.name.clone();
        self.auth.set_key(provider, key.to_string());
        self.adopt_provider(provider);
        match self.auth.save().and_then(|_| self.config.save()) {
            Ok(()) => vec![
                AgentEvent::Info(format!(
                    "{name} api key saved; provider switched to {provider}"
                )),
                self.model_status_event(),
            ],
            Err(error) => vec![AgentEvent::Error(format!("failed to save login: {error}"))],
        }
    }

    /// Point the session config at `provider_id` after a successful login:
    /// catalog model table, a sensible default model, its base URL, and a
    /// valid reasoning effort for that model.
    fn adopt_provider(&mut self, provider_id: &str) {
        let catalog = llm_provider_kit::omp::catalog();
        self.config.provider = provider_id.to_string();
        self.config.models = models_for_provider(provider_id);
        let models = catalog.models(provider_id);
        let supported = catalog.supported_models(provider_id, models);
        let pick = catalog
            .default_model(provider_id)
            .filter(|default| supported.iter().any(|m| m.id == *default))
            .map(str::to_string)
            .or_else(|| supported.first().map(|m| m.id.clone()));
        if let Some(model) = pick {
            self.config.model = model.clone();
            if let Some(base_url) = catalog.base_url_for(provider_id, &model) {
                self.config.base_url = base_url.to_string();
            }
            let efforts = self.reasoning_efforts_for_model(&model);
            if !efforts
                .iter()
                .any(|effort| effort == &self.config.reasoning_effort)
            {
                self.config.reasoning_effort = self.default_reasoning_effort_for_model(&model);
            }
        } else if let Some(base_url) = catalog.default_base_url(provider_id) {
            self.config.base_url = base_url.to_string();
        }
    }

    fn apply_login_result(&mut self, result: OAuthLoginResult) -> Vec<AgentEvent> {
        // The gateway lists every model the account can reach (across all its
        // groups); the user picks which to show. Keep an earlier pick, else
        // start from the recommended set.
        let available: Vec<ModelConfig> = result.models.iter().map(jucode_model_config).collect();
        let kept: Vec<ModelConfig> = self
            .config
            .jucode_models
            .iter()
            .filter_map(|m| available.iter().find(|a| a.name == m.name).cloned())
            .collect();
        let visible = if kept.is_empty() {
            default_jucode_models(&available)
        } else {
            kept
        };
        // Signing in again replaces this computer's device login: the old one
        // is revoked rather than left behind in 授权设备管理.
        let replaced = AuthStore::load_or_create(self.config.encrypt_secrets)
            .ok()
            .and_then(|auth| auth.jucode_tokens().map(|t| t.refresh_token.clone()));
        if let Some(refresh_token) = replaced {
            let api_url = self.config.jucode_api_url.clone();
            thread::spawn(move || {
                if let Err(error) = oauth::revoke(&api_url, &refresh_token) {
                    crate::log_error!("oauth", "revoking the replaced login failed", error = error);
                }
            });
        }
        self.config.provider = "jucode".to_string();
        self.config.jucode_web_url = result.web_url.clone();
        self.config.jucode_api_url = result.api_url.clone();
        self.config.base_url = format!("{}/v1", result.api_url);
        self.config.jucode_models = visible.clone();
        self.config.models = visible;
        if !self
            .config
            .models
            .iter()
            .any(|m| m.name == self.config.model)
        {
            if let Some(model) = self.config.models.first().map(|m| m.name.clone()) {
                self.config.model = model.clone();
                let supported = self.reasoning_efforts_for_model(&model);
                if !supported
                    .iter()
                    .any(|effort| effort == &self.config.reasoning_effort)
                {
                    self.config.reasoning_effort = self.default_reasoning_effort_for_model(&model);
                }
            }
        }
        self.auth.set_jucode_tokens(JucodeTokens {
            access_token: result.tokens.access_token,
            refresh_token: result.tokens.refresh_token,
            access_expires_at: result.tokens.access_expires_at,
            refresh_expires_at: result.tokens.refresh_expires_at,
            machine: crate::machine::machine_id().map(str::to_string),
        });
        match self.auth.save().and_then(|_| self.config.save()) {
            Ok(()) => {
                let mut events = vec![
                    AgentEvent::Info(
                        "JuCode account connected; provider switched to jucode".to_string(),
                    ),
                    self.model_status_event(),
                ];
                events.extend(self.sync_default_skills_events());
                events
            }
            Err(error) => vec![AgentEvent::Error(format!("failed to save login: {error}"))],
        }
    }

    /// List the rewindable points — the user turns on the active branch.
    fn checkpoint_list_events(&self) -> Vec<AgentEvent> {
        let turns = self.session.user_turns();
        if turns.is_empty() {
            return vec![AgentEvent::Info(
                "no earlier turns to rewind to".to_string(),
            )];
        }
        vec![AgentEvent::CheckpointView(
            turns
                .into_iter()
                .map(|turn| {
                    let label: String = turn
                        .content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(60)
                        .collect();
                    SessionListItemView {
                        active: false,
                        label: if label.trim().is_empty() {
                            "(empty)".to_string()
                        } else {
                            label
                        },
                        detail: format_checkpoint_age(turn.created_at),
                        id: turn.id,
                    }
                })
                .collect(),
        )]
    }

    /// Rewind to a user turn: truncate the conversation to before it and
    /// reconstruct the working tree to its state at that point.
    fn checkpoint_restore_events(&mut self, id: &str) -> Vec<AgentEvent> {
        if self.running {
            return vec![AgentEvent::Error(
                "cannot rewind while a response is running".to_string(),
            )];
        }
        let Some(t) = self.session.user_turn_created_at(id) else {
            return vec![AgentEvent::Error(
                "that is not a rewindable turn".to_string(),
            )];
        };
        if let Err(error) = self.session.checkout(id) {
            return vec![AgentEvent::Error(format!(
                "failed to rewind conversation: {error}"
            ))];
        }
        let (restored, removed) =
            match crate::tools::restore_to_timestamp(&self.cwd, t, &self.tool_state) {
                Ok(result) => (
                    result
                        .get("restored")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0),
                    result
                        .get("removed")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0),
                ),
                Err(error) => {
                    return vec![
                        AgentEvent::Transcript(self.session.transcript_items()),
                        AgentEvent::Error(format!(
                            "conversation rewound, but file restore failed: {error}"
                        )),
                    ];
                }
            };
        let save_event = self.save_session_event();
        vec![
            AgentEvent::Transcript(self.session.transcript_items()),
            AgentEvent::Info(format!(
                "rewound · restored {restored} file(s), removed {removed}"
            )),
        ]
        .into_iter()
        .chain(save_event)
        .collect()
    }

    fn resume_list_events(&self) -> Vec<AgentEvent> {
        match SessionStore::list_for_cwd(&self.profile_dir, &self.cwd) {
            Ok(sessions) if sessions.is_empty() => {
                vec![AgentEvent::Info(
                    "no sessions for current directory".to_string(),
                )]
            }
            Ok(sessions) => vec![AgentEvent::ResumeView(
                sessions
                    .into_iter()
                    .map(|summary| {
                        let active = summary.id == self.session.session_id();
                        let id = summary.id.clone();
                        let detail = format_resume_detail(&summary);
                        SessionListItemView {
                            active,
                            label: if summary.label.is_empty() {
                                "(no messages yet)".to_string()
                            } else {
                                summary.label
                            },
                            detail,
                            id,
                        }
                    })
                    .collect(),
            )],
            Err(error) => vec![AgentEvent::Error(format!(
                "failed to list sessions: {error}"
            ))],
        }
    }

    fn resume_session_events(&mut self, session_id: &str) -> Vec<AgentEvent> {
        if self.running {
            return vec![AgentEvent::Error(
                "cannot resume a session while a response is running".to_string(),
            )];
        }
        if session_id == self.session.session_id() {
            return vec![AgentEvent::Status(format!(
                "already on session {session_id}"
            ))];
        }
        // Take the target session's lock before loading so two processes can
        // never resume (and append to) the same journal concurrently.
        let lock = match SessionLock::acquire(&self.profile_dir, &self.cwd, session_id) {
            Ok(lock) => lock,
            Err(error) => {
                return vec![AgentEvent::Error(format!(
                    "cannot resume {session_id}: {error}"
                ))]
            }
        };
        match SessionStore::load_for_cwd(&self.profile_dir, &self.cwd, session_id) {
            Ok(session) => {
                self.queued.clear();
                self.receiver = None;
                // Any in-flight resume summary belongs to the old session; drop it.
                self.resume_summary_receiver = None;
                self.resume_summary_running = false;
                self.goal_tool_receiver = None;
                self.goal_continuation_running = false;
                self.turn_started_at = None;
                self.turn_goal_tokens = 0;
                self.session = session;
                // Release the previous session's lock only after the switch.
                self.session_lock = Some(lock);
                vec![
                    AgentEvent::Transcript(self.session.transcript_items()),
                    AgentEvent::PendingMessages(Vec::new()),
                    self.model_status_event(),
                    self.context_usage_event(),
                    AgentEvent::Status(format!("resumed session {}", self.session.session_id())),
                ]
            }
            Err(error) => {
                crate::log_error!(
                    "session",
                    "failed to load session",
                    session = session_id,
                    error = error.to_string()
                );
                vec![AgentEvent::Error(format!(
                    "failed to resume {session_id}: {error}"
                ))]
            }
        }
    }

    fn context_events(&self) -> Vec<AgentEvent> {
        let stats = self.session.context_statistics(&self.config.model);
        vec![
            AgentEvent::Info(format_context_statistics(
                &stats,
                self.total_input_tokens,
                self.total_cached_input_tokens,
                self.total_output_tokens,
                self.total_cost,
            )),
            self.context_usage_event(),
        ]
    }

    fn goal_command_events(&mut self, arg: &str) -> Vec<AgentEvent> {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            return vec![AgentEvent::Goal(self.session.goal().map(goal_view))];
        }

        let result = match trimmed.to_ascii_lowercase().as_str() {
            "pause" => self.session.set_goal_status(ThreadGoalStatus::Paused),
            "resume" => self.session.set_goal_status(ThreadGoalStatus::Active),
            "blocked" => self.session.set_goal_status(ThreadGoalStatus::Blocked),
            "complete" => self.session.set_goal_status(ThreadGoalStatus::Complete),
            "clear" => {
                let cleared = self.session.clear_goal();
                let mut events = vec![AgentEvent::Goal(None)];
                if cleared {
                    events.extend(self.save_session_event());
                    events.push(self.context_usage_event());
                    events.push(AgentEvent::Status("goal cleared".to_string()));
                }
                return events;
            }
            _ => self.session.set_goal_objective(trimmed, None),
        };

        match result {
            Ok(goal) => {
                let mut events = vec![AgentEvent::Goal(Some(goal_view(&goal)))];
                events.extend(self.save_session_event());
                events.push(self.context_usage_event());
                events
            }
            Err(error) => vec![AgentEvent::Error(error)],
        }
    }

    fn stats_events(&self) -> Vec<AgentEvent> {
        self.context_events()
    }

    fn doctor_events(&self) -> Vec<AgentEvent> {
        let mut lines = Vec::new();
        lines.push(format!("provider: {}", self.config.provider));
        lines.push(format!("model: {}", self.config.model));
        lines.push(format!(
            "auth: {}",
            if self.provider_api_key().is_some() || env::var_os(&self.config.api_key_env).is_some()
            {
                "ok"
            } else {
                "missing"
            }
        ));
        lines.push(format!("cwd: {}", self.cwd.display()));
        lines.push(format!("git: {}", command_ok("git", "--version")));
        lines.push(format!("rg: {}", command_ok("rg", "--version")));
        lines.push(crate::logging::doctor_line());
        match discover_project_instructions(&self.cwd) {
            Ok(instructions) => lines.push(format!(
                "project instructions: {} file(s)",
                instructions.len()
            )),
            Err(error) => lines.push(format!("project instructions: error: {error}")),
        }
        lines.push(self.mcp.doctor_line());
        vec![AgentEvent::Info(lines.join("\n"))]
    }

    /// `/mcp` — list servers; `tools <server>`, `reload <server>`,
    /// `enable|disable <server>` subcommands.
    fn mcp_command_events(&mut self, args: &str) -> Vec<AgentEvent> {
        let mut parts = args.split_whitespace();
        match (parts.next(), parts.next()) {
            (None, _) => vec![AgentEvent::Info(self.mcp_list_lines())],
            (Some("tools"), Some(server)) => vec![self.mcp_tools_info(server)],
            (Some("reload"), Some(server)) => match self.mcp.reload(server, &self.cwd) {
                Ok(()) => vec![AgentEvent::Status(format!(
                    "reconnecting MCP server {server}"
                ))],
                Err(error) => vec![AgentEvent::Error(error)],
            },
            (Some(action @ ("enable" | "disable")), Some(server)) => {
                self.mcp_toggle(server, action == "enable")
            }
            _ => vec![AgentEvent::Error(
                "usage: /mcp [tools|reload|enable|disable] [server]".to_string(),
            )],
        }
    }

    fn mcp_list_lines(&self) -> String {
        let views = self.mcp.views();
        if views.is_empty() {
            return "mcp: no servers configured (add mcp_servers to config.json)".to_string();
        }
        views
            .iter()
            .map(|server| {
                let mut line = format!("{} ({}): {}", server.name, server.transport, server.state);
                match (&server.error, server.state.as_str()) {
                    (Some(error), _) => line.push_str(&format!(" - {error}")),
                    (None, "connected") => {
                        line.push_str(&format!(", {} tool(s)", server.tools.len()))
                    }
                    _ => {}
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn mcp_tools_info(&self, server: &str) -> AgentEvent {
        let Some(view) = self.mcp.views().into_iter().find(|v| v.name == server) else {
            return AgentEvent::Error(format!("unknown MCP server: {server}"));
        };
        if view.state != "connected" {
            return AgentEvent::Info(format!("MCP server {server}: {}", view.state));
        }
        if view.tools.is_empty() {
            return AgentEvent::Info(format!("MCP server {server}: no tools"));
        }
        let lines = view
            .tools
            .iter()
            .map(|tool| {
                if tool.description.is_empty() {
                    format!(
                        "{} ({})",
                        tool.name,
                        crate::mcp::mcp_tool_name(server, &tool.name)
                    )
                } else {
                    format!(
                        "{} ({}) - {}",
                        tool.name,
                        crate::mcp::mcp_tool_name(server, &tool.name),
                        tool.description
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        AgentEvent::Info(format!("MCP server {server} tools:\n{lines}"))
    }

    pub fn mcp_servers_event(&self) -> AgentEvent {
        AgentEvent::McpServers {
            servers: self.mcp.views(),
        }
    }

    /// Add or update one MCP server from a full config entry (serve `mcp_set`).
    pub fn mcp_set(&mut self, entry: &Value) -> Vec<AgentEvent> {
        let config = match crate::config::parse_mcp_server_value(entry) {
            Ok(config) => config,
            Err(error) => return vec![AgentEvent::Error(error)],
        };
        let upsert = |servers: &mut Vec<crate::config::McpServerConfig>| match servers
            .iter_mut()
            .find(|existing| existing.name == config.name)
        {
            Some(existing) => *existing = config.clone(),
            None => servers.push(config.clone()),
        };
        if let Err(error) = self.change_config(|current| upsert(&mut current.mcp_servers)) {
            return vec![AgentEvent::Error(format!("failed to save config: {error}"))];
        }
        self.mcp.upsert(config, &self.cwd);
        vec![self.mcp_servers_event()]
    }

    /// Remove one MCP server by name, persisting the change (serve `mcp_remove`).
    pub fn mcp_remove(&mut self, name: &str) -> Vec<AgentEvent> {
        if !self
            .config
            .mcp_servers
            .iter()
            .any(|server| server.name == name)
            && !self.mcp.contains(name)
        {
            return vec![AgentEvent::Error(format!("unknown MCP server: {name}"))];
        }
        if let Err(error) =
            self.change_config(|current| current.mcp_servers.retain(|server| server.name != name))
        {
            return vec![AgentEvent::Error(format!("failed to save config: {error}"))];
        }
        self.mcp.remove(name);
        vec![self.mcp_servers_event()]
    }

    /// Enable/disable one MCP server, persisting the change (serve `mcp_toggle`
    /// and `/mcp enable|disable`).
    pub fn mcp_toggle(&mut self, name: &str, enabled: bool) -> Vec<AgentEvent> {
        if !self
            .config
            .mcp_servers
            .iter()
            .any(|server| server.name == name)
        {
            return vec![AgentEvent::Error(format!("unknown MCP server: {name}"))];
        }
        if let Err(error) = self.change_config(|current| {
            for server in &mut current.mcp_servers {
                if server.name == name {
                    server.enabled = enabled;
                }
            }
        }) {
            return vec![AgentEvent::Error(format!("failed to save config: {error}"))];
        }
        if let Err(error) = self.mcp.set_enabled(name, enabled, &self.cwd) {
            return vec![AgentEvent::Error(error)];
        }
        let mut events = vec![AgentEvent::Status(format!(
            "MCP server {name} {}",
            if enabled { "enabled" } else { "disabled" }
        ))];
        events.push(self.mcp_servers_event());
        events
    }

    fn model_command_events(&mut self, args: Vec<&str>) -> Vec<AgentEvent> {
        match args.as_slice() {
            [] => {
                self.reload_model_list();
                vec![AgentEvent::ModelView {
                    models: self.model_options(),
                    active_effort: self.config.reasoning_effort.clone(),
                }]
            }
            [model] if self.is_reasoning_effort_for_current_model(model) => {
                self.set_model_config(self.config.model.clone(), (*model).to_string())
            }
            [model] => {
                let reasoning_effort = self
                    .reasoning_efforts_for_model(model)
                    .into_iter()
                    .find(|effort| effort == &self.config.reasoning_effort)
                    .unwrap_or_else(|| self.default_reasoning_effort_for_model(model));
                self.set_model_config((*model).to_string(), reasoning_effort)
            }
            [model, effort] => {
                if !self.is_reasoning_effort_for_model(model, effort) {
                    return vec![AgentEvent::Error(format!(
                        "{model} does not support reasoning effort: {effort}"
                    ))];
                }
                self.set_model_config((*model).to_string(), (*effort).to_string())
            }
            _ => vec![AgentEvent::Error(
                "usage: /model [model] [none|low|medium|high]".to_string(),
            )],
        }
    }

    /// Desktop edits the visible JuCode models in config.json; pick them up
    /// when the model list opens instead of waiting for a restart.
    fn reload_model_list(&mut self) {
        if let Ok(disk) = Config::load_or_create() {
            self.config.context_window_overrides = disk.context_window_overrides.clone();
            if disk.provider == self.config.provider {
                self.config.models = disk.models;
                self.config.jucode_models = disk.jucode_models;
                self.config.jucode_groups = disk.jucode_groups;
            }
        }
    }

    /// Applies `change` to the config file as it is on disk now, then to this
    /// engine's copy. Other engines (in this process or another) may have
    /// saved since this one loaded the file; saving the whole in-memory copy
    /// would silently undo their changes.
    fn change_config(&mut self, change: impl Fn(&mut Config)) -> io::Result<()> {
        let mut current = Config::load_or_create()?;
        change(&mut current);
        current.save()?;
        change(&mut self.config);
        Ok(())
    }

    fn set_model_config(&mut self, model: String, reasoning_effort: String) -> Vec<AgentEvent> {
        if model.trim().is_empty() {
            return vec![AgentEvent::Error("model cannot be empty".to_string())];
        }
        if !self.is_reasoning_effort_for_model(&model, &reasoning_effort) {
            return vec![AgentEvent::Error(format!(
                "{model} does not support reasoning effort: {reasoning_effort}"
            ))];
        }

        match self.change_config(|config| {
            config.model = model.clone();
            config.reasoning_effort = reasoning_effort.clone();
        }) {
            Ok(()) => vec![self.model_status_event()],
            Err(error) => vec![AgentEvent::Error(format!("failed to save config: {error}"))],
        }
    }

    fn model_options(&self) -> Vec<ModelOptionView> {
        self.config
            .models
            .iter()
            .map(|model_config| {
                let active = model_config.name == self.config.model;
                ModelOptionView {
                    model: model_config.name.clone(),
                    label: model_config.display_name.clone(),
                    active,
                    // The window this engine budgets with: the user's
                    // override when set, else the gateway's.
                    context_window: self.config.model_config(&model_config.name).context_window,
                    max_output_tokens: model_config.max_output_tokens,
                    reasoning_efforts: model_config.reasoning_efforts.clone(),
                }
            })
            .collect()
    }

    fn reasoning_efforts_for_model(&self, model: &str) -> Vec<String> {
        self.config
            .models
            .iter()
            .find(|entry| entry.name == model)
            .map(|entry| entry.reasoning_efforts.clone())
            .unwrap_or_else(|| self.config.current_model_config().reasoning_efforts)
    }

    fn default_reasoning_effort_for_model(&self, model: &str) -> String {
        let efforts = self.reasoning_efforts_for_model(model);
        if efforts.iter().any(|effort| effort == "medium") {
            "medium".to_string()
        } else {
            efforts
                .first()
                .cloned()
                .unwrap_or_else(|| "medium".to_string())
        }
    }

    fn is_reasoning_effort_for_current_model(&self, value: &str) -> bool {
        self.is_reasoning_effort_for_model(&self.config.model, value)
    }

    fn is_reasoning_effort_for_model(&self, model: &str, value: &str) -> bool {
        self.reasoning_efforts_for_model(model)
            .iter()
            .any(|effort| effort == value)
    }
}

fn mask_key(value: Option<&str>) -> String {
    match value {
        Some(value) if value.len() > 8 => {
            format!("{}...{}", &value[..4], &value[value.len() - 4..])
        }
        Some(_) => "(set)".to_string(),
        None => "(not set)".to_string(),
    }
}

fn not_offered_reason(skill: &skills::SourceSkill) -> String {
    format!("{}; not redistributed by JuCode", skill.license)
}

/// Splits a command line into its command token and argument text, tolerating
/// leading (including Unicode) whitespace so argument slicing stays correct.
fn split_command_line(input: &str) -> (&str, &str) {
    let input = input.trim_start();
    let command = input.split_whitespace().next().unwrap_or("");
    (command, &input[command.len()..])
}

fn current_utc_date() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() / 86_400)
        .unwrap_or(0);
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Models shown after the first login, before the user picks their own
/// (Desktop's ModelSetup preselects the same list): the GPT-6 and Claude Fable
/// families, the newest Opus and Sonnet, and the fast DeepSeek and GLM models,
/// in this order. An account that can reach none of them sees its first few
/// models instead.
const DEFAULT_JUCODE_MODELS: &[&str] = &[
    "gpt-6.1-sol",
    "codex-auto-review",
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "claude-sonnet-5-5",
    "claude-opus-5-5",
    "claude-fable-5-1",
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-sonnet-5",
    "deepseek-v4.1-flash",
    "glm-5.3-flash",
    "kimi-k3",
];

fn default_jucode_models(available: &[ModelConfig]) -> Vec<ModelConfig> {
    let picked: Vec<ModelConfig> = DEFAULT_JUCODE_MODELS
        .iter()
        .filter_map(|name| available.iter().find(|m| m.name == *name).cloned())
        .collect();
    if picked.is_empty() {
        available.iter().take(6).cloned().collect()
    } else {
        picked
    }
}

/// A gateway model as the engine runs it. Values the gateway leaves unset stay
/// unknown (window 0, no output cap) rather than defaulted: the operator
/// configures them in the gateway admin, or the user per model
/// (`context_window_overrides`). Claude keeps its thinking tiers and a 32K
/// output floor — the tiers follow from the model family and Anthropic's
/// Messages API requires `max_tokens`.
fn jucode_model_config(model: &OAuthModel) -> ModelConfig {
    let is_claude = model.id.starts_with("claude-");
    let mut reasoning_efforts = model
        .reasoning_efforts
        .clone()
        .unwrap_or_else(|| vec!["none".to_string()]);
    let mut max_output_tokens = model.max_output_tokens.unwrap_or(0);
    // The gateway may advertise Claude models with only "none" (or nothing);
    // still surface the thinking tiers (and a budget large enough to use them).
    if is_claude && crate::config::is_thinking_disabled(&reasoning_efforts) {
        reasoning_efforts = crate::config::claude_thinking_tiers(&model.id);
        max_output_tokens = max_output_tokens.max(crate::config::CLAUDE_MIN_MAX_OUTPUT_TOKENS);
    }
    let context_window = model.context_window.unwrap_or(0);
    ModelConfig {
        name: model.id.clone(),
        context_window,
        max_context_window: model.max_context_window.unwrap_or(context_window),
        max_output_tokens,
        reasoning_efforts,
        input_cost: 0.0,
        cached_input_cost: 0.0,
        output_cost: 0.0,
        display_name: model.display_name.clone(),
        group_windows: model.group_windows.clone(),
    }
}

fn normalize_resume_status(status: ThreadGoalStatus) -> ThreadGoalStatus {
    match status {
        ThreadGoalStatus::Complete => ThreadGoalStatus::Complete,
        _ => ThreadGoalStatus::Active,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn command_ok(program: &str, arg: &str) -> &'static str {
    match Command::new(program).arg(arg).output() {
        Ok(output) if output.status.success() => "ok",
        _ => "missing",
    }
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

fn target_context_budget(model_config: &ModelConfig, threshold_percent: u64) -> usize {
    let percent = threshold_percent.clamp(10, 95) as usize;
    (model_config.context_window as usize).saturating_mul(percent) / 100
}

/// A budget of 0 means the window is unknown: never compact on a guess. An
/// over-long request is then caught by the upstream's rejection instead
/// (`is_context_overflow`).
fn should_auto_compact(context_tokens: usize, model_context_budget: usize) -> bool {
    model_context_budget > 0 && context_tokens > model_context_budget
}

/// An upstream rejection because the input exceeded the model's context
/// window. Wording varies by vendor and by the gateway's own rewrite.
fn is_context_overflow(error: &str) -> bool {
    let lower = error.to_lowercase();
    [
        "context_length_exceeded",
        "maximum context length",
        "prompt is too long",
        "prompt too long",
        "exceeds the context window",
        "input exceeds the context",
        "exceed context limit",
        "上下文过长",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn format_context_statistics(
    stats: &ContextStatistics,
    total_input_tokens: u64,
    total_cached_input_tokens: u64,
    total_output_tokens: u64,
    total_cost: f64,
) -> String {
    let mut lines = vec![
        format!(
            "context: branch_entries={} context_items={} projected_items={} compacted={}",
            stats.branch_entries, stats.context_items, stats.projected_items, stats.compacted
        ),
        format!(
            "context_tokens: full={} projected={} tokenizer={} api_usage_input={} api_usage_cached_input={} api_usage_output={} cost=${:.4}",
            stats.tokens,
            stats.projected_tokens,
            stats.tokenizer,
            total_input_tokens,
            total_cached_input_tokens,
            total_output_tokens,
            total_cost
        ),
        format!(
            "entries: users={} assistant={} tool_calls={} tool_outputs={} pinned_skills={} branches={} other_response_items={}",
            stats.counts.users,
            stats.counts.assistant_responses,
            stats.counts.tool_calls,
            stats.counts.tool_outputs,
            stats.counts.pinned_skills,
            stats.counts.branches,
            stats.counts.other_response_items
        ),
    ];
    if stats.top_items.is_empty() {
        lines.push("largest_items: none".to_string());
    } else {
        lines.push("largest_items:".to_string());
        lines.extend(stats.top_items.iter().map(|item| {
            format!(
                "  {} ~{} tokens ({} chars)",
                item.label, item.tokens, item.chars
            )
        }));
    }
    lines.join("\n")
}

fn format_checkpoint_age(created_at: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(created_at);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn goal_view(goal: &ThreadGoal) -> GoalView {
    GoalView {
        objective: goal.objective.clone(),
        status: goal.status.as_str().to_string(),
        token_budget: goal.token_budget,
        tokens_used: goal.tokens_used,
        time_used_seconds: goal.time_used_seconds,
        created_at: goal.created_at,
        updated_at: goal.updated_at,
    }
}

fn goal_tool_json(goal: &ThreadGoal) -> Value {
    let remaining_tokens = goal
        .token_budget
        .map(|budget| budget.saturating_sub(goal.tokens_used));
    json!({
        "objective": goal.objective,
        "status": goal.status.as_str(),
        "tokenBudget": goal.token_budget,
        "tokensUsed": goal.tokens_used,
        "remainingTokens": remaining_tokens,
        "timeUsedSeconds": goal.time_used_seconds,
        "createdAt": goal.created_at,
        "updatedAt": goal.updated_at,
    })
}

fn format_resume_detail(summary: &SessionSummary) -> String {
    let status = match summary.resume_status.unwrap_or(ThreadGoalStatus::Active) {
        ThreadGoalStatus::Complete => "completed",
        _ => "working",
    };
    match summary.resume_summary.as_deref() {
        Some(task) => format!("{status} · {task}"),
        None => format!(
            "{status} · updated {} · entries {} · {}",
            summary.updated_at, summary.entries, summary.leaf
        ),
    }
}

const APPROVE_USAGE: &str = "usage: /approve <call-id> <allow|deny> [always] [--hunks id1,id2]";

/// `(call_id, allow, always, hunks)` parsed from an approval command.
type ParsedApprove = (String, bool, bool, Option<Vec<String>>);

/// Parses `/approve <call-id> <allow|deny> [always|once] [--hunks id1,id2]`.
/// `always` allowlists the tool for the session and is incompatible with
/// `--hunks`; `--hunks` requires `allow`.
fn parse_approve_args(args: &str) -> Result<ParsedApprove, String> {
    let mut parts = args.split_whitespace();
    let call_id = parts
        .next()
        .ok_or_else(|| APPROVE_USAGE.to_string())?
        .to_string();
    let allow = match parts.next() {
        Some("allow") => true,
        Some("deny") => false,
        _ => return Err(APPROVE_USAGE.to_string()),
    };
    let mut always = false;
    let mut hunks = None;
    while let Some(token) = parts.next() {
        match token {
            "always" => always = true,
            // The TUI picker submits "allow once" for a one-shot allow.
            "once" => {}
            "--hunks" => {
                let list = parts.next().ok_or_else(|| {
                    "--hunks requires a comma-separated list of hunk ids".to_string()
                })?;
                hunks = Some(parse_hunk_id_list(list)?);
            }
            other => match other.strip_prefix("--hunks=") {
                Some(list) => hunks = Some(parse_hunk_id_list(list)?),
                None => return Err(format!("unexpected token '{other}'; {APPROVE_USAGE}")),
            },
        }
    }
    if hunks.is_some() {
        if always {
            return Err(
                "--hunks cannot be combined with always; approve the whole call to allowlist the tool"
                    .to_string(),
            );
        }
        if !allow {
            return Err(
                "--hunks requires allow; use plain deny to reject the whole call".to_string(),
            );
        }
    }
    Ok((call_id, allow, always, hunks))
}

fn parse_hunk_id_list(list: &str) -> Result<Vec<String>, String> {
    let ids = list
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Err("--hunks requires at least one hunk id".to_string());
    }
    Ok(ids)
}

/// Validates and forwards an approval decision to the parked tool call.
/// Errors leave the request pending (nothing is removed or sent), so a typo
/// in a hunk id never denies or approves the call as a side effect.
fn resolve_approval_decision(
    pending_approvals: &mut HashMap<String, PendingApproval>,
    approved_tools: &mut HashSet<String>,
    call_id: &str,
    allow: bool,
    always: bool,
    hunks: Option<Vec<String>>,
) -> Result<(), String> {
    let pending = pending_approvals
        .get(call_id)
        .ok_or_else(|| "no pending approval for that call".to_string())?;
    if let Some(requested) = &hunks {
        if always {
            return Err(
                "--hunks cannot be combined with always; approve the whole call to allowlist the tool"
                    .to_string(),
            );
        }
        if !allow {
            return Err(
                "--hunks requires allow; use plain deny to reject the whole call".to_string(),
            );
        }
        if pending.hunk_ids.is_empty() {
            return Err(format!(
                "call {call_id} does not support hunk selection; use plain allow or deny"
            ));
        }
        if requested.is_empty() {
            return Err("--hunks requires at least one hunk id".to_string());
        }
        for id in requested {
            if !pending.hunk_ids.contains(id) {
                return Err(format!(
                    "unknown hunk id '{id}'; valid ids: {}",
                    pending.hunk_ids.join(", ")
                ));
            }
        }
    }
    let pending = pending_approvals
        .remove(call_id)
        .expect("pending approval checked above");
    if allow && always {
        approved_tools.insert(pending.name);
    }
    let approved_hunks = hunks.map(|ids| {
        let mut seen = HashSet::new();
        ids.into_iter()
            .filter(|id| seen.insert(id.clone()))
            .collect()
    });
    let _ = pending.response_tx.send(ApprovalDecision {
        allow,
        approved_hunks,
        deferred: None,
    });
    Ok(())
}

/// Label and key-prompt flag for a catalog login flow, or None when JuCode
/// cannot run it. Whole-flow hooks (github-copilot, cursor, …) have no Rust
/// port, so callers skip those providers instead of listing a login that would
/// only mint a credential no request can use.
fn login_kind(login: &llm_provider_kit::omp::LoginRule) -> Option<(&'static str, bool)> {
    use llm_provider_kit::omp::LoginRule;
    match login {
        LoginRule::OauthCode(_) => Some(("oauth", false)),
        LoginRule::DeviceCode(_) => Some(("device code", false)),
        LoginRule::ApiKey(_) => Some(("api key", true)),
        LoginRule::Custom { .. } => None,
    }
}

/// The active provider's bearer: the JuCode session, an omp OAuth credential
/// or a stored API key.
fn provider_api_key(config: &Config, auth: &AuthStore) -> Option<String> {
    if config.provider == "jucode" {
        return auth.jucode_access_token().map(str::to_string);
    }
    let catalog = llm_provider_kit::omp::catalog();
    let store_id = catalog
        .auth_provider(&config.provider)
        .and_then(|p| p.store_as.as_deref())
        .unwrap_or(&config.provider);
    if let Some(credential) = auth.oauth_credential(store_id) {
        return Some(provider_auth::bearer_token(credential));
    }
    auth.key_for(&config.provider).map(str::to_string)
}

/// The JuCode group chosen per model, as the gateway's routing header.
/// Read from disk so a group picked in Desktop applies from the next turn.
fn model_headers(config: &Config) -> HashMap<String, Vec<(String, String)>> {
    if config.provider != "jucode" {
        return HashMap::new();
    }
    let groups = Config::load_or_create()
        .map(|disk| disk.jucode_groups)
        .unwrap_or_else(|_| config.jucode_groups.clone());
    groups
        .into_iter()
        .map(|(model, group)| (model, vec![("X-JuCode-Group".to_string(), group)]))
        .collect()
}

/// One request to the conversation-title model (`Config::title`) on the
/// user's configured provider: no tools, returns the reply text.
pub fn title_completion(system: &str, user: &str) -> Result<String, String> {
    let config = Config::load_or_create().map_err(|error| error.to_string())?;
    let auth = if config.provider == "jucode" {
        oauth::ensure_session(&config.jucode_api_url, config.encrypt_secrets)?
    } else {
        AuthStore::load_or_create(config.encrypt_secrets).map_err(|error| error.to_string())?
    };
    let (model, reasoning_effort) = config.title();
    match title_request(&config, &auth, &model, &reasoning_effort, system, user) {
        // A model that refuses its lightest listed effort (gpt-6-astra takes no
        // "none") gets the next one up.
        Err(error) if rejects_reasoning_effort(&error) => {
            let efforts = config.model_config(&model).reasoning_efforts;
            match efforts
                .iter()
                .skip_while(|effort| **effort != reasoning_effort)
                .nth(1)
            {
                Some(next) => title_request(&config, &auth, &model, next, system, user),
                None => Err(error),
            }
        }
        reply => reply,
    }
}

/// An upstream 400 about the reasoning effort (Responses `reasoning.effort`,
/// Chat `reasoning_effort`).
fn rejects_reasoning_effort(error: &str) -> bool {
    error.contains("reasoning.effort") || error.contains("reasoning_effort")
}

fn title_request(
    config: &Config,
    auth: &AuthStore,
    model: &str,
    reasoning_effort: &str,
    system: &str,
    user: &str,
) -> Result<String, String> {
    let client = OpenAiClient::from_config(OpenAiClientConfig {
        model: model.to_string(),
        provider: config.provider.clone(),
        protocol: config.protocol.clone(),
        reasoning_effort: reasoning_effort.to_string(),
        models: Vec::new(),
        subagent_models: Vec::new(),
        system_prompt: String::new(),
        prompt_cache_key: String::new(),
        mcp: McpManager::default(),
        base_url: config.base_url.clone(),
        max_output_tokens: config.model_config(model).max_output_tokens,
        api_key: provider_api_key(config, auth).as_deref(),
        api_key_env: &config.api_key_env,
        retry_attempts: config.retry_attempts,
        connect_timeout: Duration::from_secs(config.connect_timeout_seconds),
        read_timeout: Duration::from_secs(config.read_timeout_seconds),
        goal_tool_tx: None,
        approval_tx: None,
        approval_mode: LiveApprovalMode::new(config.approval_mode),
        safety_model: None,
        safety_reasoning_effort: String::new(),
        model_headers: model_headers(config),
        edit_tools: Vec::new(),
        extra_read_roots: Vec::new(),
        tool_state: crate::tools::ToolState::default(),
        host: None,
        subagent_manager: None,
        hooks: Hooks::default(),
    })?;
    client.summarize_text(system, user, |_| Ok(()))
}

/// The plan as the model writes its propose_plan call, for clients to show
/// before the call completes.
#[derive(Default)]
struct PlanDraft {
    call_id: String,
    arguments: String,
    title: String,
    sent: usize,
}

impl PlanDraft {
    /// Adds a fragment of the call's arguments; the event carries the title
    /// and the plan text added since the last one (nothing when unchanged).
    fn push(&mut self, call_id: &str, delta: &str) -> Option<AgentEvent> {
        if !call_id.is_empty() && call_id != self.call_id {
            if !self.call_id.is_empty() {
                *self = PlanDraft::default();
            }
            self.call_id = call_id.to_string();
        }
        self.arguments.push_str(delta);
        let title =
            crate::plan_mode::partial_string_field(&self.arguments, "title").unwrap_or_default();
        let plan =
            crate::plan_mode::partial_string_field(&self.arguments, "plan").unwrap_or_default();
        let append = plan.get(self.sent..).unwrap_or_default().to_string();
        if append.is_empty() && title == self.title {
            return None;
        }
        self.sent = plan.len();
        self.title = title.clone();
        Some(AgentEvent::PlanDraft {
            id: if self.call_id.is_empty() {
                "draft".to_string()
            } else {
                self.call_id.clone()
            },
            title,
            append,
        })
    }
}
#[cfg(test)]
mod approval_decision_tests {
    use super::*;

    #[test]
    fn a_plan_draft_sends_its_title_and_only_the_new_text() {
        let mut draft = PlanDraft::default();
        let mut push = |call_id: &str, delta: &str| match draft.push(call_id, delta) {
            Some(AgentEvent::PlanDraft { id, title, append }) => Some((id, title, append)),
            _ => None,
        };
        let event = |id: &str, title: &str, append: &str| {
            Some((id.to_string(), title.to_string(), append.to_string()))
        };
        assert_eq!(
            push("call_1", "{\"title\":\"Sna"),
            event("call_1", "Sna", "")
        );
        assert_eq!(
            push("", "ke\",\"plan\":\"## Go"),
            event("call_1", "Snake", "## Go")
        );
        assert_eq!(push("", "al\\n"), event("call_1", "Snake", "al\n"));
        // Nothing new yet (an escape cut off): no event.
        assert_eq!(push("", "\\"), None);
        // Another call starts over.
        assert_eq!(push("call_2", "{\"title\":\"B"), event("call_2", "B", ""));
    }
    use std::sync::mpsc::TryRecvError;

    fn pending(
        hunk_ids: &[&str],
    ) -> (HashMap<String, PendingApproval>, Receiver<ApprovalDecision>) {
        let (response_tx, response_rx) = mpsc::channel();
        let mut map = HashMap::new();
        map.insert(
            "call_1".to_string(),
            PendingApproval {
                response_tx,
                name: "apply_patch".to_string(),
                hunk_ids: hunk_ids.iter().map(|id| id.to_string()).collect(),
                summary: String::new(),
                arguments: String::new(),
                cwd: PathBuf::new(),
                subagent_id: None,
            },
        );
        (map, response_rx)
    }

    #[test]
    fn approve_parses_plain_allow_deny_always_and_hunks() {
        assert_eq!(
            parse_approve_args("call_1 allow").unwrap(),
            ("call_1".to_string(), true, false, None)
        );
        assert_eq!(
            parse_approve_args("call_1 allow once").unwrap(),
            ("call_1".to_string(), true, false, None)
        );
        assert_eq!(
            parse_approve_args("call_1 allow always").unwrap(),
            ("call_1".to_string(), true, true, None)
        );
        assert_eq!(
            parse_approve_args("call_1 deny").unwrap(),
            ("call_1".to_string(), false, false, None)
        );
        let with_hunks = parse_approve_args("call_1 allow --hunks f0h1,f0h3").unwrap();
        assert_eq!(
            with_hunks,
            (
                "call_1".to_string(),
                true,
                false,
                Some(vec!["f0h1".to_string(), "f0h3".to_string()])
            )
        );
        assert_eq!(
            parse_approve_args("call_1 allow --hunks=f1h2").unwrap().3,
            Some(vec!["f1h2".to_string()])
        );
    }

    #[test]
    fn approve_parsing_rejects_bad_syntax_and_incompatible_flags() {
        assert!(parse_approve_args("").unwrap_err().contains("usage"));
        assert!(parse_approve_args("call_1").unwrap_err().contains("usage"));
        assert!(parse_approve_args("call_1 maybe")
            .unwrap_err()
            .contains("usage"));
        assert!(parse_approve_args("call_1 allow --hunks")
            .unwrap_err()
            .contains("comma-separated"));
        assert!(parse_approve_args("call_1 allow --hunks ,")
            .unwrap_err()
            .contains("at least one hunk id"));
        assert!(parse_approve_args("call_1 allow always --hunks f0h1")
            .unwrap_err()
            .contains("cannot be combined with always"));
        assert!(parse_approve_args("call_1 deny --hunks f0h1")
            .unwrap_err()
            .contains("requires allow"));
    }

    #[test]
    fn unknown_hunk_id_fails_the_command_and_keeps_the_request_pending() {
        let (mut pending_approvals, response_rx) = pending(&["f0h1", "f0h2"]);
        let mut approved_tools = HashSet::new();

        let error = resolve_approval_decision(
            &mut pending_approvals,
            &mut approved_tools,
            "call_1",
            true,
            false,
            Some(vec!["f9h9".to_string()]),
        )
        .unwrap_err();

        assert!(error.contains("unknown hunk id 'f9h9'"), "{error}");
        assert!(error.contains("f0h1, f0h2"), "{error}");
        assert!(
            pending_approvals.contains_key("call_1"),
            "request must stay pending"
        );
        assert_eq!(response_rx.try_recv().unwrap_err(), TryRecvError::Empty);
    }

    #[test]
    fn hunk_subset_approval_sends_a_deduplicated_selection() {
        let (mut pending_approvals, response_rx) = pending(&["f0h1", "f0h2"]);
        let mut approved_tools = HashSet::new();

        resolve_approval_decision(
            &mut pending_approvals,
            &mut approved_tools,
            "call_1",
            true,
            false,
            Some(vec![
                "f0h2".to_string(),
                "f0h2".to_string(),
                "f0h1".to_string(),
            ]),
        )
        .unwrap();

        let decision = response_rx.recv().unwrap();
        assert!(decision.allow);
        assert_eq!(
            decision.approved_hunks,
            Some(vec!["f0h2".to_string(), "f0h1".to_string()])
        );
        assert!(pending_approvals.is_empty());
        assert!(approved_tools.is_empty(), "--hunks never allowlists");
    }

    #[test]
    fn hunk_selection_is_rejected_for_calls_without_hunks() {
        let (mut pending_approvals, _response_rx) = pending(&[]);
        let mut approved_tools = HashSet::new();

        let error = resolve_approval_decision(
            &mut pending_approvals,
            &mut approved_tools,
            "call_1",
            true,
            false,
            Some(vec!["f0h1".to_string()]),
        )
        .unwrap_err();

        assert!(error.contains("does not support hunk selection"), "{error}");
        assert!(pending_approvals.contains_key("call_1"));
    }

    #[test]
    fn whole_call_allow_always_still_allowlists_the_tool() {
        let (mut pending_approvals, response_rx) = pending(&["f0h1"]);
        let mut approved_tools = HashSet::new();

        resolve_approval_decision(
            &mut pending_approvals,
            &mut approved_tools,
            "call_1",
            true,
            true,
            None,
        )
        .unwrap();

        assert!(approved_tools.contains("apply_patch"));
        let decision = response_rx.recv().unwrap();
        assert!(decision.allow);
        assert!(decision.approved_hunks.is_none());
    }
}

#[cfg(test)]
mod command_parsing_tests {
    use super::split_command_line;

    #[test]
    fn split_command_line_ignores_leading_unicode_whitespace() {
        let (command, args) = split_command_line("\u{3000} \t/goal 完成任务");
        assert_eq!(command, "/goal");
        assert_eq!(args.trim(), "完成任务");
    }

    #[test]
    fn split_command_line_handles_bare_command_and_empty_input() {
        assert_eq!(split_command_line("/help"), ("/help", ""));
        assert_eq!(split_command_line("   "), ("", ""));
    }
}

#[cfg(test)]
mod model_config_tests {
    use super::*;

    fn oauth_model(id: &str) -> OAuthModel {
        OAuthModel {
            id: id.to_string(),
            context_window: None,
            max_context_window: None,
            max_output_tokens: None,
            reasoning_efforts: None,
            display_name: None,
            group_windows: Default::default(),
        }
    }

    #[test]
    fn default_jucode_models_preserve_original_catalog_order() {
        let requested = [
            "gpt-6.1-sol",
            "codex-auto-review",
            "gpt-6-astra",
            "gpt-6-sol",
            "gpt-6-luna",
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "claude-sonnet-5-5",
            "claude-opus-5-5",
            "claude-fable-5-1",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-sonnet-5",
            "deepseek-v4.1-flash",
            "glm-5.3-flash",
            "kimi-k3",
        ];
        let available: Vec<_> = requested
            .iter()
            .rev()
            .chain(["gpt-5.5", "private-model"].iter())
            .map(|id| jucode_model_config(&oauth_model(id)))
            .collect();
        let selected = default_jucode_models(&available);
        assert_eq!(
            selected
                .iter()
                .map(|model| model.name.as_str())
                .collect::<Vec<_>>(),
            requested
        );
        let subset = vec![jucode_model_config(&oauth_model("claude-fable-5"))];
        assert_eq!(default_jucode_models(&subset)[0].name, "claude-fable-5");
        let other = vec![jucode_model_config(&oauth_model("private-model"))];
        assert_eq!(default_jucode_models(&other)[0].name, "private-model");
    }

    #[test]
    fn unconfigured_gateway_values_stay_unknown() {
        let config = jucode_model_config(&oauth_model("gpt-6-sol"));
        assert_eq!(config.context_window, 0);
        assert_eq!(config.max_context_window, 0);
        assert_eq!(config.max_output_tokens, 0);
        assert!(crate::config::is_thinking_disabled(
            &config.reasoning_efforts
        ));
    }

    #[test]
    fn gateway_window_range_is_kept() {
        let model = OAuthModel {
            context_window: Some(272_000),
            max_context_window: Some(1_050_000),
            ..oauth_model("gpt-6-sol")
        };
        let config = jucode_model_config(&model);
        assert_eq!(
            (config.context_window, config.max_context_window),
            (272_000, 1_050_000)
        );
        // A gateway that only sends the smallest window means "one size".
        let single = jucode_model_config(&OAuthModel {
            context_window: Some(200_000),
            ..oauth_model("x")
        });
        assert_eq!(single.max_context_window, 200_000);
    }

    #[test]
    fn unknown_window_never_triggers_compaction() {
        assert!(!should_auto_compact(1_000_000, 0));
        assert!(should_auto_compact(150_001, 150_000));
    }

    #[test]
    fn detects_context_overflow_rejections() {
        assert!(is_context_overflow(
            r#"HTTP 400: {"error":{"code":"context_length_exceeded","message":"..."}}"#
        ));
        assert!(is_context_overflow(
            "prompt is too long: 210000 tokens > 200000 maximum"
        ));
        assert!(is_context_overflow(
            "上下文过长：当前约 300000 tokens，模型上限 272000。"
        ));
        assert!(is_context_overflow(
            "input length and `max_tokens` exceed context limit: 188240 + 32000 > 200000"
        ));
        assert!(!is_context_overflow("HTTP 429: rate limited"));
    }

    #[test]
    fn claude_models_offer_thinking_strength_tiers() {
        // Regression: Claude models synced from /login used to default to
        // ["none"] only, so no thinking strength could be selected.
        let config = jucode_model_config(&oauth_model("claude-opus-4-8"));
        assert_eq!(
            config.reasoning_efforts,
            vec!["none", "low", "medium", "high", "xhigh", "max"]
        );
        // max_output must leave room for the higher tiers' thinking budgets.
        assert!(config.max_output_tokens >= 32_000);
    }

    #[test]
    fn marketplace_reasoning_efforts_override_claude_defaults() {
        let model = OAuthModel {
            id: "claude-sonnet-4-6".to_string(),
            context_window: Some(1_000_000),
            max_context_window: None,
            max_output_tokens: Some(64_000),
            reasoning_efforts: Some(vec!["low".to_string(), "high".to_string()]),
            display_name: None,
            group_windows: Default::default(),
        };
        let config = jucode_model_config(&model);
        assert_eq!(config.reasoning_efforts, vec!["low", "high"]);
        assert_eq!(config.max_output_tokens, 64_000);
        assert_eq!(config.context_window, 1_000_000);
    }
}

#[cfg(test)]
mod login_kind_tests {
    use super::*;
    use llm_provider_kit::omp::{catalog, LoginRule};

    fn kind_of(id: &str) -> Option<(&'static str, bool)> {
        let login = catalog().auth_provider(id).and_then(|p| p.login.as_ref());
        login.and_then(login_kind)
    }

    #[test]
    fn login_kind_labels_the_ported_flows() {
        assert_eq!(kind_of("anthropic"), Some(("oauth", false)));
        assert_eq!(kind_of("openai-codex"), Some(("oauth", false)));
        assert_eq!(kind_of("kimi-code"), Some(("device code", false)));
        assert_eq!(kind_of("deepseek"), Some(("api key", true)));
    }

    #[test]
    fn login_kind_drops_unported_flows_and_providers_without_one() {
        // Custom hooks have no Rust port; azure (BYOK endpoint + api-key) and
        // the codex device flow declare no runnable login either.
        assert_eq!(kind_of("github-copilot"), None);
        assert_eq!(kind_of("openai-codex-device"), None);
        assert_eq!(kind_of("azure"), None);
        assert_eq!(
            login_kind(&LoginRule::Custom {
                hook: "whatever".to_string()
            }),
            None
        );
    }
}
