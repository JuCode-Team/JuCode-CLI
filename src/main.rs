use jucode_agent_core::{AgentCore, AgentEvent};
use jucode_tui::{TuiApp, TuiRuntime};
use std::io;

struct Runtime(AgentCore);

impl TuiRuntime for Runtime {
    fn startup_events(&self) -> Vec<AgentEvent> {
        self.0.startup_events()
    }

    fn model_status_event(&self) -> AgentEvent {
        self.0.model_status_event()
    }

    fn submit_user_message(&mut self, message: String) -> Vec<AgentEvent> {
        self.0.submit_user_message(message)
    }

    fn steer(&mut self) -> Vec<AgentEvent> {
        self.0.steer()
    }

    fn handle_command(&mut self, input: &str) -> (bool, Vec<AgentEvent>) {
        self.0.handle_command(input)
    }

    fn poll_events(&mut self) -> Vec<AgentEvent> {
        self.0.poll_events()
    }
}

fn main() -> io::Result<()> {
    TuiApp::new(Runtime(AgentCore::new()?)).run()
}
