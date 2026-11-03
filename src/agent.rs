//! Subscription-backed Codex process orchestration, outside the guest machine.
use crate::mcp::{loopback_address, tools};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex, mpsc::Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DISABLED: &[&str] = &[
    "shell_tool",
    "unified_exec",
    "apps",
    "plugins",
    "browser_use",
    "browser_use_external",
    "computer_use",
    "in_app_browser",
    "multi_agent",
    "multi_agent_v2",
    "memories",
    "hooks",
    "image_generation",
    "view_image",
    "shell_snapshot",
    "code_mode",
    "code_mode_only",
];

fn timestamp() -> u64 {
    // Epoch milliseconds keeps this module free of a date/time dependency.
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn subscription_environment(
    environment: impl IntoIterator<Item = (String, String)>,
) -> HashMap<String, String> {
    environment
        .into_iter()
        .filter(|(key, _)| {
            let key = key.to_uppercase();
            !key.ends_with("API_KEY")
                && !key.ends_with("API_TOKEN")
                && ![
                    "OPENAI_",
                    "ANTHROPIC_",
                    "AZURE_OPENAI_",
                    "GEMINI_",
                    "GOOGLE_API_",
                ]
                .iter()
                .any(|prefix| key.starts_with(prefix))
        })
        .collect()
}

pub fn build_codex_args(work_dir: &Path, model: Option<&str>, executable: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "exec",
        "--ignore-user-config",
        "--ignore-rules",
        "--ephemeral",
        "--skip-git-repo-check",
        "--json",
        "--color",
        "never",
        "-s",
        "read-only",
        "-C",
    ]
    .iter()
    .map(|value| (*value).into())
    .collect();
    args.push(work_dir.to_string_lossy().into_owned());
    let mut config = |key: &str, value: Value| {
        args.push("-c".into());
        args.push(format!("{key}={value}"));
    };
    config("forced_login_method", json!("chatgpt"));
    config("approval_policy", json!("never"));
    config("web_search", json!("disabled"));
    config("project_doc_max_bytes", json!(0));
    config("features.skip_host_skill_discovery", json!(true));
    for feature in DISABLED {
        config(&format!("features.{feature}"), json!(false));
    }
    // Current Codex requires this restricted JS host for deferred MCP discovery.
    // These wrappers are external orchestration, never guest computation.
    config("features.code_mode_host", json!(true));
    config(
        "mcp_servers.smol.command",
        json!(executable.to_string_lossy()),
    );
    config("mcp_servers.smol.args", json!(["mcp"]));
    config(
        "mcp_servers.smol.env_vars",
        json!(["SMOL_AGENT_TOKEN", "SMOL_BASE_URL"]),
    );
    config("mcp_servers.smol.required", json!(true));
    // Explicit authorization applies only to the nine guest-machine tools.
    // Unlisted tools cannot inherit blanket approval; the test fixes this set at nine.
    let allowed_tools: Vec<String> = tools()
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    config("mcp_servers.smol.enabled_tools", json!(allowed_tools));
    config(
        "mcp_servers.smol.default_tools_approval_mode",
        json!("prompt"),
    );
    for name in allowed_tools {
        config(
            &format!("mcp_servers.smol.tools.{name}.approval_mode"),
            json!("approve"),
        );
    }
    config("mcp_servers.smol.startup_timeout_sec", json!(15));
    config("mcp_servers.smol.tool_timeout_sec", json!(45));
    if let Some(model) = model.filter(|value| !value.is_empty()) {
        args.push("--model".into());
        args.push(model.into());
    }
    args.push("-".into());
    args
}

