pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;
pub mod pi;

use crate::model::{Agent, ParsedSession};
use anyhow::Result;
use std::path::Path;

pub trait Parser {
    fn agent(&self) -> Agent;
    fn discover(&self, root: &Path) -> Result<Vec<ParsedSession>>;
}

pub fn parser(agent: Agent) -> Box<dyn Parser> {
    match agent {
        Agent::Claude => Box::new(claude::ClaudeParser),
        Agent::Codex => Box::new(codex::CodexParser),
        Agent::OpenCode => Box::new(opencode::OpenCodeParser),
        Agent::Gemini => Box::new(gemini::GeminiParser),
        Agent::Pi => Box::new(pi::PiParser),
    }
}
