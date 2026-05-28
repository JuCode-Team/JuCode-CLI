use crate::{
    config::{profile_dir, AuthStore, Config, ModelConfig},
    event::{AgentEvent, CommandView, ModelOptionView, SessionListItemView},
    extensions::ExtensionRegistry,
    llm::{OpenAiClient, OpenAiClientConfig, StreamEvent},
    oauth,
    prompt::{
        build_system_prompt, discover_project_instructions, discover_skills, skill_commands,
        skill_message, PromptContext,
    },
    session::{EntryKind, SessionStore, SessionSummary},
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    env, io,
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug)]
enum WorkerEvent {
    CallStart,
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
        is_error: bool,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
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
    queued: VecDeque<String>,
    running: bool,
    receiver: Option<Receiver<WorkerEvent>>,
}

impl AgentCore {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            config: Config::load_or_create()?,
            auth: AuthStore::load_or_create()?,
            session: SessionStore::new(),
            profile_dir: profile_dir()?,
            cwd: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            queued: VecDeque::new(),
            running: false,
            receiver: None,
        })
    }

    pub fn startup_events(&self) -> Vec<AgentEvent> {
        vec![
            AgentEvent::Startup {
                version: env!("CARGO_PKG_VERSION").to_string(),
                profile_dir: self.config.profile_dir().display().to_string(),
                config_path: self.config.path().display().to_string(),
            },
            self.model_status_event(),
            self.command_list_event(),
        ]
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
            max_output_tokens: model_config.max_output_tokens,
            reasoning_efforts: model_config.reasoning_efforts,
            state,
        }
    }

    fn command_list_event(&self) -> AgentEvent {
        let mut commands = [
            "/help",
            "/login",
            "/new",
            "/config",
            "/model",
            "/tree",
            "/fork",
            "/checkout",
            "/delete",
            "/resume",
            "/extensions",
            "/context",
            "/quit",
        ]
        .iter()
        .map(|command| CommandView {
            command: (*command).to_string(),
            marker: None,
        })
        .collect::<Vec<_>>();
        if let Ok(skill_commands) = skill_commands(self.config.profile_dir(), &self.cwd) {
            commands.extend(skill_commands.into_iter().map(|entry| CommandView {
                command: entry.command,
                marker: Some("SKILL".to_string()),
            }));
        }
        AgentEvent::CommandList(commands)
    }

    pub fn submit_user_message(&mut self, message: String) -> Vec<AgentEvent> {
        if self.running {
            self.queued.push_back(message);
            return vec![
                AgentEvent::PendingMessages(self.queued.iter().cloned().collect()),
                AgentEvent::Status(format!("queued: {}", self.queued.len())),
            ];
        }
        let mut events = vec![AgentEvent::UserMessage(message.clone())];
        events.extend(self.start_turn(message));
        events
    }

    pub fn steer(&mut self) -> Vec<AgentEvent> {
        if !self.running || self.queued.is_empty() {
            return Vec::new();
        }

        self.receiver = None;
        self.running = false;
        let Some(next) = self.queued.pop_front() else {
            return Vec::new();
        };
        let mut events = vec![
            AgentEvent::Status("steering".to_string()),
            AgentEvent::PendingMessages(self.queued.iter().cloned().collect()),
            AgentEvent::UserMessage(next.clone()),
        ];
        events.extend(self.start_turn(next));
        events
    }

    pub fn handle_command(&mut self, input: &str) -> (bool, Vec<AgentEvent>) {
        let mut parts = input.split_whitespace();
        let command = parts.next().unwrap_or_default();
        if let Some(events) = self.skill_command_events(command, input[command.len()..].trim()) {
            return (false, events);
        }

        let events = match command {
            "/quit" | "/exit" => return (true, Vec::new()),
            "/help" | "/" => vec![AgentEvent::Info(
                "/help /login [web-url] [api-url] /new /config /model [model] [effort] /tree /fork <label> /checkout [label] /delete <label> /resume [session-id] /extensions /context /quit"
                    .to_string(),
            )],
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
            "/checkout" => {
                let label = input[command.len()..].trim();
                if label.is_empty() {
                    vec![AgentEvent::TreeView(self.session.tree_view())]
                } else {
                    match self.session.checkout(label) {
                    Ok(()) => {
                        let save_event = self.save_session_event();
                        vec![
                            AgentEvent::Transcript(self.session.transcript_items()),
                            AgentEvent::Status(format!("checked out {label}")),
                        ]
                        .into_iter()
                        .chain(save_event)
                        .collect()
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
            "/extensions" => self.extension_events(),
            "/context" => self
                .context_events(),
            _ => vec![AgentEvent::Error(format!("unknown command: {command}"))],
        };
        (false, events)
    }

    fn skill_command_events(&mut self, command: &str, request: &str) -> Option<Vec<AgentEvent>> {
        let commands = skill_commands(self.config.profile_dir(), &self.cwd).ok()?;
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
            self.queued.push_back(message);
            return Some(vec![
                AgentEvent::PendingMessages(self.queued.iter().cloned().collect()),
                AgentEvent::Status(format!("queued: {}", self.queued.len())),
            ]);
        }
        let display = if request.is_empty() {
            command.to_string()
        } else {
            format!("{command} {request}")
        };
        let mut events = vec![AgentEvent::UserMessage(display)];
        events.extend(self.start_turn(message));
        Some(events)
    }

    pub fn poll_events(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let mut disconnected = false;

        if let Some(rx) = self.receiver.take() {
            while let Ok(event) = rx.try_recv() {
                match event {
                    WorkerEvent::CallStart => events.push(AgentEvent::ThinkingStart),
                    WorkerEvent::Delta(delta) => events.push(AgentEvent::AssistantDelta(delta)),
                    WorkerEvent::Retrying { attempt } => {
                        events.push(AgentEvent::Retrying { attempt });
                    }
                    WorkerEvent::ResponseItem(item) => {
                        self.session.append(EntryKind::ResponseItem { item });
                        events.extend(self.save_session_event());
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
                        is_error,
                    } => {
                        self.session.append(EntryKind::ToolOutput {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            output: output.clone(),
                        });
                        events.extend(self.save_session_event());
                        events.push(AgentEvent::ToolOutput {
                            call_id,
                            name,
                            output,
                            is_error,
                        });
                    }
                    WorkerEvent::Usage {
                        input_tokens,
                        output_tokens,
                    } => events.push(AgentEvent::Usage {
                        input_tokens,
                        output_tokens,
                    }),
                    WorkerEvent::Done => {
                        self.running = false;
                        disconnected = true;
                        if !self.queued.is_empty() {
                            events.push(AgentEvent::PendingMessages(
                                self.queued.iter().cloned().collect(),
                            ));
                        }
                        events.push(AgentEvent::Status(if self.queued.is_empty() {
                            "ready".to_string()
                        } else {
                            format!("queued: {}", self.queued.len())
                        }));
                    }
                    WorkerEvent::Error(error) => {
                        self.running = false;
                        disconnected = true;
                        events.push(AgentEvent::Error(error));
                    }
                }
            }
            if !disconnected {
                self.receiver = Some(rx);
            }
        }

        if !self.running {
            if let Some(next) = self.queued.pop_front() {
                events.push(AgentEvent::PendingMessages(
                    self.queued.iter().cloned().collect(),
                ));
                events.push(AgentEvent::UserMessage(next.clone()));
                events.extend(self.start_turn(next));
            }
        }

        events
    }

    fn start_turn(&mut self, message: String) -> Vec<AgentEvent> {
        self.session.append(EntryKind::User { content: message });
        let save_event = self.save_session_event();

        if self.config.provider != "openai" && self.config.provider != "jucode" {
            let mut events = save_event;
            events.push(AgentEvent::Error(format!(
                "unsupported provider '{}'. MVP supports openai.",
                self.config.provider
            )));
            return events;
        }

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
        let skills = match discover_skills(self.config.profile_dir(), &self.cwd) {
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
                    "edit",
                    "write",
                    "bash",
                    "apply_patch",
                    "diff",
                    "ls",
                    "ripgrep",
                ],
                project_instructions,
                skills,
            },
        );

        let Ok(client) = OpenAiClient::from_config(OpenAiClientConfig {
            model: self.config.model.clone(),
            reasoning_effort: self.config.reasoning_effort.clone(),
            system_prompt,
            extensions: ExtensionRegistry::load(
                &self.config.extensions,
                &self.cwd,
                self.config.profile_dir(),
            ),
            base_url: self.config.base_url.clone(),
            api_key: self.auth.key_for(&self.config.provider),
            api_key_env: &self.config.api_key_env,
            retry_attempts: self.config.retry_attempts,
        }) else {
            let mut events = save_event;
            events.push(AgentEvent::Error(
                "missing API key in auth.json or env".to_string(),
            ));
            return events;
        };

        let input = self.session.context_projection().items;
        let cwd = self.cwd.clone();
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        self.running = true;

        thread::spawn(move || {
            let result = client.run_turn_events(input, &cwd, |event| {
                let mapped = match event {
                    StreamEvent::CallStart => WorkerEvent::CallStart,
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
                        is_error,
                    } => WorkerEvent::ToolOutput {
                        call_id,
                        name,
                        output,
                        is_error,
                    },
                    StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                    } => WorkerEvent::Usage {
                        input_tokens,
                        output_tokens,
                    },
                };
                tx.send(mapped).map_err(|error| error.to_string())
            });

            match result {
                Ok(()) => {
                    let _ = tx.send(WorkerEvent::Done);
                }
                Err(error) => {
                    let _ = tx.send(WorkerEvent::Error(error));
                }
            }
        });

        let mut events = save_event;
        events.extend([
            AgentEvent::AssistantStart,
            AgentEvent::Status("streaming".to_string()),
        ]);
        events
    }

    fn save_session_event(&mut self) -> Vec<AgentEvent> {
        match self.session.save_for_cwd(&self.profile_dir, &self.cwd) {
            Ok(()) => Vec::new(),
            Err(error) => vec![AgentEvent::Error(format!(
                "failed to save session: {error}"
            ))],
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
        let mut events = vec![AgentEvent::Info(format!(
            "opening browser for JuCode OAuth: {web_url}"
        ))];
        match oauth::login(&web_url, &api_url) {
            Ok(result) => {
                self.config.provider = "jucode".to_string();
                self.config.jucode_web_url = result.web_url.clone();
                self.config.jucode_api_url = result.api_url.clone();
                self.config.base_url = format!("{}/v1", result.api_url);
                for model in &result.models {
                    if !self.config.models.iter().any(|entry| entry.name == *model) {
                        self.config.models.push(ModelConfig {
                            name: model.clone(),
                            context_window: 400_000,
                            max_output_tokens: 128_000,
                            reasoning_efforts: vec![
                                "none".to_string(),
                                "low".to_string(),
                                "medium".to_string(),
                                "high".to_string(),
                                "xhigh".to_string(),
                            ],
                        });
                    }
                }
                if let Some(model) = result.models.first() {
                    self.config.model = model.clone();
                    let supported = self.reasoning_efforts_for_model(model);
                    if !supported
                        .iter()
                        .any(|effort| effort == &self.config.reasoning_effort)
                    {
                        self.config.reasoning_effort =
                            self.default_reasoning_effort_for_model(model);
                    }
                }
                self.auth.set_key_for("jucode", result.api_key);
                match self.auth.save().and_then(|_| self.config.save()) {
                    Ok(()) => {
                        events.push(AgentEvent::Info(
                            "JuCode account connected; provider switched to jucode".to_string(),
                        ));
                        events.push(self.model_status_event());
                    }
                    Err(error) => {
                        events.push(AgentEvent::Error(format!("failed to save login: {error}")))
                    }
                }
            }
            Err(error) => events.push(AgentEvent::Error(format!("JuCode login failed: {error}"))),
        }
        events
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
                        SessionListItemView {
                            active,
                            label: format_session_summary(summary),
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
        match SessionStore::load_for_cwd(&self.profile_dir, &self.cwd, session_id) {
            Ok(session) => {
                self.session = session;
                vec![
                    AgentEvent::Transcript(self.session.transcript_items()),
                    AgentEvent::Status(format!("resumed session {}", self.session.session_id())),
                ]
            }
            Err(error) => vec![AgentEvent::Error(format!(
                "failed to resume {session_id}: {error}"
            ))],
        }
    }

    fn context_events(&self) -> Vec<AgentEvent> {
        let projection = self.session.context_projection();
        let mut events = vec![AgentEvent::Info(format!(
            "context projection: branch_entries={} projected_entries={}",
            projection.branch_entries, projection.projected_entries
        ))];
        events.extend(
            projection
                .items
                .into_iter()
                .map(|item| AgentEvent::Info(item.to_string())),
        );
        events
    }

    fn extension_events(&self) -> Vec<AgentEvent> {
        if self.config.extensions.is_empty() {
            return vec![AgentEvent::Info("extensions: none".to_string())];
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
                events.push(AgentEvent::Info(format!(
                    "extension {}: failed to initialize: {error}",
                    extension.name
                )));
            } else if tools.is_empty() {
                events.push(AgentEvent::Info(format!(
                    "extension {}: no tools",
                    extension.name
                )));
            } else {
                events.push(AgentEvent::Info(format!(
                    "extension {}: {}",
                    extension.name,
                    tools.join(", ")
                )));
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

fn format_session_summary(summary: SessionSummary) -> String {
    format!(
        "{} | entries {} | leaf {} | updated {}",
        summary.id, summary.entries, summary.leaf, summary.updated_at
    )
}