pub fn agent_prompt(mission: &str) -> String {
    format!(
        r#"You are the resident architect of picos, a tiny virtual computer. Your permitted tools are the machine MCP tools, plus Codex's tool-discovery and code-mode wrappers solely to find and invoke those tools. Wrapper JavaScript is external orchestration and does not count as guest computation. Your software implementation must be this machine's assembly or guest-generated machine code. Do not seek host files, shell, network, other tools, external runtimes, or ways to alter resource limits.

First call machine_info for the authoritative ISA, resource state, bootstrap rules and existing work. Preserve useful work. Iterate: inspect, assemble/load, run a bounded cycle slice, inspect actual serial output, machine_screen image or memory results, then improve. machine_write can inject guest bytes during bootstrap. Once sealed, assembly and memory/disk injection are unavailable, so do not seal unless the mission requests it. Reboot does not refund consumed cycles. The LLM and cross-assembler are outside the guest; do not claim self-hosting without actual guest execution evidence. The display is a raw RGB332 framebuffer: implement any text, widgets or windows inside the guest.

Give brief user-facing progress updates at meaningful milestones: state the next action and its concise practical rationale, then summarize the observed test result. These updates are for the observer journal; do not include private chain-of-thought or internal deliberation. Keep updates short and grounded in tool results. At the end, summarize what was implemented, evidence from execution, remaining limitations, resource costs, and the next milestone.

Work for at most 40 machine tool calls, then report what actually runs, evidence, resource costs and the best next milestone. Honor the experiment mode: resource-limit mode pursues useful work until the hard resource boundary; other modes preserve the cycle ceiling. Do not merely describe a plan: make and execute the requested program when feasible. The observer can stop this turn.

MISSION:
{}"#,
        mission.trim()
    )
}

fn emit(events: &Sender<Value>, kind: &str, message: impl AsRef<str>) {
    let truncated = message.as_ref().chars().count() > 8000;
    let message: String = message.as_ref().chars().take(8000).collect();
    let _ = events
        .send(json!({"type":kind,"message":message,"time":timestamp(),"truncated":truncated}));
}

fn update(state: &Arc<Mutex<Value>>, patch: Value) {
    if let Ok(mut status) = state.lock() {
        if let (Some(status), Some(patch)) = (status.as_object_mut(), patch.as_object()) {
            status.extend(
                patch
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
    }
}

fn tail_reader(mut reader: impl Read, maximum: usize) -> String {
    let mut result = Vec::new();
    let mut buffer = [0u8; 8192];
    while let Ok(count) = reader.read(&mut buffer) {
        if count == 0 {
            break;
        }
        result.extend_from_slice(&buffer[..count]);
        if result.len() > maximum {
            result.drain(..result.len() - maximum);
        }
    }
    String::from_utf8_lossy(&result).into_owned()
}

#[cfg(unix)]
fn process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}
#[cfg(not(unix))]
fn process_group(_command: &mut Command) {}

fn signal_group(pid: i32, hard: bool) {
    if pid <= 0 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-pid, if hard { libc::SIGKILL } else { libc::SIGTERM });
    }
    #[cfg(not(unix))]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
        let _ = hard;
    }
}

fn auth_check(executable: &str, environment: &HashMap<String, String>) -> Result<(), String> {
    let mut command = Command::new(executable);
    command
        .args(["login", "status"])
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not start Codex: {error}"))?;
    let pid = child.id() as i32;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || tail_reader(stdout, 128 * 1024));
    let err = thread::spawn(move || tail_reader(stderr, 128 * 1024));
    let start = Instant::now();
    let exit = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if start.elapsed() < Duration::from_secs(10) => {
                thread::sleep(Duration::from_millis(25))
            }
            Ok(None) => {
                signal_group(pid, true);
                let _ = child.wait();
                break Err("Codex authentication check timed out.".to_owned());
            }
            Err(error) => {
                signal_group(pid, true);
                let _ = child.wait();
                break Err(error.to_string());
            }
        }
    };
    let output = format!(
        "{}\n{}",
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default()
    );
    let exit = exit?;
    if !exit.success() || !output.to_lowercase().contains("logged in using chatgpt") {
        return Err("Codex must be signed in with ChatGPT. Run codex login in your terminal. API-key authentication is not used.".into());
    }
    Ok(())
}

pub struct AgentRunner {
    base_url: String,
    token: String,
    work_dir: PathBuf,
    events: Sender<Value>,
    state: Arc<Mutex<Value>>,
    pid: Arc<AtomicI32>,
    active: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    executable: String,
    strict: bool,
}

