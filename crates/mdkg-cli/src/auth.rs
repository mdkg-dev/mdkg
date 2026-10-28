//! `mdkg auth`: the API key fallback's helper (mdkg_run::auth).
//!
//! `auth key <provider>` prints the fallback's key from the macOS Keychain
//! for Claude Code's `apiKeyHelper`, which runs it when a turn bills the
//! key. It refuses a terminal, so the key isn't printed for a person by
//! accident, and runs without a graph. Marked agent turns are refused;
//! the launcher's key helper clears that mark for its own invocation.
//! This is a semantic guardrail, not isolation (docs/agents.md).

use std::io::{IsTerminal as _, Write as _};

use clap::Subcommand;
use mdkg_run::auth::KeySource;

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Print the API key the fallback bills, for a harness's key helper
    /// (`anthropic` or `openai`); refuses a terminal.
    #[command(after_help = examples!("auth key anthropic"))]
    Key {
        /// `anthropic` (Claude) or `openai` (Codex).
        provider: String,
    },
}

impl AuthCommand {
    pub fn name(&self) -> &'static str {
        match self {
            AuthCommand::Key { .. } => "auth.key",
        }
    }
}

pub fn run(c: &AuthCommand) -> u8 {
    if std::env::var_os(mdkg_run::profile::TURN_MARK).is_some_and(|mark| !mark.is_empty()) {
        eprintln!("error: auth is refused in an agent turn");
        return mdkg_graph::ErrorKind::Usage.exit_code();
    }
    match c {
        AuthCommand::Key { provider } => {
            let service = match provider.as_str() {
                "anthropic" => "mdkg-anthropic",
                "openai" => "mdkg-openai",
                _ => {
                    eprintln!("error: no provider {provider:?}: anthropic or openai");
                    return 1;
                }
            };
            if std::io::stdout().is_terminal() {
                eprintln!(
                    "error: auth key prints a secret for a key helper, never to a terminal \
                     (check the Keychain item {service} with `security find-generic-password -s \
                     {service}`)"
                );
                return 1;
            }
            match KeySource::default().read(service) {
                Ok(key) => {
                    let mut out = std::io::stdout().lock();
                    if writeln!(out, "{}", key.expose())
                        .and_then(|()| out.flush())
                        .is_err()
                    {
                        // Not bad arguments: a failure (the exit-code contract).
                        return mdkg_graph::ErrorKind::Failure.exit_code();
                    }
                    0
                }
                Err(e) => {
                    eprintln!("error: {}", e.message);
                    if let Some(h) = e.hint {
                        eprintln!("hint: {h}");
                    }
                    mdkg_graph::ErrorKind::Failure.exit_code()
                }
            }
        }
    }
}
