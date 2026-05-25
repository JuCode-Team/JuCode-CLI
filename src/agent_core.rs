use crate::{
    config::{profile_dir, AuthStore, Config},
    event::AgentEvent,
    llm::{OpenAiClient, StreamEvent},
    session::{EntryKind, SessionStore, SessionSummary},
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    env, io,
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    thread,
};

#[derive(Debug)]
enum WorkerEvent {
    CallStart,
    Delta(String),
    ResponseItem(Value),
    ToolStart {
        name: String,
    },
    ToolOutput {
        call_id: String,
        name: String,
        output: String,
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

        AgentEvent::ModelStatus {
            provider: self.config.provider.clone(),
            model: self.config.model.clone(),
            state,
        }
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

        let events = match command {
            "/quit" | "/exit" => return (true, Vec::new()),
            "/help" | "/" => vec![AgentEvent::Info(
                "/help /config /tree /switch <id|root> /branch [id|root] /resume [session-id] /context /quit"
                    .to_string(),
            )],
            "/config" => vec![AgentEvent::Info(format!(
                "provider={} model={} base_url={} auth_key={} api_key_env={}",
                self.config.provider,
                self.config.model,
                self.config.base_url,
                mask_key(self.auth.key_for(&self.config.provider)),
                self.config.api_key_env
            ))],
            "/tree" => vec![AgentEvent::TreeView(self.session.tree_view())],
            "/switch" => {
                let target = parts.next().unwrap_or_default();
                let id = if target == "root" || target == "none" {
                    None
                } else {
                    match SessionStore::parse_id(target) {
                        Some(id) => Some(id),
                        None => {
                            return (
                                false,
                                vec![AgentEvent::Error(format!(
                                    "usage: {command} <entry-id|root>"
                                ))],
                            );
                        }
                    }
                };

                match self.session.switch_to(id) {
                    Ok(()) => {
                        let save_event = self.save_session_event();
                        vec![
                            AgentEvent::Transcript(self.session.transcript_items()),
                            AgentEvent::Status(format!(
                                "active leaf: {}",
                                format_leaf(self.session.leaf_id())
                            )),
                        ]
                        .into_iter()
                        .chain(save_event)
                        .collect()
                    }
                    Err(error) => vec![AgentEvent::Error(error)],
                }
            }
            "/branch" => {
                let target = parts.next();
                let id = match target {
                    None => self.session.leaf_id(),
                    Some("root" | "none") => None,
                    Some(target) => match SessionStore::parse_id(target) {
                        Some(id) => Some(id),
                        None => {
                            return (
                                false,
                                vec![AgentEvent::Error(
                                    "usage: /branch [entry-id|root]".to_string(),
                                )],
                            );
                        }
                    },
                };

                match self.session.branch_from(id) {
                    Ok(id) => {
                        let save_event = self.save_session_event();
                        vec![
                            AgentEvent::Transcript(self.session.transcript_items()),
                            AgentEvent::Status(format!("created branch: {}", id.display())),
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
            "/context" => self
                .session
                .context_items()
                .into_iter()
                .map(|item| AgentEvent::Info(item.to_string()))
                .collect(),
            _ => vec![AgentEvent::Error(format!("unknown command: {command}"))],
        };
        (false, events)
    }

    pub fn poll_events(&mut self) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let mut disconnected = false;

        if let Some(rx) = self.receiver.take() {
            while let Ok(event) = rx.try_recv() {
                match event {
                    WorkerEvent::CallStart => events.push(AgentEvent::ThinkingStart),
                    WorkerEvent::Delta(delta) => events.push(AgentEvent::AssistantDelta(delta)),
                    WorkerEvent::ResponseItem(item) => {
                        self.session.append(EntryKind::ResponseItem { item });
                        events.extend(self.save_session_event());
                    }
                    WorkerEvent::ToolStart { name } => {
                        events.push(AgentEvent::ToolStart { name });
                    }
                    WorkerEvent::ToolOutput {
                        call_id,
                        name,
                        output,
                    } => {
                        self.session.append(EntryKind::ToolOutput {
                            call_id: call_id.clone(),
                            name: name.clone(),
                            output: output.clone(),
                        });
                        events.extend(self.save_session_event());
                        events.push(AgentEvent::ToolOutput { name, output });
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

        if self.config.provider != "openai" {
            let mut events = save_event;
            events.push(AgentEvent::Error(format!(
                "unsupported provider '{}'. MVP supports openai.",
                self.config.provider
            )));
            return events;
        }

        let Ok(client) = OpenAiClient::from_config(
            self.config.model.clone(),
            self.config.base_url.clone(),
            self.auth.key_for(&self.config.provider),
            &self.config.api_key_env,
        ) else {
            let mut events = save_event;
            events.push(AgentEvent::Error(
                "missing API key in auth.json or env".to_string(),
            ));
            return events;
        };

        let input = self.session.context_items();
        let cwd = self.cwd.clone();
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        self.running = true;

        thread::spawn(move || {
            let result = client.run_turn_events(input, &cwd, |event| {
                let mapped = match event {
                    StreamEvent::CallStart => WorkerEvent::CallStart,
                    StreamEvent::Delta(delta) => WorkerEvent::Delta(delta),
                    StreamEvent::ResponseItem(item) => WorkerEvent::ResponseItem(item),
                    StreamEvent::ToolStart { name } => WorkerEvent::ToolStart { name },
                    StreamEvent::ToolOutput {
                        call_id,
                        name,
                        output,
                    } => WorkerEvent::ToolOutput {
                        call_id,
                        name,
                        output,
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

    fn resume_list_events(&self) -> Vec<AgentEvent> {
        match SessionStore::list_for_cwd(&self.profile_dir, &self.cwd) {
            Ok(sessions) if sessions.is_empty() => {
                vec![AgentEvent::Info(
                    "no sessions for current directory".to_string(),
                )]
            }
            Ok(sessions) => {
                let mut lines = vec![format!("sessions for {}", self.cwd.display())];
                lines.extend(sessions.into_iter().map(format_session_summary));
                vec![AgentEvent::Info(lines.join("\n"))]
            }
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

fn format_leaf(id: Option<crate::session::EntryId>) -> String {
    id.map(crate::session::EntryId::display)
        .unwrap_or_else(|| "root".to_string())
}

fn format_session_summary(summary: SessionSummary) -> String {
    format!(
        "{} | entries {} | leaf {} | updated {}",
        summary.id, summary.entries, summary.leaf, summary.updated_at
    )
}