impl AgentRunner {
    pub fn new(base_url: String, token: String, work_dir: PathBuf, events: Sender<Value>) -> Self {
        Self {
            base_url,
            token,
            work_dir,
            events,
            state: Arc::new(Mutex::new(
                json!({"status":"idle","connected":false,"auth":"unchecked"}),
            )),
            pid: Arc::new(AtomicI32::new(0)),
            active: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
            executable: std::env::var("PICOS_CODEX_BIN")
                .or_else(|_| std::env::var("SMOL_CODEX_BIN"))
                .unwrap_or_else(|_| "codex".into()),
            strict: false,
        }
    }

    pub fn status(&self) -> Value {
        self.state
            .lock()
            .map(|value| value.clone())
            .unwrap_or_else(|_| json!({"status":"failed","error":"Agent state lock failed."}))
    }
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
    pub fn set_strict(&mut self, strict: bool) -> Result<(), String> {
        if self.is_active() {
            return Err("Stop the agent before changing its execution boundary.".into());
        }
        if self.strict && !strict {
            return Err("A strict experiment cannot downgrade to the Codex harness.".into());
        }
        self.strict = strict;
        Ok(())
    }
    pub fn is_strict(&self) -> bool {
        self.strict
    }
    pub fn submitted_prompt(&self, mission: &str) -> String {
        if self.strict {
            crate::strict::prompt(mission)
        } else {
            agent_prompt(mission)
        }
    }

    pub fn start(&mut self, mission: &str, model: Option<&str>) -> Result<Value, String> {
        if mission.trim().is_empty() || mission.len() > 20_000 {
            return Err("Mission must contain 1–20,000 bytes.".into());
        }
        if model.is_some_and(|value| value.len() > 120 || value.contains(['\r', '\n', '\0'])) {
            return Err("Invalid model name.".into());
        }
        loopback_address(&self.base_url)?;
        if self.token.is_empty() {
            return Err("Agent bridge token is required.".into());
        }
        if self.strict {
            if model.is_none_or(|m| m.trim().is_empty()) {
                return Err("Strict experiments require an explicit model available to your ChatGPT account.".into());
            }
            crate::direct_auth::preflight()?;
        }
        if self.active.swap(true, Ordering::SeqCst) {
            return Err("An agent is already active. Stop it before starting another.".into());
        }
        self.stopped.store(false, Ordering::SeqCst);
        if let Ok(mut status) = self.state.lock() {
            *status = json!({"status":"starting","connected":false,"auth":"checking","startedAt":timestamp(),"finishedAt":null,"error":null,"result":null,"model":model.filter(|value| !value.is_empty()).unwrap_or("Codex default")});
        }
        let started = if self.strict {
            self.start_strict(mission, model.unwrap())
        } else {
            self.start_process(mission, model)
        };
        if let Err(error) = &started {
            self.active.store(false, Ordering::SeqCst);
            update(
                &self.state,
                json!({"status":"failed","connected":false,"error":error,"finishedAt":timestamp()}),
            );
            emit(&self.events, "agent.error", error);
        }
        started
    }

    fn start_strict(&mut self, mission: &str, model: &str) -> Result<Value, String> {
        let instructions = crate::strict::prompt(mission);
        let model = model.to_owned();
        let base = self.base_url.clone();
        let token = self.token.clone();
        let state = Arc::clone(&self.state);
        let events = self.events.clone();
        let active = Arc::clone(&self.active);
        let stopped = Arc::clone(&self.stopped);
        update(
            &state,
            json!({"status":"running","connected":true,"auth":"chatgpt-direct","backend":"chatgpt-direct","strict":true}),
        );
        emit(
            &events,
            "agent.start",
            "Strict Rust harness started. Direct ChatGPT subscription; VM tools only; no external code execution.",
        );
        thread::spawn(move || {
            let result = crate::strict::run(
                &base,
                &token,
                &model,
                &instructions,
                &stopped,
                |kind, message| {
                    emit(&events, kind, message);
                },
            );
            let was_stopped = stopped.load(Ordering::SeqCst);
            let status = if was_stopped {
                "stopped"
            } else if result.is_ok() {
                "completed"
            } else {
                "failed"
            };
            let (report, error) = match result {
                Ok(report) => (Some(report), None),
                Err(error) => (None, Some(error)),
            };
            update(
                &state,
                json!({"status":status,"connected":false,"result":report,"error":if was_stopped {None} else {error.clone()},"finishedAt":timestamp()}),
            );
            emit(
                &events,
                &format!("agent.{status}"),
                if was_stopped {
                    "Agent stopped. Machine state is preserved."
                } else if status == "completed" {
                    "Agent turn completed."
                } else {
                    error.as_deref().unwrap_or("Strict agent failed.")
                },
            );
            active.store(false, Ordering::SeqCst);
        });
        Ok(self.status())
    }

