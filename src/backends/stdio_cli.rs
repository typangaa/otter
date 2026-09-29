//! Stdio-CLI backend: spawns a local command in oneshot mode and captures its
//! stdout as the model response.
//!
//! Argument templates: the following tokens in the `args` list are replaced at
//! call time:
//! - `{prompt}` — the user prompt text (always substituted)
//! - `{model}`  — the model name from `ChatRequest.model`; if the model is
//!   absent the arg containing `{model}` **and the immediately preceding arg**
//!   (typically the flag, e.g. `-m`) are both dropped from the final arg list.

use async_trait::async_trait;
use std::process::Stdio;
use tokio::process::Command;
use tracing::debug;

use crate::config::{BackendConfig, BackendKind};
use crate::error::{Result, WeirError};

use super::{Backend, ChatRequest, ChatResponse};

// ── Backend struct ────────────────────────────────────────────────────────────

pub struct StdioCliBackend {
    name: String,
    command: String,
    /// Argument template list; `{prompt}` and `{model}` are substituted at call time.
    args_template: Vec<String>,
    /// Fallback model used when `ChatRequest.model` is `None`. If both are absent,
    /// any `{model}` placeholder (and its preceding flag) is dropped from the args.
    default_model: Option<String>,
    /// Per-request timeout (seconds) for the spawned command; a hung subprocess
    /// is killed once this elapses (see `kill_on_drop` in `chat`).
    timeout_secs: u64,
}

impl StdioCliBackend {
    pub fn new(cfg: &BackendConfig) -> Result<Self> {
        // `stdio-cli` is the only backend kind, so this destructure is irrefutable.
        let BackendKind::StdioCli { command, args } = &cfg.kind;
        let (command, args) = (command.clone(), args.clone());

        Ok(Self {
            name: cfg.name.clone(),
            command,
            args_template: args,
            default_model: cfg.default_model.clone(),
            timeout_secs: cfg.timeout_secs,
        })
    }

    /// Substitute `{prompt}` and `{model}` in the argument template list.
    ///
    /// If `model` is `None`, any arg whose template is exactly `{model}` (or
    /// contains it as the only dynamic part) resolves to an empty string; that
    /// arg **and the immediately preceding arg** (the flag, e.g. `-m`) are
    /// both removed from the final list.
    fn build_args(&self, prompt_text: &str, model: Option<&str>) -> Vec<String> {
        let model_str = model.unwrap_or("");
        let mut result: Vec<String> = Vec::with_capacity(self.args_template.len());

        for tmpl in &self.args_template {
            // Single pass: scan the template once and substitute placeholders.
            // Substituted text (e.g. the user prompt) is never re-scanned, so a
            // literal "{model}" inside the prompt is passed through unchanged.
            let mut expanded = String::with_capacity(tmpl.len());
            let mut rest = tmpl.as_str();
            loop {
                let next = match (rest.find("{prompt}"), rest.find("{model}")) {
                    (Some(p), Some(m)) if p < m => Some((p, "{prompt}", prompt_text)),
                    (Some(p), None) => Some((p, "{prompt}", prompt_text)),
                    (_, Some(m)) => Some((m, "{model}", model_str)),
                    (None, None) => None,
                };
                let Some((idx, token, value)) = next else {
                    break;
                };
                expanded.push_str(&rest[..idx]);
                expanded.push_str(value);
                rest = &rest[idx + token.len()..];
            }
            expanded.push_str(rest);

            if expanded.is_empty() && tmpl.contains("{model}") {
                // Drop the preceding flag (e.g. "-m") together with this arg.
                result.pop();
            } else {
                result.push(expanded);
            }
        }
        result
    }

    /// Debug-safe version of `build_args`: truncates long prompt values.
    fn debug_args(&self, prompt_text: &str, model: Option<&str>) -> Vec<String> {
        // Truncate by chars, never bytes: byte slicing panics mid multi-byte
        // character (CJK, Vietnamese, emoji).
        let display_prompt: String = if prompt_text.chars().count() > 200 {
            let head: String = prompt_text.chars().take(200).collect();
            format!("{}…[{} chars]", head, prompt_text.chars().count())
        } else {
            prompt_text.to_string()
        };
        self.build_args(&display_prompt, model)
    }
}

// ── Backend trait ─────────────────────────────────────────────────────────────

