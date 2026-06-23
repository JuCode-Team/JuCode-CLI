use crate::{
    config::{profile_dir, AuthStore, Config, ModelConfig},
    event::{AgentEvent, CommandView, GoalView, ModelOptionView, PlanItem, SessionListItemView},
    extensions::ExtensionRegistry,
    hooks::Hooks,
    llm::{
        ApprovalRequest, GoalToolRequest, OpenAiClient, OpenAiClientConfig, StreamEvent,
        ToolGoalResponse,
    },
    oauth::{self, OAuthLoginResult, OAuthModel},
    prompt::{
        build_system_prompt, discover_project_instructions, discover_skills, skill_commands,
        skill_message, skill_pin_message, PromptContext,
    },
    session::{
        compaction_summary_item, ContextStatistics, EntryKind, SessionStore, SessionSummary,
        ThreadGoal, ThreadGoalStatus,
    },
    skills,
    subagents::SubagentManager,
    trust::{self, TrustStore},
    update::{self, UpdateNotice},
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env, io,
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
        Arc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Recent context (in tokenizer-counted tokens) kept verbatim when compacting; older
/// turns are folded into the summary.
const COMPACTION_KEEP_RECENT_TOKENS: usize = 20_000;
const RESUME_SUMMARY_IDLE_SECONDS: u64 = 5 * 60;
const RESUME_SUMMARY_MODEL: &str = "gpt-5.4-mini";

#[derive(Debug)]
enum WorkerEvent {
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
    session: SessionStore,
    profile_dir: PathBuf,
    cwd: PathBuf,
    queued: VecDeque<(String, Vec<String>)>,
    running: bool,
    receiver: Option<Receiver<WorkerEvent>>,
    goal_tool_receiver: Option<Receiver<GoalToolRequest>>,
    approval_receiver: Option<Receiver<ApprovalRequest>>,
    pending_approvals: HashMap<String, (mpsc::Sender<bool>, String)>,
    approved_tools: HashSet<String>,
    update_receiver: Option<Receiver<UpdateNotice>>,
    login_receiver: Option<Receiver<Result<OAuthLoginResult, String>>>,
    total_input_tokens: u64,
    total_cached_input_tokens: u64,
    total_output_tokens: u64,
    total_cost: f64,
    turn_started_at: Option<SystemTime>,
    turn_goal_tokens: u64,
    goal_continuation_running: bool,
    resume_summary_running: bool,
    interrupt_flag: Arc<AtomicBool>,
    subagent_manager: SubagentManager,
    trust: TrustStore,
    project_trusted: bool,
    hooks: Hooks,
    plan: Vec<PlanItem>,
}

impl AgentCore {
    pub fn new() -> io::Result<Self> {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let trust = TrustStore::load_or_create()?;
        let project_trusted = if trust::project_has_local_resources(&cwd) {
            trust.decision_for(&cwd).unwrap_or(false)
        } else {
            true
        };
        let hooks = Hooks::load(&profile_dir()?, &cwd, project_trusted);
        Ok(Self {
            config: Config::load_or_create()?,
            auth: AuthStore::load_or_create()?,
            session: SessionStore::new(),
            profile_dir: profile_dir()?,
            cwd,
            queued: VecDeque::new(),
            running: false,
            receiver: None,
            goal_tool_receiver: None,
            update_receiver: None,
            login_receiver: None,
            total_input_tokens: 0,
            total_cached_input_tokens: 0,
            total_output_tokens: 0,
            total_cost: 0.0,
            turn_started_at: None,
            turn_goal_tokens: 0,
            goal_continuation_running: false,
            resume_summary_running: false,
            interrupt_flag: Arc::new(AtomicBool::new(false)),
            subagent_manager: SubagentManager::default(),
            trust,
            project_trusted,
            hooks,
            plan: Vec::new(),
            approval_receiver: None,
            pending_approvals: HashMap::new(),
            approved_tools: HashSet::new(),
        })
    }

    pub fn startup_events(&self) -> Vec<AgentEvent> {
        let model_config = self.config.current_model_config();
        let mut events = vec![
            AgentEvent::Startup {
                version: env!("CARGO_PKG_VERSION").to_string(),
                session_id: self.session.session_id().to_string(),
                profile_dir: self.config.profile_dir().display().to_string(),
                config_path: self.config.path().display().to_string(),
                cwd: self.cwd.display().to_string(),
                model: self.config.model.clone(),
                context_window: model_config.context_window,
            },
            self.model_status_event(),
            self.command_list_event(),
        ];
        if trust::project_has_local_resources(&self.cwd)
            && self.trust.decision_for(&self.cwd).is_none()
        {
            events.push(AgentEvent::TrustPrompt {
                cwd: self.cwd.display().to_string(),
                repo_root: trust::repo_root(&self.cwd).map(|path| path.display().to_string()),
            });
        }
        for message in self.hooks.session_start(&self.cwd) {
            events.push(AgentEvent::Info(message));
        }
        events
    }

    pub fn start_update_check(&mut self) {
        if self.update_receiver.is_none() {
            self.update_receiver = Some(update::spawn_update_check(env!("CARGO_PKG_VERSION")));
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
            reasoning_effort: self.config.reasoning_effort.clone(),
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
            .map(|spec| CommandView {
                command: spec.name.to_string(),
                marker: spec.advanced.then(|| "ADV".to_string()),
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
        let (images, mut events) = self.validate_image_attachments(images);
        if self.running {
            self.queued.push_back((message, images));
            events.push(AgentEvent::PendingMessages(self.pending_texts()));
            events.push(AgentEvent::Status(format!("queued: {}", self.queued.len())));
            return events;
        }
        if let Err(reason) = self.hooks.user_prompt_submit(&message, &self.cwd) {
            events.push(AgentEvent::UserMessage(message));
            events.push(AgentEvent::Error(format!(
                "blocked by user_prompt_submit hook: {reason}"
            )));
            events.push(AgentEvent::Status("ready".to_string()));
            return events;
        }
        events.push(AgentEvent::UserMessage(message.clone()));
        events.extend(self.start_turn(message, images));
        events
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

    pub fn steer(&mut self) -> Vec<AgentEvent> {
        if !self.running || self.queued.is_empty() {
            return Vec::new();
        }

        self.receiver = None;
        self.running = false;
        let Some((next, images)) = self.queued.pop_front() else {
            return Vec::new();
        };
        let mut events = vec![
            AgentEvent::Status("steering".to_string()),
            AgentEvent::PendingMessages(self.pending_texts()),
            AgentEvent::UserMessage(next.clone()),
        ];
        events.extend(self.start_turn(next, images));
        events
    }

    pub fn interrupt(&mut self) -> Vec<AgentEvent> {
        if !self.running {
            return Vec::new();
        }
        self.interrupt_flag.store(true, Ordering::SeqCst);
        self.subagent_manager.close_all();
        self.receiver = None;
        self.running = false;
        self.goal_tool_receiver = None;
        self.approval_receiver = None;
        self.pending_approvals.clear();
        self.goal_continuation_running = false;
        self.turn_started_at = None;
        self.turn_goal_tokens = 0;
        let mut events = vec![
            AgentEvent::Info("request interrupted".to_string()),
            AgentEvent::Status("interrupted".to_string()),
        ];
        events.extend(self.drain_subagent_events());
        events
    }

    pub fn handle_command(&mut self, input: &str) -> (bool, Vec<AgentEvent>) {
        let mut parts = input.split_whitespace();
        let command = parts.next().unwrap_or_default();
        if let Some(events) = self.skill_command_events(command, input[command.len()..].trim()) {
            return (false, events);
        }
        if !crate::commands::is_known(command) {
            return (
                false,
                vec![AgentEvent::Error(format!("unknown command: {command}"))],
            );
        }

        let events = match command {
            "/quit" | "/exit" => return (true, Vec::new()),
            "/help" | "/" => vec![AgentEvent::Info(crate::commands::help_line())],
            "/login" => self.login_events(input[command.len()..].trim()),
            "/new" => self.new_session_events(),
            "/config" => vec![AgentEvent::Info(format!(
                "provider={} model={} reasoning_effort={} base_url={} jucode_web_url={} jucode_api_url={} auth_key={} api_key_env={} retry_attempts={}",
                self.config.provider,
                self.config.model,
                self.config.reasoning_effort,
                self.config.base_url,
                self.config.jucode_web_url,
                self.config.jucode_api_url,
                mask_key(self.auth.key_for(&self.config.provider)),
                self.config.api_key_env,
                self.config.retry_attempts
            ))],
            "/model" => self.model_command_events(parts.collect()),
            "/tree" => vec![AgentEvent::TreeView(self.session.tree_view())],
            "/trust" => self.trust_command_events(input[command.len()..].trim()),
            "/checkout" => {
                let label = input[command.len()..].trim();
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
                let label = input[command.len()..].trim();
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
                let label = input[command.len()..].trim();
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
            "/approve" => {
                let call_id = parts.next().unwrap_or_default();
                let allow = parts.next() == Some("allow");
                let always = parts.next() == Some("always");
                self.approve_events(call_id, allow, always)
            }
            "/extensions" => self.extension_events(),
            "/context" => self.context_events(),
            "/stats" => self.stats_events(),
            "/goal" => self.goal_command_events(input[command.len()..].trim()),
            "/doctor" => self.doctor_events(),
            "/skills" => self.skills_events(input[command.len()..].trim()),
            "/pin" => self.pin_skill_events(input[command.len()..].trim()),
            "/compact" => self.compact_command_events(),
            // Reached only if a command is registered in `commands::COMMANDS` but
            // has no dispatch arm here — a wiring bug, surfaced explicitly.
            _ => vec![AgentEvent::Error(format!(
                "command not implemented: {command}"
            ))],
        };
        (false, events)
    }

    fn skills_events(&mut self, arg: &str) -> Vec<AgentEvent> {
        let mut parts = arg.split_whitespace();
        match parts.next().unwrap_or("list") {
            "list" => self.list_marketplace_skills_events(),
            "install" => match parts.next() {
                Some(id) => self.install_marketplace_skill_events(id),
                None => vec![AgentEvent::Error("usage: /skills install <id>".to_string())],
            },
            "sync" => self.sync_default_skills_events(),
            other => vec![AgentEvent::Error(format!(
                "unknown /skills action: {other}; use list, install, or sync"
            ))],
        }
    }

    fn list_marketplace_skills_events(&self) -> Vec<AgentEvent> {
        match self.fetch_marketplace() {
            Ok(marketplace) if marketplace.skills.is_empty() => {
                vec![AgentEvent::Info("skills marketplace is empty".to_string())]
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
                vec![AgentEvent::Info(format!(
                    "Available marketplace skills:\n{}\n\nInstall with /skills install <id>; sync defaults with /skills sync.",
                    lines.join("\n")
                ))]
            }
            Err(error) => vec![AgentEvent::Error(format!(
                "failed to fetch skills marketplace: {error}"
            ))],
        }
    }

    fn install_marketplace_skill_events(&mut self, id: &str) -> Vec<AgentEvent> {
        match self.fetch_marketplace() {
            Ok(marketplace) => {
                let Some(skill) = marketplace.skills.iter().find(|skill| skill.id == id) else {
                    return vec![AgentEvent::Error(format!(
                        "marketplace skill not found: {id}"
                    ))];
                };
                match skills::install_marketplace_skill(self.config.profile_dir(), skill) {
                    Ok(()) => vec![
                        AgentEvent::Status(format!("installed skill {}", skill.id)),
                        self.command_list_event(),
                    ],
                    Err(error) => vec![AgentEvent::Error(format!(
                        "failed to install skill {}: {error}",
                        skill.id
                    ))],
                }
            }
            Err(error) => vec![AgentEvent::Error(format!(
                "failed to fetch skills marketplace: {error}"
            ))],
        }
    }

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
                    Err(error) => vec![AgentEvent::Error(format!(
                        "failed to sync default skills: {error}"
                    ))],
                }
            }
            Err(error) => vec![AgentEvent::Error(format!(
                "failed to fetch skills marketplace: {error}"
            ))],
        }
    }

    fn fetch_marketplace(&self) -> Result<skills::Marketplace, String> {
        skills::fetch_marketplace(
            &self.config.jucode_api_url,
            self.auth
                .key_for("jucode")
                .or_else(|| self.auth.key_for(&self.config.provider)),
        )
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
        events.extend(self.start_turn(message, Vec::new()));
        Some(events)
    }

    pub fn poll_events(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let mut disconnected = false;

        if let Some(rx) = self.receiver.take() {
            while let Ok(event) = rx.try_recv() {
                match event {
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
                    WorkerEvent::ResumeSummaryDone {
                        summary,
                        status,
                        summarized_at,
                    } => {
                        self.resume_summary_running = false;
                        self.session
                            .set_resume_summary(Some(summary), Some(status), summarized_at);
                        events.extend(self.save_session_event());
                    }
                    WorkerEvent::ResumeSummaryFailed(_error) => {
                        self.resume_summary_running = false;
                        self.session.set_resume_summary(
                            None,
                            self.session
                                .goal()
                                .map(|goal| normalize_resume_status(goal.status)),
                            now_secs(),
                        );
                        events.extend(self.save_session_event());
                    }
                    WorkerEvent::CallStart => events.push(AgentEvent::Connecting),
                    WorkerEvent::Connected => events.push(AgentEvent::ThinkingStart),
                    WorkerEvent::ReasoningDelta(delta) => {
                        events.push(AgentEvent::ReasoningDelta(delta))
                    }
                    WorkerEvent::Delta(delta) => events.push(AgentEvent::AssistantDelta(delta)),
                    WorkerEvent::Retrying { attempt } => {
                        events.push(AgentEvent::Retrying { attempt });
                    }
                    WorkerEvent::ResponseItem(item) => {
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
                    WorkerEvent::Error(error) => {
                        self.subagent_manager
                            .close_all_with_message("parent turn failed");
                        events.extend(self.finish_goal_turn());
                        self.running = false;
                        disconnected = true;
                        self.goal_tool_receiver = None;
                        events.push(AgentEvent::Error(error));
                    }
                }
            }
            if !disconnected {
                self.receiver = Some(rx);
            }
        }
        events.extend(self.drain_subagent_events());
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
        events.extend(self.poll_goal_tool_requests());
        events.extend(self.poll_approval_requests());
        if self.should_generate_resume_summary() {
            self.start_resume_summary();
        }

        if !self.running {
            if let Some((next, images)) = self.queued.pop_front() {
                self.goal_continuation_running = false;
                events.push(AgentEvent::PendingMessages(self.pending_texts()));
                events.push(AgentEvent::UserMessage(next.clone()));
                events.extend(self.start_turn(next, images));
            } else if self.should_continue_goal() {
                events.extend(self.start_goal_continuation());
            }
        }

        events
    }

    fn drain_subagent_events(&self) -> Vec<AgentEvent> {
        self.subagent_manager
            .drain_events()
            .into_iter()
            .map(|event| AgentEvent::SubagentLifecycle {
                path: event.path,
                status: event.status,
                message: event.message,
            })
            .collect()
    }

    fn start_turn(&mut self, message: String, images: Vec<String>) -> Vec<AgentEvent> {
        self.session.append(EntryKind::User { content: message });
        if !images.is_empty() {
            self.session.append(EntryKind::UserImage { paths: images });
        }
        self.turn_started_at = Some(SystemTime::now());
        self.turn_goal_tokens = 0;
        let save_event = self.save_session_event();

        if self.config.provider == "jucode" && !is_jucode_supported_model(&self.config.model) {
            let mut events = save_event;
            events.push(AgentEvent::Error(format!(
                "{} is not supported by JuCode CLI. Run /model and choose a GPT or Claude model.",
                self.config.model
            )));
            return events;
        }

        self.spawn_current_context_turn(save_event)
    }

    fn spawn_current_context_turn(&mut self, save_event: Vec<AgentEvent>) -> Vec<AgentEvent> {
        self.subagent_manager = SubagentManager::default();
        let base_prompt = match self.config.system_prompt() {
            Ok(prompt) => prompt,
            Err(error) => {
                let mut events = save_event;
                events.push(AgentEvent::Error(format!(
                    "failed to read prompt.txt: {error}"
                )));
                return events;
            }
        };
        let skills =
            match discover_skills(self.config.profile_dir(), &self.cwd, self.project_trusted) {
                Ok(skills) => skills,
                Err(error) => {
                    let mut events = save_event;
                    events.push(AgentEvent::Error(format!(
                        "failed to discover skills: {error}"
                    )));
                    return events;
                }
            };
        let project_instructions = if self.config.include_project_instructions {
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
        let system_prompt = build_system_prompt(
            &base_prompt,
            &PromptContext {
                date: current_utc_date(),
                cwd: self.cwd.clone(),
                tools: vec![
                    "read",
                    "str_replace",
                    "hashline_edit",
                    "write",
                    "apply_patch",
                    "bash",
                    "write_stdin",
                    "ls",
                    "ripgrep",
                    "outline",
                    "checkpoint",
                    "spawn_agent",
                    "wait_agent",
                    "list_agents",
                    "send_message",
                    "close_agent",
                ],
                project_instructions,
                skills,
            },
        );

        let (goal_tool_tx, goal_tool_rx) = mpsc::channel();
        self.goal_tool_receiver = Some(goal_tool_rx);
        let (approval_tx, approval_rx) = mpsc::channel();
        self.approval_receiver = Some(approval_rx);
        self.pending_approvals.clear();
        let Ok(client) = OpenAiClient::from_config(OpenAiClientConfig {
            model: self.config.model.clone(),
            reasoning_effort: self.config.reasoning_effort.clone(),
            system_prompt,
            prompt_cache_key: self.session.session_id().to_string(),
            extensions: ExtensionRegistry::load(
                &self.config.extensions,
                &self.cwd,
                self.config.profile_dir(),
            ),
            base_url: self.config.base_url.clone(),
            max_output_tokens: self.config.current_model_config().max_output_tokens,
            api_key: self.auth.key_for(&self.config.provider),
            api_key_env: &self.config.api_key_env,
            retry_attempts: self.config.retry_attempts,
            connect_timeout: Duration::from_secs(self.config.connect_timeout_seconds),
            read_timeout: Duration::from_secs(self.config.read_timeout_seconds),
            goal_tool_tx: Some(goal_tool_tx),
            approval_tx: Some(approval_tx),
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
        let compaction =
            if should_auto_compact(context_tokens, model_context_budget) {
                self.session
                    .plan_compaction(COMPACTION_KEEP_RECENT_TOKENS, &self.config.model)
            } else {
                None
            };
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
                    StreamEvent::Retrying { attempt } => WorkerEvent::Retrying { attempt },
                    StreamEvent::ResponseItem(item) => WorkerEvent::ResponseItem(item),
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
        if self.config.provider == "jucode"
            && !is_jucode_supported_model(&self.config.compact_model)
        {
            return vec![AgentEvent::Error(format!(
                "{} is not supported by JuCode CLI. Configure compact_model to a GPT or Claude model.",
                self.config.compact_model
            ))];
        }

        let Some(plan) = self
            .session
            .plan_compaction(COMPACTION_KEEP_RECENT_TOKENS, &self.config.model)
        else {
            return vec![AgentEvent::Info(
                "nothing old enough to compact".to_string(),
            )];
        };
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
        OpenAiClient::from_config(OpenAiClientConfig {
            model: self.config.compact_model.clone(),
            reasoning_effort: self.config.compact_reasoning_effort.clone(),
            system_prompt: String::new(),
            prompt_cache_key: self.session.session_id().to_string(),
            extensions: ExtensionRegistry::load(&[], &self.cwd, self.config.profile_dir()),
            base_url: self.config.base_url.clone(),
            max_output_tokens: self.config.compact_model_config().max_output_tokens,
            api_key: self.auth.key_for(&self.config.provider),
            api_key_env: &self.config.api_key_env,
            retry_attempts: self.config.retry_attempts,
            connect_timeout: Duration::from_secs(self.config.connect_timeout_seconds),
            read_timeout: Duration::from_secs(self.config.read_timeout_seconds),
            goal_tool_tx: None,
            approval_tx: None,
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
            .unwrap_or_else(|| self.config.compact_model.clone());
        let max_output_tokens = self
            .config
            .models
            .iter()
            .find(|entry| entry.name == model)
            .map(|entry| entry.max_output_tokens)
            .unwrap_or_else(|| self.config.compact_model_config().max_output_tokens);
        OpenAiClient::from_config(OpenAiClientConfig {
            model,
            reasoning_effort: self.config.compact_reasoning_effort.clone(),
            system_prompt: String::new(),
            prompt_cache_key: self.session.session_id().to_string(),
            extensions: ExtensionRegistry::load(&[], &self.cwd, self.config.profile_dir()),
            base_url: self.config.base_url.clone(),
            max_output_tokens,
            api_key: self.auth.key_for(&self.config.provider),
            api_key_env: &self.config.api_key_env,
            retry_attempts: self.config.retry_attempts,
            connect_timeout: Duration::from_secs(self.config.connect_timeout_seconds),
            read_timeout: Duration::from_secs(self.config.read_timeout_seconds),
            goal_tool_tx: None,
            approval_tx: None,
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
        self.turn_goal_tokens = 0;
        let save_event = self.save_session_event();

        if self.config.provider == "jucode" && !is_jucode_supported_model(&self.config.model) {
            let mut events = save_event;
            events.push(AgentEvent::Error(format!(
                "{} is not supported by JuCode CLI. Run /model and choose a GPT or Claude model.",
                self.config.model
            )));
            return events;
        }

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

    /// Drain pending tool-approval requests from the worker. Allowlisted tools
    /// are auto-approved; the rest surface an ApprovalRequest and park their
    /// responder until the client decides.
    fn poll_approval_requests(&mut self) -> Vec<AgentEvent> {
        let Some(rx) = self.approval_receiver.take() else {
            return Vec::new();
        };
        let mut events = Vec::new();
        while let Ok(request) = rx.try_recv() {
            if self.approved_tools.contains(&request.name) {
                let _ = request.response_tx.send(true);
                continue;
            }
            events.push(AgentEvent::ApprovalRequest {
                call_id: request.call_id.clone(),
                name: request.name.clone(),
                summary: request.summary.clone(),
            });
            self.pending_approvals
                .insert(request.call_id, (request.response_tx, request.name));
        }
        self.approval_receiver = Some(rx);
        events
    }

    /// Forward the client's allow/deny decision to the parked tool call. With
    /// `always`, the tool is added to the per-session allowlist.
    fn approve_events(&mut self, call_id: &str, allow: bool, always: bool) -> Vec<AgentEvent> {
        let Some((response_tx, name)) = self.pending_approvals.remove(call_id) else {
            return vec![AgentEvent::Error(
                "no pending approval for that call".to_string(),
            )];
        };
        if allow && always {
            self.approved_tools.insert(name);
        }
        let _ = response_tx.send(allow);
        Vec::new()
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
        let client = match self.resume_summary_client() {
            Ok(client) => client,
            Err(_) => return,
        };
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
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
            Err(error) => vec![AgentEvent::Error(format!(
                "failed to save session: {error}"
            ))],
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
        self.session = SessionStore::new();
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

    fn login_events(&mut self, arg: &str) -> Vec<AgentEvent> {
        if self.running {
            return vec![AgentEvent::Error(
                "cannot login while a response is running".to_string(),
            )];
        }
        let mut parts = arg.split_whitespace();
        let web_url = parts
            .next()
            .map(str::to_string)
            .unwrap_or_else(|| self.config.jucode_web_url.clone());
        let api_url = parts.next().map(str::to_string).unwrap_or_else(|| {
            if arg.is_empty() {
                self.config.jucode_api_url.clone()
            } else {
                web_url.clone()
            }
        });
        if self.login_receiver.is_some() {
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
        vec![AgentEvent::Info(format!(
            "opening browser for JuCode OAuth: {web_url}"
        ))]
    }

    fn apply_login_result(&mut self, result: OAuthLoginResult) -> Vec<AgentEvent> {
        let models = result
            .models
            .iter()
            .filter(|model| is_jucode_supported_model(&model.id))
            .cloned()
            .collect::<Vec<_>>();
        self.config.provider = "jucode".to_string();
        self.config.jucode_web_url = result.web_url.clone();
        self.config.jucode_api_url = result.api_url.clone();
        self.config.base_url = format!("{}/v1", result.api_url);
        for model in &models {
            let model_config = jucode_model_config(model);
            if let Some(existing) = self
                .config
                .models
                .iter_mut()
                .find(|entry| entry.name == model.id)
            {
                *existing = model_config;
            } else {
                self.config.models.push(model_config);
            }
        }
        if let Some(model) = models.first() {
            self.config.model = model.id.clone();
            let supported = self.reasoning_efforts_for_model(&model.id);
            if !supported
                .iter()
                .any(|effort| effort == &self.config.reasoning_effort)
            {
                self.config.reasoning_effort = self.default_reasoning_effort_for_model(&model.id);
            }
        }
        self.auth.set_key_for("jucode", result.api_key);
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
            return vec![AgentEvent::Info("no earlier turns to rewind to".to_string())];
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
        let (restored, removed) = match crate::tools::restore_to_timestamp(&self.cwd, t) {
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
                            label: summary.label,
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
        match SessionStore::load_for_cwd(&self.profile_dir, &self.cwd, session_id) {
            Ok(session) => {
                self.queued.clear();
                self.receiver = None;
                self.goal_tool_receiver = None;
                self.goal_continuation_running = false;
                self.turn_started_at = None;
                self.turn_goal_tokens = 0;
                self.session = session;
                vec![
                    AgentEvent::Transcript(self.session.transcript_items()),
                    AgentEvent::PendingMessages(Vec::new()),
                    self.model_status_event(),
                    self.context_usage_event(),
                    AgentEvent::Status(format!("resumed session {}", self.session.session_id())),
                ]
            }
            Err(error) => vec![AgentEvent::Error(format!(
                "failed to resume {session_id}: {error}"
            ))],
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
            if self.auth.key_for(&self.config.provider).is_some()
                || env::var_os(&self.config.api_key_env).is_some()
            {
                "ok"
            } else {
                "missing"
            }
        ));
        lines.push(format!("cwd: {}", self.cwd.display()));
        lines.push(format!("git: {}", command_ok("git", "--version")));
        lines.push(format!("rg: {}", command_ok("rg", "--version")));
        match discover_project_instructions(&self.cwd) {
            Ok(instructions) => lines.push(format!(
                "project instructions: {} file(s)",
                instructions.len()
            )),
            Err(error) => lines.push(format!("project instructions: error: {error}")),
        }
        if self.config.extensions.is_empty() {
            lines.push("extensions: none".to_string());
        } else {
            let extensions = ExtensionRegistry::load(
                &self.config.extensions,
                &self.cwd,
                self.config.profile_dir(),
            );
            lines.push(format!(
                "extensions: {} tool(s), {} error(s)",
                extensions.definitions().len(),
                extensions.errors().len()
            ));
            lines.extend(self.extension_info_lines());
        }
        vec![AgentEvent::Info(lines.join("\n"))]
    }

    fn extension_events(&self) -> Vec<AgentEvent> {
        self.extension_info_lines()
            .into_iter()
            .map(AgentEvent::Info)
            .collect()
    }

    fn extension_info_lines(&self) -> Vec<String> {
        if self.config.extensions.is_empty() {
            return vec!["extensions: none".to_string()];
        }

        let registry = ExtensionRegistry::load(
            &self.config.extensions,
            &self.cwd,
            self.config.profile_dir(),
        );
        let mut events = Vec::new();
        for extension in &self.config.extensions {
            let tools = registry
                .summaries()
                .into_iter()
                .filter(|summary| summary.extension == extension.name)
                .map(|summary| {
                    if summary.description.is_empty() {
                        summary.tool
                    } else {
                        format!("{} - {}", summary.tool, summary.description)
                    }
                })
                .collect::<Vec<_>>();
            let error = registry
                .errors()
                .iter()
                .find(|(name, _)| name == &extension.name)
                .map(|(_, error)| error);
            if let Some(error) = error {
                events.push(format!(
                    "extension {}: failed to initialize: {error}",
                    extension.name
                ));
            } else if tools.is_empty() {
                events.push(format!("extension {}: no tools", extension.name));
            } else {
                events.push(format!(
                    "extension {}: {}",
                    extension.name,
                    tools.join(", ")
                ));
            }
        }
        events
    }

    fn model_command_events(&mut self, args: Vec<&str>) -> Vec<AgentEvent> {
        match args.as_slice() {
            [] => vec![AgentEvent::ModelView {
                models: self.model_options(),
                active_effort: self.config.reasoning_effort.clone(),
            }],
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

    fn set_model_config(&mut self, model: String, reasoning_effort: String) -> Vec<AgentEvent> {
        if model.trim().is_empty() {
            return vec![AgentEvent::Error("model cannot be empty".to_string())];
        }
        if self.config.provider == "jucode" && !is_jucode_supported_model(&model) {
            return vec![AgentEvent::Error(format!(
                "{model} is not supported by JuCode CLI"
            ))];
        }
        if !self.is_reasoning_effort_for_model(&model, &reasoning_effort) {
            return vec![AgentEvent::Error(format!(
                "{model} does not support reasoning effort: {reasoning_effort}"
            ))];
        }

        self.config.model = model;
        self.config.reasoning_effort = reasoning_effort;
        match self.config.save() {
            Ok(()) => vec![self.model_status_event()],
            Err(error) => vec![AgentEvent::Error(format!("failed to save config: {error}"))],
        }
    }

    fn model_options(&self) -> Vec<ModelOptionView> {
        self.config
            .models
            .iter()
            .filter(|model_config| {
                self.config.provider != "jucode" || is_jucode_supported_model(&model_config.name)
            })
            .map(|model_config| {
                let active = model_config.name == self.config.model;
                ModelOptionView {
                    model: model_config.name.clone(),
                    active,
                    context_window: model_config.context_window,
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

fn current_utc_date() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() / 86_400)
        .unwrap_or(0);
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

fn is_jucode_supported_model(model: &str) -> bool {
    matches!(
        model,
        "gpt-5.5" | "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.3-codex" | "gpt-5.2"
    ) || model.starts_with("claude-")
}

fn jucode_model_config(model: &OAuthModel) -> ModelConfig {
    let (default_context_window, default_max_output_tokens, default_reasoning_efforts) =
        if model.id.starts_with("claude-") {
            (200_000, 8_192, vec!["none".to_string()])
        } else {
            (
                400_000,
                128_000,
                vec![
                    "none".to_string(),
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                    "xhigh".to_string(),
                ],
            )
        };
    ModelConfig {
        name: model.id.clone(),
        context_window: model.context_window.unwrap_or(default_context_window),
        max_output_tokens: model.max_output_tokens.unwrap_or(default_max_output_tokens),
        reasoning_efforts: model
            .reasoning_efforts
            .clone()
            .unwrap_or(default_reasoning_efforts),
        input_cost: 0.0,
        cached_input_cost: 0.0,
        output_cost: 0.0,
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

fn should_auto_compact(context_tokens: usize, model_context_budget: usize) -> bool {
    context_tokens > model_context_budget
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