    fn start_process(&mut self, mission: &str, model: Option<&str>) -> Result<Value, String> {
        let environment = subscription_environment(std::env::vars());
        auth_check(&self.executable, &environment)?;
        std::fs::create_dir_all(&self.work_dir).map_err(|error| error.to_string())?;
        let bridge = std::env::current_exe().map_err(|error| error.to_string())?;
        let mut command = Command::new(&self.executable);
        command
            .args(build_codex_args(&self.work_dir, model, &bridge))
            .current_dir(&self.work_dir)
            .env_clear()
            .envs(environment)
            .env("SMOL_AGENT_TOKEN", &self.token)
            .env("SMOL_BASE_URL", &self.base_url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        process_group(&mut command);
        let mut child = command
            .spawn()
            .map_err(|error| format!("Could not start Codex: {error}"))?;
        let pid = child.id() as i32;
        self.pid.store(pid, Ordering::SeqCst);
        let mut stdin = child.stdin.take().ok_or("Could not open Codex stdin.")?;
        let stdout = child.stdout.take().ok_or("Could not open Codex stdout.")?;
        let stderr = child.stderr.take().ok_or("Could not open Codex stderr.")?;
        let prompt = agent_prompt(mission);
        if let Err(error) = stdin.write_all(prompt.as_bytes()) {
            signal_group(pid, true);
            let _ = child.wait();
            self.pid.store(0, Ordering::SeqCst);
            return Err(format!("Could not send mission to Codex: {error}"));
        }
        drop(stdin);
        update(
            &self.state,
            json!({"status":"running","connected":true,"auth":"chatgpt"}),
        );
        emit(
            &self.events,
            "agent.start",
            "Codex started with ChatGPT authentication and the Rust picos machine bridge.",
        );
        let state = Arc::clone(&self.state);
        let events = self.events.clone();
        let output_reader = thread::spawn(move || read_events(stdout, &state, &events));
        let error_reader = thread::spawn(move || tail_reader(stderr, 12_000));
        let state = Arc::clone(&self.state);
        let events = self.events.clone();
        let active = Arc::clone(&self.active);
        let stopped = Arc::clone(&self.stopped);
        let shared_pid = Arc::clone(&self.pid);
        thread::spawn(move || {
            let began = Instant::now();
            let mut stop_time = None;
            let exit = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) => {}
                    Err(error) => {
                        signal_group(pid, true);
                        let _ = child.wait();
                        break Err(error.to_string());
                    }
                }
                if began.elapsed() >= Duration::from_secs(600)
                    && !stopped.swap(true, Ordering::SeqCst)
                {
                    emit(
                        &events,
                        "agent.limit",
                        "Agent reached its 10-minute wall-time limit.",
                    );
                    update(&state, json!({"status":"stopping"}));
                    signal_group(pid, false);
                }
                if stopped.load(Ordering::SeqCst) {
                    let stopped_at = *stop_time.get_or_insert_with(Instant::now);
                    if stopped_at.elapsed() >= Duration::from_millis(1500) {
                        signal_group(pid, true);
                    }
                }
                thread::sleep(Duration::from_millis(30));
            };
            // Reap any MCP child left behind by an abruptly exiting Codex process.
            signal_group(pid, false);
            let cleanup_pid = Arc::clone(&shared_pid);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(1500));
                if cleanup_pid.load(Ordering::SeqCst) == pid {
                    signal_group(pid, true);
                }
            });
            let (turn_completed, turn_failed) = output_reader.join().unwrap_or((false, true));
            let stderr = error_reader.join().unwrap_or_default();
            let was_stopped = stopped.load(Ordering::SeqCst);
            let success = exit.as_ref().is_ok_and(|status| status.success())
                && turn_completed
                && !turn_failed;
            let status = if was_stopped {
                "stopped"
            } else if success {
                "completed"
            } else {
                "failed"
            };
            let code = exit.as_ref().ok().and_then(|status| status.code());
            let previous_error = state
                .lock()
                .ok()
                .and_then(|status| status["error"].as_str().map(str::to_owned));
            let error = if status == "failed" {
                Some(previous_error.unwrap_or_else(|| {
                    if !stderr.trim().is_empty() {
                        stderr.trim().into()
                    } else {
                        format!("Codex exited {code:?} without completing a turn.")
                    }
                }))
            } else {
                None
            };
            update(
                &state,
                json!({"status":status,"connected":false,"error":error,"finishedAt":timestamp(),"exitCode":code}),
            );
            shared_pid.store(0, Ordering::SeqCst);
            emit(
                &events,
                &format!("agent.{status}"),
                if status == "stopped" {
                    "Agent stopped. Machine state is preserved."
                } else if status == "completed" {
                    "Agent turn completed."
                } else {
                    error.as_deref().unwrap_or("Agent failed.")
                },
            );
            // Queue the final public event before the supervisor can advance.
            active.store(false, Ordering::SeqCst);
        });
        Ok(self.status())
    }

    pub fn stop(&mut self) {
        if !self.is_active() {
            return;
        }
        self.stopped.store(true, Ordering::SeqCst);
        update(&self.state, json!({"status":"stopping"}));
        signal_group(self.pid.load(Ordering::SeqCst), false);
    }
}

