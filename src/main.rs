mod agent_core;
mod config;
mod event;
mod llm;
mod session;
mod tools;
mod tui;

use crate::{
    agent_core::AgentCore,
    event::AgentEvent,
    tui::{TuiApp, TuiRuntime},
};
use std::io;

impl TuiRuntime for AgentCore {
    fn startup_events(&self) -> Vec<AgentEvent> {
        AgentCore::startup_events(self)
    }

    fn model_status_event(&self) -> AgentEvent {
        AgentCore::model_status_event(self)
    }

    fn submit_user_message(&mut self, message: String) -> Vec<AgentEvent> {
        AgentCore::submit_user_message(self, message)
    }

    fn steer(&mut self) -> Vec<AgentEvent> {
        AgentCore::steer(self)
    }

    fn handle_command(&mut self, input: &str) -> (bool, Vec<AgentEvent>) {
        AgentCore::handle_command(self, input)
    }

    fn poll_events(&mut self) -> Vec<AgentEvent> {
        AgentCore::poll_events(self)
    }
}

fn main() -> io::Result<()> {
    TuiApp::new(AgentCore::new()?).run()
}