#[async_trait]
impl Backend for StdioCliBackend {
    fn name(&self) -> &str {
        &self.name
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse> {
        // Use the content of the last user message as the prompt.
        let prompt = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.as_str())
            .unwrap_or("");

        let model = req.model.as_deref().or(self.default_model.as_deref());
        let args = self.build_args(prompt, model);

        // Only build the (possibly large) debug string when DEBUG is enabled.
        let debug_args = if tracing::enabled!(tracing::Level::DEBUG) {
            Some(self.debug_args(prompt, model))
        } else {
            None
        };
        debug!(
            backend  = %self.name,
            command  = %self.command,
            model    = ?model,
            args     = ?debug_args,
            "spawning cli for chat"
        );

        let mut cmd = Command::new(&self.command);
        cmd.args(&args)
            .stdin(Stdio::null()) // child must never inherit our stdin pipe (else it hangs)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true); // ensure the child is killed if we time out / drop

        let child = cmd.spawn().map_err(|e| {
            WeirError::Backend(format!(
                "backend '{}': failed to spawn '{}': {e}",
                self.name, self.command
            ))
        })?;

        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return Err(WeirError::Backend(format!(
                    "backend '{}': '{}' failed while waiting for output: {e}",
                    self.name, self.command
                )));
            }
            Err(_elapsed) => {
                // The wait future (owning the child) is dropped here; kill_on_drop
                // sends SIGKILL. Return a retryable Backend error so the breaker/retry
                // layer sees a genuine failure.
                return Err(WeirError::Backend(format!(
                    "backend '{}': '{}' timed out after {}s",
                    self.name, self.command, self.timeout_secs
                )));
            }
        };

        debug!(
            backend = %self.name,
            exit_code = ?output.status.code(),
            stdout_bytes = output.stdout.len(),
            stderr_bytes = output.stderr.len(),
            "cli exited"
        );

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(WeirError::Backend(format!(
                "backend '{}': '{}' exited with {}: {}",
                self.name,
                self.command,
                output.status,
                stderr.trim()
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

        Ok(ChatResponse {
            content: stdout,
            backend_name: self.name.clone(),
            model: None,
            usage: None,
        })
    }

    async fn health(&self) -> Result<()> {
        debug!(
            backend = %self.name,
            command = %self.command,
            "health check: probing with --version"
        );

        // Try --version first; if the process spawns at all, we consider it
        // healthy (some CLIs may exit non-zero for --version, which is still
        // a sign the binary exists and is executable).
        let result = Command::new(&self.command)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .await;

        match result {
            Ok(output) => {
                debug!(
                    backend = %self.name,
                    exit_code = ?output.status.code(),
                    "health check: process spawned successfully"
                );
                Ok(())
            }
            Err(e) => Err(WeirError::Backend(format!(
                "backend '{}': health check failed — could not spawn '{}': {e}",
                self.name, self.command
            ))),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::ChatMessage;
    use crate::config::BackendConfig;

    #[tokio::test]
    async fn chat_times_out_on_slow_command() {
        let cfg = BackendConfig {
            name: "slow".to_string(),
            kind: BackendKind::StdioCli {
                command: "sleep".to_string(),
                args: vec!["5".to_string()],
            },
            timeout_secs: 1,
            default_model: None,
            retry_attempts: None,
            failure_threshold: None,
            recovery_secs: None,
            rate_limit_rps: None,
        };
        let backend = StdioCliBackend::new(&cfg).unwrap();
        let req = ChatRequest {
            messages: vec![ChatMessage::user("hi")],
            max_tokens: None,
            temperature: None,
            model: None,
        };
        let err = backend.chat(req).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "got: {err}");
    }

    fn make_backend(args: Vec<&str>) -> StdioCliBackend {
        let cfg = BackendConfig {
            name: "test".to_string(),
            kind: BackendKind::StdioCli {
                command: "true".to_string(),
                args: args.into_iter().map(String::from).collect(),
            },
            timeout_secs: 5,
            default_model: None,
            retry_attempts: None,
            failure_threshold: None,
            recovery_secs: None,
            rate_limit_rps: None,
        };
        StdioCliBackend::new(&cfg).unwrap()
    }

    #[test]
    fn debug_args_truncates_cjk_prompt_by_chars_without_panicking() {
        let backend = make_backend(vec!["{prompt}"]);
        let cjk: String = std::iter::repeat_n('世', 300).collect();
        let args = backend.debug_args(&cjk, None);
        let out = &args[0];
        assert!(out.contains('…'), "expected ellipsis, got: {out}");
        // The prompt portion is at most 200 chars plus the ellipsis/marker.
        let head = out.split('…').next().unwrap();
        assert_eq!(head.chars().count(), 200);
        assert!(head.chars().all(|c| c == '世'));
    }

    #[test]
    fn build_args_passes_literal_model_token_in_prompt_through() {
        let backend = make_backend(vec!["-m", "{model}", "{prompt}"]);
        let prompt = "explain the {model} placeholder";

        let with_model = backend.build_args(prompt, Some("gpt-x"));
        assert_eq!(
            with_model,
            vec![
                "-m".to_string(),
                "gpt-x".to_string(),
                "explain the {model} placeholder".to_string()
            ]
        );

        // With model None, the {model} arg and its preceding flag are dropped,
        // but the literal token inside the prompt survives.
        let without_model = backend.build_args(prompt, None);
        assert_eq!(
            without_model,
            vec!["explain the {model} placeholder".to_string()]
        );
    }

    #[test]
    fn build_args_normal_prompts_unchanged() {
        let backend = make_backend(vec!["-m", "{model}", "--input", "{prompt}"]);
        let args = backend.build_args("hello world", Some("m1"));
        assert_eq!(
            args,
            vec![
                "-m".to_string(),
                "m1".to_string(),
                "--input".to_string(),
                "hello world".to_string()
            ]
        );

        let args = backend.build_args("hello world", None);
        assert_eq!(args, vec!["--input".to_string(), "hello world".to_string()]);
    }

    #[test]
    fn build_args_model_before_prompt_in_one_arg() {
        let backend = make_backend(vec!["{model}:{prompt}:{model}"]);
        let args = backend.build_args("a{model}b", Some("m1"));
        assert_eq!(args, vec!["m1:a{model}b:m1".to_string()]);
    }
}