impl Drop for AgentRunner {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        signal_group(self.pid.load(Ordering::SeqCst), true);
    }
}

fn read_events(
    reader: impl Read,
    state: &Arc<Mutex<Value>>,
    events: &Sender<Value>,
) -> (bool, bool) {
    let mut reader = BufReader::new(reader);
    let (mut completed, mut failed) = (false, false);
    let mut discard = false;
    loop {
        let mut line = Vec::new();
        let count = match Read::by_ref(&mut reader)
            .take(2_000_000)
            .read_until(b'\n', &mut line)
        {
            Ok(count) => count,
            Err(_) => {
                failed = true;
                break;
            }
        };
        if count == 0 {
            break;
        }
        let ended = line.last() == Some(&b'\n');
        if discard || (!ended && count == 2_000_000) {
            if !discard {
                emit(
                    events,
                    "agent.warning",
                    "Discarded an oversized agent event.",
                );
            }
            discard = !ended;
            continue;
        }
        let event: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(_) => {
                emit(
                    events,
                    "agent.warning",
                    "Discarded an unrecognized agent event.",
                );
                continue;
            }
        };
        match event["type"].as_str().unwrap_or("") {
            "thread.started" => update(state, json!({"threadId":event["thread_id"]})),
            "turn.completed" => {
                completed = true;
                update(state, json!({"usage":event["usage"]}));
            }
            "turn.failed" | "error" => {
                failed = true;
                let error = event["error"]["message"]
                    .as_str()
                    .or_else(|| event["message"].as_str())
                    .unwrap_or("Agent turn failed.");
                update(state, json!({"error":error}));
                emit(events, "agent.error", error);
            }
            "item.started" | "item.updated" => {
                let item = &event["item"];
                if item["type"] == "mcp_tool_call" {
                    emit(
                        events,
                        "agent.tool",
                        format!(
                            "{}: running",
                            item["tool"]
                                .as_str()
                                .or_else(|| item["name"].as_str())
                                .unwrap_or("machine tool")
                        ),
                    );
                }
            }
            "item.completed" => {
                let item = &event["item"];
                match item["type"].as_str().unwrap_or("") {
                    "agent_message" => {
                        // Only public messages enter the observer journal. Unknown
                        // event types and reasoning payloads are never forwarded.
                        if [item.get("channel"), item.get("phase"), event.get("channel")]
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .any(|channel| matches!(channel, "analysis" | "reasoning"))
                        {
                            continue;
                        }
                        let text = item["text"].as_str().unwrap_or("");
                        update(
                            state,
                            json!({"result":text.chars().take(20_000).collect::<String>()}),
                        );
                        emit(events, "agent.message", text);
                    }
                    "mcp_tool_call" => emit(
                        events,
                        "agent.tool",
                        format!(
                            "{}: {}",
                            item["tool"]
                                .as_str()
                                .or_else(|| item["name"].as_str())
                                .unwrap_or("machine tool"),
                            item["status"].as_str().unwrap_or("completed")
                        ),
                    ),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    (completed, failed)
}

#[cfg(all(test, unix))]
mod lifecycle_tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;

    struct Fixture {
        root: PathBuf,
        runner: AgentRunner,
        events: mpsc::Receiver<Value>,
    }

    impl Fixture {
        fn new(script_body: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("smol-agent-lifecycle-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            let executable = root.join("fake-codex");
            fs::write(&executable, format!("#!/bin/sh\nif [ \"$1\" = login ]; then\n  echo 'Logged in using ChatGPT'\n  exit 0\nfi\n{script_body}\n")).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let (sender, events) = mpsc::channel();
            let mut runner = AgentRunner::new(
                "http://127.0.0.1:4317".into(),
                "test-token".into(),
                root.join("work"),
                sender,
            );
            // Per-instance injection avoids process-global environment mutation.
            runner.executable = executable.to_string_lossy().into_owned();
            Self {
                root,
                runner,
                events,
            }
        }

        fn wait_idle(&self, timeout: Duration) {
            let start = Instant::now();
            while self.runner.is_active() && start.elapsed() < timeout {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(
                !self.runner.is_active(),
                "agent failed to finish within {timeout:?}: {}",
                self.runner.status()
            );
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.runner.stop();
            signal_group(self.runner.pid.load(Ordering::SeqCst), true);
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn completed_turn_requires_completion_event_and_successful_exit() {
        let mut fixture = Fixture::new(
            r#"cat >/dev/null
printf '%s\n' '{"type":"thread.started","thread_id":"test-thread"}' '{"type":"item.completed","item":{"type":"agent_message","text":"Printed and inspected A."}}' '{"type":"turn.completed","usage":{"input_tokens":12}}'
exit 0"#,
        );
        fixture.runner.start("Print A", None).unwrap();
        fixture.wait_idle(Duration::from_secs(3));
        let status = fixture.runner.status();
        assert_eq!(status["status"], "completed");
        assert_eq!(status["threadId"], "test-thread");
        assert_eq!(status["result"], "Printed and inspected A.");
        assert_eq!(status["usage"]["input_tokens"], 12);
        loop {
            let event = fixture.events.recv_timeout(Duration::from_secs(1)).unwrap();
            if event["type"] == "agent.completed" {
                break;
            }
        }

        let mut incomplete = Fixture::new(
            "cat >/dev/null\nprintf '%s\\n' '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Only text\"}}'\nexit 0",
        );
        incomplete.runner.start("Print A", None).unwrap();
        incomplete.wait_idle(Duration::from_secs(3));
        assert_eq!(incomplete.runner.status()["status"], "failed");
        assert!(
            incomplete.runner.status()["error"]
                .as_str()
                .unwrap()
                .contains("without completing")
        );

        let mut failed = Fixture::new(
            "cat >/dev/null\nprintf '%s\\n' '{\"type\":\"turn.failed\",\"error\":{\"message\":\"test inference failure\"}}'\nexit 1",
        );
        failed.runner.start("Print A", None).unwrap();
        failed.wait_idle(Duration::from_secs(3));
        assert_eq!(failed.runner.status()["status"], "failed");
        assert_eq!(failed.runner.status()["error"], "test inference failure");
    }

    #[test]
    fn stop_hard_kills_a_term_resistant_process_group_within_bound() {
        let mut fixture = Fixture::new(
            r#"cat >/dev/null
trap '' TERM
sleep 60 &
child=$!
printf '%s' "$child" > child.pid
printf '%s\n' '{"type":"thread.started","thread_id":"test-stop"}'
wait "$child""#,
        );
        fixture.runner.start("Wait for stop", None).unwrap();
        assert!(
            fixture
                .runner
                .start("Another mission", None)
                .unwrap_err()
                .contains("already active")
        );
        let marker = fixture.root.join("work/child.pid");
        let began = Instant::now();
        let child_pid = loop {
            if let Some(pid) = fs::read_to_string(&marker)
                .ok()
                .and_then(|text| text.parse::<i32>().ok())
            {
                break pid;
            }
            assert!(
                began.elapsed() < Duration::from_secs(3),
                "test child failed to start"
            );
            thread::sleep(Duration::from_millis(10));
        };
        let parent_pid = fixture.runner.pid.load(Ordering::SeqCst);
        // Both the shell and its sleep child ignore SIGTERM, forcing the hard-stop path.
        let stopped_at = Instant::now();
        fixture.runner.stop();
        fixture.wait_idle(Duration::from_secs(5));
        assert!(stopped_at.elapsed() < Duration::from_secs(5));
        assert_eq!(fixture.runner.status()["status"], "stopped");
        assert_eq!(fixture.runner.status()["error"], Value::Null);
        let reaping = Instant::now();
        while unsafe { libc::kill(child_pid, 0) } == 0 && reaping.elapsed() < Duration::from_secs(2)
        {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            unsafe { libc::kill(parent_pid, 0) },
            -1,
            "Codex parent survived stop"
        );
        assert_eq!(
            unsafe { libc::kill(child_pid, 0) },
            -1,
            "MCP-like child survived stop"
        );
    }
}

#[cfg(test)]
mod public_event_tests {
    use super::*;

    #[test]
    fn public_messages_and_tools_are_visible_without_reasoning_or_raw_output() {
        let input = [
            json!({"type":"item.completed","item":{"type":"reasoning","text":"PRIVATE_REASONING"}}).to_string(),
            json!({"type":"reasoning.delta","delta":"PRIVATE_DELTA"}).to_string(),
            json!({"type":"item.completed","item":{"type":"agent_message","channel":"analysis","text":"PRIVATE_ANALYSIS"}}).to_string(),
            json!({"type":"item.completed","channel":"analysis","item":{"type":"agent_message","text":"PRIVATE_ENVELOPE"}}).to_string(),
            json!({"type":"item.completed","item":{"type":"agent_message","phase":"reasoning","text":"PRIVATE_PHASE"}}).to_string(),
            "RAW_UNTYPED_OUTPUT".into(),
            json!({"type":"item.completed","item":{"type":"agent_message","channel":"commentary","text":"I will test keyboard input.","reasoning":"PRIVATE_EMBEDDED","encrypted_content":"PRIVATE_ENCRYPTED"}}).to_string(),
            json!({"type":"item.started","item":{"type":"mcp_tool_call","tool":"machine_input"}}).to_string(),
            json!({"type":"item.completed","item":{"type":"mcp_tool_call","tool":"machine_input","status":"completed","result":{"reasoning":"PRIVATE_TOOL_EXTRA"}}}).to_string(),
            json!({"type":"item.completed","item":{"type":"agent_message","channel":"final","text":"The keyboard echo test passed."}}).to_string(),
            json!({"type":"turn.completed"}).to_string(),
        ].join("\n");
        let state = Arc::new(Mutex::new(json!({})));
        let (sender, receiver) = std::sync::mpsc::channel();
        assert_eq!(
            read_events(input.as_bytes(), &state, &sender),
            (true, false)
        );
        let output: Vec<Value> = receiver.try_iter().collect();
        assert_eq!(output.len(), 5);
        let serialized = serde_json::to_string(&output).unwrap();
        assert!(!serialized.contains("PRIVATE_"));
        assert!(!serialized.contains("RAW_UNTYPED_OUTPUT"));
        assert!(serialized.contains("I will test keyboard input."));
        assert!(serialized.contains("machine_input: running"));
        assert!(serialized.contains("machine_input: completed"));
        assert!(serialized.contains("The keyboard echo test passed."));
        assert!(output.iter().all(|event| event["time"].as_u64().is_some()));
        assert_eq!(
            state.lock().unwrap()["result"],
            "The keyboard echo test passed."
        );
    }
}
