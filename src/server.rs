use crate::{
    agent::AgentRunner,
    assembler::assemble,
    journal::Journal,
    session::{AgentSession, Progress, RunMode},
    vm::{Config, MAX_DISK_BYTES, Machine},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use uuid::Uuid;

const SPEC: &str = include_str!("../docs/SPEC.md");
const ISA: &str = include_str!("../docs/ISA.md");
const BLANK: &str = "; A blank machine. What will you build?\n; Programs, data and pixels share the same RAM.\nhalt\n";
const MAX_BODY: usize = 512 * 1024;
const MAX_TOOLS: u64 = 240;
const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;
static STOP: AtomicBool = AtomicBool::new(false);
#[cfg(unix)]
extern "C" fn shutdown_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
fn token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
fn number(
    value: Option<&Value>,
    default: u64,
    min: u64,
    max: u64,
    name: &str,
) -> Result<u64, String> {
    let value = match value {
        Some(v) => v
            .as_u64()
            .ok_or_else(|| format!("{name} must be a nonnegative integer"))?,
        None => default,
    };
    if value < min || value > max {
        return Err(format!("{name} must be {min}..{max}"));
    }
    Ok(value)
}
fn text<'a>(body: &'a Value, key: &str) -> Result<&'a str, String> {
    body.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} must be text"))
}
fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

pub struct Lab {
    machine: Machine,
    source: String,
    experiment: String,
    restored_from: Option<String>,
    events: Vec<Value>,
    running: bool,
    throughput: f64,
    data_dir: PathBuf,
    agent: AgentRunner,
    agent_token: String,
    operator_token: String,
    last_report: Option<String>,
    journal: Journal,
    journal_session: String,
    event_tx: mpsc::Sender<Value>,
    event_rx: mpsc::Receiver<Value>,
    base_url: String,
    tool_calls: u64,
    dirty: bool,
    last_save: Instant,
    details: Option<Value>,
    read_only: bool,
    terminal_events: bool,
    session: Option<AgentSession>,
    memory_revision: u64,
    memory_events: Vec<Value>,
}

impl Lab {
    pub fn new(data_dir: PathBuf, base_url: String) -> Result<Self, String> {
        Self::with_config(data_dir, base_url, None)
    }
    fn with_config(
        data_dir: PathBuf,
        base_url: String,
        config: Option<Config>,
    ) -> Result<Self, String> {
        fs::create_dir_all(data_dir.join("checkpoints")).map_err(|e| e.to_string())?;
        let data_dir = fs::canonicalize(data_dir).map_err(|e| e.to_string())?;
        let (event_tx, event_rx) = mpsc::channel();
        let agent_token = token();
        let agent = AgentRunner::new(
            base_url.clone(),
            agent_token.clone(),
            data_dir.join("agent-work"),
            event_tx.clone(),
        );
        let mut machine = Machine::new(config.unwrap_or_default())?;
        let code = assemble(BLANK, &machine.config)?;
        machine.load(&code.bytes)?;
        machine.take_memory_changes();
        let mut lab = Self {
            machine,
            source: BLANK.into(),
            experiment: Uuid::new_v4().to_string(),
            restored_from: None,
            events: vec![],
            running: false,
            throughput: 0.0,
            data_dir,
            agent,
            agent_token,
            operator_token: token(),
            last_report: None,
            journal: Journal::default(),
            journal_session: Uuid::new_v4().to_string(),
            event_tx,
            event_rx,
            base_url,
            tool_calls: 0,
            dirty: true,
            last_save: Instant::now(),
            details: None,
            read_only: false,
            terminal_events: false,
            session: None,
            memory_revision: 0,
            memory_events: vec![],
        };
        let latest = lab.latest();
        if latest.exists() {
            lab.restore(&latest, false).map_err(|e|format!("Cannot restore {}: {e}. The file has not been overwritten; move it aside to start fresh.",latest.display()))?;
            lab.record("system", "Restored the last saved Rust machine.");
        } else {
            lab.record("system", "A blank Rust machine is ready.");
        }
        let metadata = lab.data_dir.join("experiment.json");
        if metadata.exists() {
            lab.details = Some(
                serde_json::from_slice(&fs::read(metadata).map_err(|e| e.to_string())?)
                    .map_err(|e| format!("Invalid experiment metadata: {e}"))?,
            );
        }
        if lab.details.as_ref().is_some_and(|v| v["strict"] == true) {
            lab.agent.set_strict(true)?;
        }
        Ok(lab)
    }
    fn latest(&self) -> PathBuf {
        self.data_dir.join("latest.json.gz")
    }
    fn checkpoint_path(&self, name: &str) -> PathBuf {
        self.data_dir
            .join("checkpoints")
            .join(format!("{name}.json.gz"))
    }
    fn record(&mut self, kind: &str, message: &str) {
        if self.terminal_events
            && (kind.starts_with("agent.") || kind == "tool-error" || kind == "error")
        {
            println!(
                "  {kind}  {}",
                message
                    .chars()
                    .map(|c| if c.is_control() && c != '\n' && c != '\t' {
                        ' '
                    } else {
                        c
                    })
                    .take(1600)
                    .collect::<String>()
            );
        }
        self.record_detail(kind, message, None);
    }
    fn record_detail(&mut self, kind: &str, message: &str, details: Option<Value>) {
        let turn = self
            .session
            .as_ref()
            .map(|s| s.state()["turn"].clone())
            .unwrap_or(Value::Null);
        let event = json!({"time":now_ms(), "experiment":self.experiment, "type":kind, "message":message.chars().take(8000).collect::<String>(),"truncated":message.chars().count()>8000,"turn":turn,"sessionId":self.journal_session,"details":details});
        self.journal
            .record(event.clone(), Some(&self.data_dir.join("journal.jsonl")));
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.data_dir.join("ledger.jsonl"))
        {
            Ok(mut file) => {
                if let Err(e) = writeln!(file, "{event}") {
                    eprintln!("Ledger write failed: {e}");
                }
            }
            Err(e) => eprintln!("Ledger open failed: {e}"),
        }
        self.events.push(event);
        if self.events.len() > 160 {
            self.events.remove(0);
        }
        self.dirty = true;
    }
    pub fn state(&self) -> Value {
        let mut agent = self.agent.status();
        agent["toolCalls"] = json!(self.tool_calls);
        agent["maxToolCalls"] = json!(self.tool_limit());
        agent["session"] = self
            .session
            .as_ref()
            .map(AgentSession::state)
            .unwrap_or(Value::Null);
        json!({"machine":self.machine.state(), "source":self.source, "events":self.events, "running":self.running,"strict":self.agent.is_strict(),"agentBackend":if self.agent.is_strict(){"chatgpt-direct"}else{"codex"},
            "checkpoints":self.checkpoints(), "metrics":{"hostInstructionsPerSecond":self.throughput},
            "experiment":self.experiment, "experimentDetails":self.details, "observerOnly":self.read_only, "restoredFrom":self.restored_from, "config":self.machine.config, "agent":agent, "implementation":"Rust", "operatorToken":self.operator_token, "sessionControls":{"canContinue":!self.agent.is_active() && self.session.as_ref().is_some_and(AgentSession::can_continue),"canChangeMode":!self.agent.is_active()}, "memoryActivity":{"revision":self.memory_revision,"events":self.memory_events}})
    }
    fn checkpoints(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.data_dir.join("checkpoints"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|s| s.strip_suffix(".json.gz"))
                    .map(str::to_owned)
            })
            .collect();
        names.sort();
        names
    }
    fn snapshot(&self) -> Value {
        json!({"version":1,"implementation":"Rust","strict":self.agent.is_strict(),"experiment":self.experiment,"restoredFrom":self.restored_from,"source":self.source,"session":self.session,"toolCalls":self.tool_calls,"lastReport":self.last_report,"journal":self.journal,"journalSession":self.journal_session,"memoryActivity":{"revision":self.memory_revision,"events":self.memory_events},"events":self.events.iter().rev().take(80).cloned().collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>(),"machine":self.machine.export_state()})
    }
    fn encoded_snapshot(&self) -> Result<Vec<u8>, String> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        serde_json::to_writer(&mut encoder, &self.snapshot()).map_err(|e| e.to_string())?;
        encoder.finish().map_err(|e| e.to_string())
    }
    pub fn save(&mut self) -> Result<(), String> {
        self.save_to(&self.latest())?;
        self.dirty = false;
        self.last_save = Instant::now();
        Ok(())
    }
    fn save_to(&self, file: &Path) -> Result<(), String> {
        let bytes = self.encoded_snapshot()?;
        let temporary = file.with_extension("gz.tmp");
        let mut output = fs::File::create(&temporary).map_err(|e| e.to_string())?;
        output.write_all(&bytes).map_err(|e| e.to_string())?;
        output.sync_all().map_err(|e| e.to_string())?;
        fs::rename(temporary, file).map_err(|e| e.to_string())
    }
    fn restore(&mut self, file: &Path, branch: bool) -> Result<(), String> {
        let input = fs::File::open(file).map_err(|e| e.to_string())?;
        let mut decoded = Vec::new();
        GzDecoder::new(input)
            .take(MAX_SNAPSHOT_BYTES + 1)
            .read_to_end(&mut decoded)
            .map_err(|e| e.to_string())?;
        if decoded.len() as u64 > MAX_SNAPSHOT_BYTES {
            return Err("Checkpoint is too large".into());
        }
        let value: Value = serde_json::from_slice(&decoded).map_err(|e| e.to_string())?;
        if value["version"] != 1 {
            return Err("Unsupported checkpoint version".into());
        }
        let strict = match value.get("strict") {
            None => false, // Legacy checkpoints predate the execution boundary.
            Some(Value::Bool(strict)) => *strict,
            Some(_) => return Err("Invalid checkpoint execution boundary".into()),
        };
        let source = text(&value, "source")?;
        if source.len() > 256 * 1024 {
            return Err("Checkpoint source is too large".into());
        }
        let machine = Machine::from_state(&value["machine"])?;
        let experiment = if branch {
            Uuid::new_v4().to_string()
        } else {
            text(&value, "experiment")?.into()
        };
        self.session = if value["session"].is_null() {
            None
        } else {
            Some(
                serde_json::from_value(value["session"].clone())
                    .map_err(|e| format!("Invalid session checkpoint: {e}"))?,
            )
        };
        if let Some(session) = self.session.as_mut() {
            session.validate()?;
            session.pause_on_restore();
        }
        self.tool_calls = value["toolCalls"].as_u64().unwrap_or(0);
        self.last_report = value["lastReport"].as_str().map(str::to_owned);
        self.machine = machine;
        self.memory_revision = value["memoryActivity"]["revision"].as_u64().unwrap_or(0);
        self.memory_events = value["memoryActivity"]["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .rev()
            .take(24)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        self.source = source.into();
        self.experiment = experiment;
        self.events = value["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .rev()
            .take(80)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        self.journal_session = value["journalSession"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        self.journal = if value["journal"].is_null() {
            let mut journal = Journal::default();
            for event in &self.events {
                let mut event = event.clone();
                event["turn"] = Value::Null;
                event["sessionId"] = json!("legacy");
                event["imported"] = json!(true);
                journal.record(event, None);
            }
            journal
        } else {
            serde_json::from_value(value["journal"].clone())
                .map_err(|e| format!("Invalid public journal: {e}"))?
        };
        self.journal.restore_bounds();
        self.restored_from = if branch {
            file.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json.gz"))
                .map(str::to_owned)
        } else {
            value["restoredFrom"].as_str().map(str::to_owned)
        };
        // The checkpoint carries its own boundary when moved without metadata.
        // Restoring older or non-strict work cannot relax an existing boundary.
        if strict && !self.agent.is_strict() {
            self.agent.set_strict(true)?;
        }
        self.running = false;
        self.throughput = 0.0;
        self.dirty = true;
        Ok(())
    }
    fn capture_memory(&mut self, kind: &str, label: &str) {
        let changes = self.machine.take_memory_changes();
        if changes.changed_bytes == 0 {
            return;
        }
        self.memory_revision += 1;
        self.memory_events.push(json!({"id":self.memory_revision,"time":now_ms(),"kind":kind,"label":label,"changedBytes":changes.changed_bytes,"ranges":changes.ranges,"truncated":changes.truncated}));
        if self.memory_events.len() > 24 {
            self.memory_events.remove(0);
        }
        self.dirty = true;
    }
    fn progress(&self) -> Progress {
        Progress {
            memory_revision: self.memory_revision,
            cycles: self.machine.cycles,
            disk_touched_bytes: self.machine.state()["diskTouchedBytes"]
                .as_u64()
                .unwrap_or(0),
        }
    }
    fn start_session(
        &mut self,
        mission: &str,
        model: Option<&str>,
        max_turns: u64,
        mode: RunMode,
        tell_turns: bool,
    ) -> Result<Value, String> {
        if self.agent.is_active() {
            return Err("An agent is already active. Stop it before starting another.".into());
        }
        // Inactive is published only after the worker has queued all its events.
        // Retain that tail under the old session identity before replacing it.
        self.drain_agent_events();
        self.tool_calls = 0;
        self.last_report = None;
        self.journal_session = Uuid::new_v4().to_string();
        self.session = Some(AgentSession::new_with_policy(
            mission.into(),
            model.map(str::to_owned),
            max_turns,
            mode,
            tell_turns,
        )?);
        self.start_next_turn(None)
    }
    fn start_next_turn(&mut self, previous_report: Option<&str>) -> Result<Value, String> {
        self.enforce_limits(None);
        if self
            .session
            .as_ref()
            .is_some_and(|session| session.state()["status"] != "waiting")
        {
            return Err(
                "Session cannot start: its resource or tool allowance is exhausted.".into(),
            );
        }
        let progress = self.progress();
        let session = self.session.as_mut().ok_or("No agent session")?;
        let prompt = session.prompt_for_next_turn(previous_report.or(self.last_report.as_deref()));
        let model = session.model.clone();
        session.begin_turn(progress)?;
        let session_state = session.state();
        self.record(
            "agent.turn",
            &format!(
                "Starting turn {} of {}. Inspect, build, test, improve.",
                session_state["turn"], session_state["maxTurns"]
            ),
        );
        let result = self.agent.start(&prompt, model.as_deref());
        if result.is_ok() {
            self.journal.prompt(json!({"sessionId":self.journal_session,"turn":session_state["turn"],"time":now_ms(),"model":model,"backend":if self.agent.is_strict(){"chatgpt-direct"}else{"codex"},"prompt":self.agent.submitted_prompt(&prompt),"opening":if self.agent.is_strict(){Some(crate::strict::OPENING)}else{None}}),&self.data_dir.join("journal.jsonl"));
            self.dirty = true;
        }
        if let Err(e) = &result {
            self.session.as_mut().unwrap().stop("failed", e);
        }
        result
    }
    fn tool_limit(&self) -> Option<u64> {
        self.session
            .as_ref()
            .map_or(Some(MAX_TOOLS), AgentSession::tool_limit)
    }
    fn stop_resource(&mut self, reason: &str) {
        if let Some(session) = self.session.as_mut() {
            session.stop_resource(reason);
        }
        self.running = false;
        self.agent.stop();
        self.record("agent.limit", reason);
    }
    fn enforce_limits(&mut self, error: Option<&str>) {
        let resource_mode = self
            .session
            .as_ref()
            .is_some_and(|s| s.mode() == RunMode::ResourceLimit);
        let limit = self
            .machine
            .resource_limit()
            .or_else(|| error.and_then(crate::vm::resource_limit_error));
        if let Some(limit) = limit.filter(|limit| *limit == "cycles" || resource_mode) {
            self.stop_resource(&format!(
                "{limit} resource boundary reached. Agent stopped immediately; machine preserved."
            ));
        } else if self
            .tool_limit()
            .is_some_and(|limit| self.tool_calls >= limit)
        {
            if let Some(session) = self.session.as_mut() {
                session.stop_tool_limit();
            }
            self.agent.stop();
            self.record("agent.limit", "Tool allowance reached; agent stopped.");
        }
    }
    fn advance_session(&mut self) {
        if self.agent.is_active() {
            return;
        }
        // The worker may have finished between tick's initial drain and this
        // inactive check. No worker can enqueue more events after publishing
        // inactive, and only this server thread can start the next turn.
        self.drain_agent_events();
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let status = session.state()["status"].as_str().unwrap_or("").to_owned();
        if status == "waiting" {
            if let Err(e) = self.start_next_turn(None) {
                self.record("agent.error", &e);
            }
            return;
        }
        if status != "running" {
            return;
        }
        let agent = self.agent.status();
        if let Some(report) = agent["result"].as_str() {
            self.last_report = Some(report.chars().take(6000).collect());
        }
        let progress = self.progress();
        let tool_limit = self.tool_limit().unwrap_or(u64::MAX);
        let again = self.session.as_mut().unwrap().finish_turn(
            agent["status"].as_str().unwrap_or("failed"),
            progress,
            self.tool_calls,
            tool_limit,
            self.machine.config.max_cycles,
        );
        if again {
            if let Err(e) = self.start_next_turn(None) {
                self.record("agent.error", &e);
            }
        } else {
            let reason = self.session.as_ref().unwrap().state()["reason"]
                .as_str()
                .unwrap_or("Session ended")
                .to_owned();
            self.record("agent.session", &reason);
        }
    }
    fn session_control(&mut self, path: &str, body: &Value) -> Result<Value, String> {
        if self.session.is_none() {
            return Err("No configured experiment session. Start one from the CLI.".into());
        }
        match path {
            "/api/session/settings" => {
                let mode = body
                    .get("mode")
                    .map(|v| {
                        v.as_str()
                            .ok_or("mode must be text")
                            .and_then(|s| s.parse::<RunMode>().map_err(|_| "Unknown mode"))
                    })
                    .transpose()?;
                let tell = body
                    .get("tellTurns")
                    .map(|v| v.as_bool().ok_or("tellTurns must be a boolean"))
                    .transpose()?;
                if mode.is_some() && self.agent.is_active() {
                    return Err("Stop or finish the active turn before changing mode.".into());
                }
                let session = self.session.as_mut().unwrap();
                if let Some(mode) = mode {
                    session.set_mode(mode);
                }
                if let Some(tell) = tell {
                    session.set_tell_turns(tell);
                }
                self.record(
                    "operator.settings",
                    "Experiment policy updated; future turns use these settings.",
                );
            }
            "/api/session/turns" => {
                if self.agent.status()["status"] == "stopping" {
                    return Err("The agent is stopping; try again when it has stopped.".into());
                }
                let count = number(body.get("turns"), 1, 1, 24, "turns")?;
                self.session.as_mut().unwrap().add_turns(count)?;
                self.record(
                    "operator.turns",
                    &format!(
                        "Granted {count} additional turns. Guest resource limits are unchanged."
                    ),
                );
            }
            "/api/session/continue" => {
                if self.agent.is_active() {
                    return Err("Agent is already running.".into());
                }
                self.session.as_mut().unwrap().request_continue()?;
                self.record(
                    "operator.continue",
                    "Operator requested continuation with the existing allowance.",
                );
            }
            "/api/session/stop" => {
                self.session.as_mut().unwrap().stop(
                    "stopped",
                    "Stopped by operator; resources and work preserved.",
                );
                self.agent.stop();
                self.record("operator.stop", "Agent stop requested.");
            }
            _ => return Err("Unknown session control".into()),
        }
        self.save()?;
        Ok(self.state())
    }
    fn execute(&mut self, cycles: u64) -> Result<Value, String> {
        let before = self.machine.instructions;
        let started = Instant::now();
        let result = self.machine.run(cycles);
        self.capture_memory("run", "Guest execution changed RAM");
        let result = result?;
        self.throughput = (self.machine.instructions - before) as f64
            / started.elapsed().as_secs_f64().max(0.000001);
        if ["halted", "fault", "budget"].contains(&self.machine.status.as_str()) {
            self.running = false;
        }
        self.dirty = true;
        Ok(result)
    }
    fn compile(&mut self, source: &str) -> Result<Value, String> {
        if source.len() > 256 * 1024 {
            return Err("Assembly source exceeds 256 KiB".into());
        }
        let result = assemble(source, &self.machine.config)?;
        self.machine.load(&result.bytes)?;
        self.capture_memory(
            "assemble",
            &format!("Loaded {} bytes of assembled code", result.bytes.len()),
        );
        self.source = source.into();
        self.running = false;
        self.record(
            "assemble",
            &format!(
                "Loaded {} bytes into guest RAM and disk.",
                result.bytes.len()
            ),
        );
        Ok(json!({"bytes":result.bytes.len(),"symbols":result.symbols}))
    }
    fn inspect(&self, args: &Value) -> Result<Value, String> {
        let space = args["space"].as_str().unwrap_or("ram");
        let address = number(args.get("address"), 0, 0, u32::MAX as u64, "address")? as usize;
        let length = number(args.get("length"), 128, 1, 4096, "length")? as usize;
        let bytes = match space {
            "ram" => self.machine.read_memory(address, length)?,
            "disk" => self.machine.read_disk(address, length)?,
            _ => return Err("space must be ram or disk".into()),
        };
        Ok(json!({"space":space,"address":address,"bytes":bytes}))
    }
    fn seal(&mut self) -> Value {
        self.machine.sealed = true;
        self.record(
            "seal",
            "Machine sealed. External assembly and RAM/disk injection are disabled.",
        );
        self.machine.state()
    }
    fn clear_agent_history(&mut self) {
        let strict = self.agent.is_strict();
        self.session = None;
        self.last_report = None;
        self.agent_token = token();
        self.tool_calls = 0;
        self.agent = AgentRunner::new(
            self.base_url.clone(),
            self.agent_token.clone(),
            self.data_dir.join("agent-work"),
            self.event_tx.clone(),
        );
        self.agent
            .set_strict(strict)
            .expect("fresh runner accepts execution boundary");
        // The old turn has ended before operator branching is allowed. Its
        // remaining display events belong to the previous experiment.
        while self.event_rx.try_recv().is_ok() {}
    }
    fn tool(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        // Authoritative validation also applies to the direct harness; schemas
        // and model obedience are never the resource/security boundary.
        crate::mcp::validate_arguments(name, args)?;
        match name {
            "machine_info" => {
                let mut info = json!({"machine":self.machine.state(),"source":self.source,"experiment":self.experiment,"restoredFrom":self.restored_from,"toolCalls":self.tool_calls,"strict":self.agent.is_strict(),"executionBoundary":if self.agent.is_strict(){"Direct model connection; only fixed VM functions; no external code runtime. Cross-assembler and byte injection are bootstrap infrastructure until sealed."}else{"Codex harness; external orchestration permitted."},"specification":format!("{SPEC}\n\n{ISA}")});
                if self.session.as_ref().is_none_or(AgentSession::tell_turns) {
                    info["remainingToolCalls"] = json!(
                        self.tool_limit()
                            .map(|limit| limit.saturating_sub(self.tool_calls))
                    );
                }
                Ok(info)
            }
            "machine_screen" => {
                Ok(json!({"width":256,"height":192,"pixels":STANDARD.encode(self.frame())}))
            }
            "machine_assemble" => self.compile(text(args, "source")?),
            "machine_run" => {
                self.running = false;
                self.execute(number(args.get("cycles"), 100000, 1, 2000000, "cycles")?)
            }
            "machine_inspect" => self.inspect(args),
            "machine_write" => {
                let address =
                    number(args.get("address"), 0, 0, u32::MAX as u64, "address")? as usize;
                let values = args["bytes"].as_array().ok_or("bytes must be an array")?;
                if values.is_empty() || values.len() > 4096 {
                    return Err("Write requires 1..4096 bytes".into());
                }
                let bytes: Vec<u8> = values
                    .iter()
                    .map(|v| {
                        v.as_u64()
                            .filter(|v| *v <= 255)
                            .map(|v| v as u8)
                            .ok_or("Invalid byte".to_string())
                    })
                    .collect::<Result<_, _>>()?;
                match text(args, "space")? {
                    "ram" => self.machine.write_memory(address, &bytes)?,
                    "disk" => self.machine.write_disk(address, &bytes)?,
                    _ => return Err("space must be ram or disk".into()),
                }
                self.capture_memory("write", "Agent wrote guest RAM");
                Ok(json!({"written":bytes.len()}))
            }
            "machine_input" => {
                self.machine.input(text(args, "text")?)?;
                Ok(json!({"queued":true}))
            }
            "machine_reset" => {
                self.running = false;
                self.machine.reset()?;
                self.capture_memory("boot", "Reboot restored RAM from disk");
                Ok(self.machine.state())
            }
            "machine_seal" => Ok(self.seal()),
            _ => Err(format!("Unknown machine tool: {name}")),
        }
    }
    pub fn action(&mut self, body: &Value) -> Result<Value, String> {
        let action = text(body, "action")?;
        if self.agent.is_active() && !["inspect", "input", "pause", "checkpoint"].contains(&action)
        {
            return Err("Stop the agent before changing the machine manually.".into());
        }
        let result = match action {
            "assemble" => self.compile(text(body, "source")?),
            "run" => {
                self.running = false;
                self.execute(number(body.get("cycles"), 100000, 1, 2000000, "cycles")?)
            }
            "step" => {
                self.running = false;
                let result = self.machine.step();
                self.capture_memory("run", "Guest instruction changed RAM");
                result
            }
            "pause" => {
                self.running = false;
                Ok(json!({"paused":true}))
            }
            "resume" => {
                self.running = true;
                Ok(json!({"running":true}))
            }
            "reset" => {
                self.running = false;
                self.machine.reset()?;
                self.capture_memory("boot", "Reboot restored RAM from disk");
                self.record(
                    "reboot",
                    "Rebooted from guest disk; lifetime cycles retained.",
                );
                Ok(self.machine.state())
            }
            "input" => {
                self.machine.input(text(body, "text")?)?;
                Ok(json!({"queued":true}))
            }
            "inspect" => self.inspect(body),
            "seal" => Ok(self.seal()),
            "new" => {
                let defaults = Config::default();
                let config = Config {
                    ram_bytes: number(
                        body.get("ramBytes"),
                        defaults.ram_bytes as u64,
                        65536,
                        64 * 1024 * 1024,
                        "ramBytes",
                    )? as usize,
                    disk_bytes: number(
                        body.get("diskBytes"),
                        defaults.disk_bytes as u64,
                        8,
                        MAX_DISK_BYTES as u64,
                        "diskBytes",
                    )? as usize,
                    clock_hz: number(
                        body.get("clockHz"),
                        defaults.clock_hz,
                        1,
                        10_000_000_000,
                        "clockHz",
                    )?,
                    max_cycles: number(
                        body.get("maxCycles"),
                        defaults.max_cycles,
                        1,
                        9_007_199_254_740_991,
                        "maxCycles",
                    )?,
                };
                let mut fresh = Machine::new(config)?;
                fresh.load(&assemble(BLANK, &fresh.config)?.bytes)?;
                self.save_to(&self.checkpoint_path(&format!("before-new-{}", now_ms())))?;
                self.machine = fresh;
                self.machine.take_memory_changes();
                self.memory_revision = 0;
                self.memory_events.clear();
                self.source = BLANK.into();
                self.running = false;
                self.experiment = Uuid::new_v4().to_string();
                self.events.clear();
                self.journal = Journal::default();
                self.journal_session = Uuid::new_v4().to_string();
                self.restored_from = None;
                self.throughput = 0.0;
                self.clear_agent_history();
                self.record(
                    "system",
                    "Started a fresh Rust experiment. Previous machine checkpointed.",
                );
                Ok(self.state())
            }
            "checkpoint" => {
                let name = body["name"]
                    .as_str()
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("snapshot-{}", now_ms()));
                if !safe_name(&name) {
                    return Err(
                        "Checkpoint name: 1..48 letters, numbers, underscores or hyphens".into(),
                    );
                }
                let file = self.checkpoint_path(&name);
                if file.exists() {
                    return Err("Checkpoint already exists".into());
                }
                self.record("checkpoint", &format!("Saved checkpoint {name}."));
                self.save_to(&file)?;
                Ok(json!({"name":name}))
            }
            "restore" => {
                let name = text(body, "name")?;
                if !safe_name(name) {
                    return Err("Invalid checkpoint name".into());
                }
                let file = self.checkpoint_path(name);
                if !file.exists() {
                    return Err("Checkpoint not found".into());
                }
                self.save_to(&self.checkpoint_path(&format!("before-restore-{}", now_ms())))?;
                self.restore(&file, true)?;
                self.clear_agent_history();
                self.record(
                    "restore",
                    &format!("Branched from {name}; cycle consumption and seal retained."),
                );
                Ok(self.state())
            }
            _ => Err(format!("Unknown action: {action}")),
        };
        if action != "inspect" && result.is_ok() {
            self.dirty = true;
        }
        result
    }
    fn frame(&self) -> &[u8] {
        &self.machine.ram[self.machine.video_base..self.machine.video_base + 49152]
    }
    fn drain_agent_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            let message = event["message"].as_str().unwrap_or("Agent event");
            let message = if event["truncated"] == true {
                format!("[Long public message truncated]\n{message}")
            } else {
                message.to_owned()
            };
            self.record(event["type"].as_str().unwrap_or("agent"), &message);
        }
    }
    fn tick(&mut self) {
        self.drain_agent_events();
        if !STOP.load(Ordering::Relaxed) {
            self.advance_session();
        }
        if self.running {
            if let Err(e) = self.execute(100000) {
                self.running = false;
                self.record("error", &e);
            }
            if !self.running {
                self.record("cpu", &format!("Machine {}.", self.machine.status));
            }
        }
        if self.dirty && self.last_save.elapsed() >= Duration::from_secs(2) {
            if let Err(e) = self.save() {
                eprintln!("Persistence failed: {e}");
                self.last_save = Instant::now();
                self.record("error", &format!("Persistence failed: {e}"));
            }
        }
    }
    fn shutdown(&mut self) -> Result<(), String> {
        if let Some(session) = self.session.as_mut() {
            session.pause_on_restore();
        }
        self.agent.stop();
        self.running = false;
        self.save()
    }
}

fn tool_evidence(name: &str, args: &Value, result: Option<&Value>) -> Value {
    let arguments = match name {
        "machine_assemble" => {
            json!({"sourceBytes":args["source"].as_str().map(str::len),"sourcePreview":args["source"].as_str().map(|s|s.chars().take(1600).collect::<String>()),"previewOnly":true})
        }
        "machine_write" => {
            json!({"space":args["space"],"address":args["address"],"byteCount":args["bytes"].as_array().map(Vec::len),"bytesPreview":args["bytes"].as_array().map(|b|b.iter().take(32).collect::<Vec<_>>())})
        }
        "machine_input" => {
            json!({"text":args["text"].as_str().map(|s|s.chars().take(1000).collect::<String>())})
        }
        _ => args.clone(),
    };
    let result=result.map(|v|match name {
        "machine_info"=>json!({"status":v["machine"]["status"],"cycles":v["machine"]["cycles"],"config":v["machine"]["config"]}),
        "machine_screen"=>json!({"width":v["width"],"height":v["height"],"observation":"Guest framebuffer snapshot returned to agent"}),
        "machine_run"|"machine_reset"|"machine_seal"=>json!({"status":v["status"],"cycles":v["cycles"],"instructions":v["instructions"],"pc":v["pc"],"fault":v["fault"],"serialTail":v["serial"].as_str().map(|s|{let chars=s.chars().collect::<Vec<_>>(); chars.iter().skip(chars.len().saturating_sub(1000)).collect::<String>()})}),
        "machine_assemble"=>json!({"bytes":v["bytes"],"symbolCount":v["symbols"].as_object().map(|s|s.len())}),
        "machine_inspect"=>json!({"space":v["space"],"address":v["address"],"length":v["bytes"].as_array().map(Vec::len),"bytesPreview":v["bytes"].as_array().map(|b|b.iter().take(64).collect::<Vec<_>>())}),
        _=>v.clone(),
    });
    json!({"tool":name,"arguments":arguments,"result":result,"summary":true})
}

fn header(req: &Request, name: &'static str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}
fn reply(req: Request, status: u16, mime: &str, bytes: Vec<u8>, extra: Option<(&str, &str)>) {
    // Every response is already bounded and buffered. A fixed Content-Length also
    // keeps the deliberately small loopback MCP client independent of chunking.
    let mut response = Response::from_data(bytes)
        .with_status_code(StatusCode(status))
        .with_chunked_threshold(usize::MAX);
    for (name, value) in [
        ("Content-Type", mime),
        ("Cache-Control", "no-store"),
        ("X-Content-Type-Options", "nosniff"),
        ("Referrer-Policy", "no-referrer"),
        (
            "Content-Security-Policy",
            "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'",
        ),
    ] {
        if let Ok(h) = Header::from_bytes(name, value) {
            response.add_header(h);
        }
    }
    if let Some((name, value)) = extra {
        if let Ok(h) = Header::from_bytes(name, value) {
            response.add_header(h);
        }
    }
    let _ = req.respond(response);
}
fn json_reply(req: Request, status: u16, value: Value) {
    reply(
        req,
        status,
        "application/json",
        serde_json::to_vec(&value).unwrap_or_default(),
        None,
    );
}
fn body(req: &mut Request) -> Result<Value, String> {
    if !header(req, "Content-Type").is_some_and(|v| v.starts_with("application/json")) {
        return Err("Content-Type must be application/json".into());
    }
    if req.body_length().is_some_and(|len| len > MAX_BODY) {
        return Err("Request body too large".into());
    }
    let mut bytes = vec![];
    req.as_reader()
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BODY {
        return Err("Request body too large".into());
    }
    let result: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if !result.is_object() {
        return Err("Expected a JSON object".into());
    }
    Ok(result)
}
struct NetworkAccess {
    port: u16,
    hosts: Vec<String>,
    access_key: Option<String>,
}
impl NetworkAccess {
    fn new(port: u16, lan: bool, addresses: &[std::net::Ipv4Addr]) -> Self {
        let mut hosts = vec![format!("127.0.0.1:{port}"), format!("localhost:{port}")];
        if lan {
            hosts.extend(addresses.iter().map(|address| format!("{address}:{port}")));
        }
        Self {
            port,
            hosts,
            access_key: lan.then(token),
        }
    }
    fn observer_url(&self, host: &str) -> String {
        let base = format!("http://{host}:{}", self.port);
        match &self.access_key {
            Some(key) => format!("{base}/#access={key}"),
            None => base,
        }
    }
    fn authorize(
        &self,
        host: Option<&str>,
        origin: Option<&str>,
        loopback_peer: bool,
        path: &str,
        access: Option<&str>,
        machine_auth: (Option<&str>, &str),
    ) -> Result<(), (u16, &'static str, &'static str)> {
        let Some(host) = host.filter(|host| self.hosts.iter().any(|allowed| allowed == host))
        else {
            return Err((403, "Observer host is not allowed", "host_not_allowed"));
        };
        if origin.is_some_and(|origin| origin != format!("http://{host}")) {
            return Err((403, "Cross-origin access is disabled", "origin_not_allowed"));
        }
        if path == "/api/tool" {
            if !loopback_peer
                || ![
                    format!("127.0.0.1:{}", self.port),
                    format!("localhost:{}", self.port),
                ]
                .contains(&host.to_owned())
            {
                return Err((
                    403,
                    "Machine tools are available only to the local agent",
                    "local_agent_required",
                ));
            }
            if machine_auth.0 != Some(format!("Bearer {}", machine_auth.1).as_str()) {
                return Err((
                    401,
                    "Invalid machine capability",
                    "machine_capability_required",
                ));
            }
        } else if path.starts_with("/api/")
            && self
                .access_key
                .as_deref()
                .is_some_and(|key| access != Some(key))
        {
            return Err((
                401,
                "Open the private access link printed by picos to connect this device",
                "access_required",
            ));
        }
        Ok(())
    }
}

fn local_network_addresses() -> Result<Vec<std::net::Ipv4Addr>, String> {
    #[cfg(unix)]
    {
        let mut addresses = std::collections::BTreeSet::new();
        let mut first = std::ptr::null_mut();
        // getifaddrs owns the linked list until freeifaddrs. Only AF_INET
        // addresses are cast, and no references escape the list's lifetime.
        unsafe {
            if libc::getifaddrs(&mut first) != 0 {
                return Err("Could not list local network interfaces.".into());
            }
            let mut current = first;
            while !current.is_null() {
                let interface = &*current;
                if !interface.ifa_addr.is_null()
                    && interface.ifa_flags & libc::IFF_UP as u32 != 0
                    && (*interface.ifa_addr).sa_family as i32 == libc::AF_INET
                {
                    let socket = &*(interface.ifa_addr as *const libc::sockaddr_in);
                    let address = std::net::Ipv4Addr::from(socket.sin_addr.s_addr.to_ne_bytes());
                    if address.is_private() || address.is_link_local() {
                        addresses.insert(address);
                    }
                }
                current = interface.ifa_next;
            }
            libc::freeifaddrs(first);
        }
        if addresses.is_empty() {
            return Err("No private IPv4 network address found. Connect to Wi-Fi or Ethernet before using --lan.".into());
        }
        Ok(addresses.into_iter().collect())
    }
    #[cfg(not(unix))]
    {
        Err("Local network sharing currently requires macOS or Linux.".into())
    }
}

fn handle(mut req: Request, lab: &mut Lab, network: &NetworkAccess) {
    let url = req.url().split('?').next().unwrap_or("/").to_string();
    if let Err((status, message, code)) = network.authorize(
        header(&req, "Host").as_deref(),
        header(&req, "Origin").as_deref(),
        req.remote_addr()
            .is_some_and(|peer| peer.ip().is_loopback()),
        &url,
        header(&req, "X-Picos-Access").as_deref(),
        (header(&req, "Authorization").as_deref(), &lab.agent_token),
    ) {
        return json_reply(req, status, json!({"error":message,"code":code}));
    }
    if req.method() == &Method::Get {
        match url.as_str() {
            "/api/agent/journal" => {
                let after = req
                    .url()
                    .split_once('?')
                    .and_then(|(_, q)| q.split('&').find_map(|p| p.strip_prefix("after=")))
                    .unwrap_or("0");
                let Ok(after) = after.parse::<u64>() else {
                    return json_reply(
                        req,
                        400,
                        json!({"error":"after must be a nonnegative integer"}),
                    );
                };
                let mut journal = lab.journal.view(after);
                journal["experiment"] = json!(lab.experiment);
                journal["mission"] = json!(
                    lab.session
                        .as_ref()
                        .map(AgentSession::mission)
                        .or_else(|| lab.details.as_ref().and_then(|d| d["mission"].as_str()))
                        .unwrap_or("")
                );
                journal["initialMission"] = lab
                    .details
                    .as_ref()
                    .map(|d| d["initialMission"].clone())
                    .unwrap_or(Value::Null);
                return json_reply(req, 200, journal);
            }
            "/api/state" => {
                let mut state = lab.state();
                if lab.read_only {
                    state.as_object_mut().unwrap().remove("source");
                    state.as_object_mut().unwrap().remove("checkpoints");
                    state["events"] = json!(
                        lab.events
                            .iter()
                            .rev()
                            .take(12)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<Vec<_>>()
                    );
                }
                return json_reply(req, 200, state);
            }
            "/api/disassembly" => {
                let result = (|| -> Result<Value, String> {
                    let mut address = 0usize;
                    let mut length = 256usize;
                    let mut seen = std::collections::HashSet::new();
                    if let Some((_, query)) = req.url().split_once('?') {
                        for pair in query.split('&').filter(|s| !s.is_empty()) {
                            let (key, value) =
                                pair.split_once('=').ok_or("Invalid memory query")?;
                            if !seen.insert(key) {
                                return Err(format!("Duplicate query parameter {key}"));
                            }
                            let value=value.parse::<usize>().map_err(|_|"Memory address and length must be nonnegative decimal integers")?;
                            match key {
                                "address" => address = value,
                                "length" => length = value,
                                _ => return Err(format!("Unknown query parameter {key}")),
                            }
                        }
                    }
                    crate::disassembler::disassemble(&lab.machine, address, length)
                })();
                return match result {
                    Ok(value) => json_reply(req, 200, value),
                    Err(error) => json_reply(req, 400, json!({"error":error})),
                };
            }
            "/api/isa" => return json_reply(req, 200, json!({"text":format!("{SPEC}\n\n{ISA}")})),
            "/api/examples" => {
                return json_reply(
                    req,
                    200,
                    json!([
                        {"name":"blank.asm","source":include_str!("../examples/blank.asm")},
                        {"name":"signal.asm","source":include_str!("../examples/signal.asm")},
                        {"name":"echo.asm","source":include_str!("../examples/echo.asm")}
                    ]),
                );
            }
            "/api/frame" => {
                let frame = lab.frame().to_vec();
                return reply(req, 200, "application/octet-stream", frame, None);
            }
            "/api/checkpoint/download" => {
                match lab.encoded_snapshot() {
                    Ok(data) => reply(
                        req,
                        200,
                        "application/gzip",
                        data,
                        Some((
                            "Content-Disposition",
                            "attachment; filename=\"smol-checkpoint.json.gz\"",
                        )),
                    ),
                    Err(e) => json_reply(req, 500, json!({"error":e})),
                }
                return;
            }
            "/" => {
                return reply(
                    req,
                    200,
                    "text/html; charset=utf-8",
                    include_bytes!("../public/index.html").to_vec(),
                    None,
                );
            }
            "/app.js" => {
                return reply(
                    req,
                    200,
                    "text/javascript; charset=utf-8",
                    include_bytes!("../public/app.js").to_vec(),
                    None,
                );
            }
            "/style.css" => {
                return reply(
                    req,
                    200,
                    "text/css; charset=utf-8",
                    include_bytes!("../public/style.css").to_vec(),
                    None,
                );
            }
            _ => return json_reply(req, 404, json!({"error":"Not found"})),
        }
    }
    if req.method() != &Method::Post {
        return json_reply(req, 405, json!({"error":"Method not allowed"}));
    }
    let session_control = [
        "/api/session/settings",
        "/api/session/turns",
        "/api/session/continue",
        "/api/session/stop",
    ]
    .contains(&url.as_str());
    if session_control && header(&req, "X-Picos-Operator") != Some(lab.operator_token.clone()) {
        return json_reply(req, 401, json!({"error":"Operator capability required"}));
    }
    if lab.read_only && url != "/api/tool" && !session_control {
        return json_reply(
            req,
            403,
            json!({"error":"This is a read-only experiment observer. Manage experiments from the picos CLI."}),
        );
    }
    if url == "/api/tool" {
        if header(&req, "Authorization") != Some(format!("Bearer {}", lab.agent_token)) {
            return json_reply(req, 401, json!({"error":"Invalid machine capability"}));
        }
        if !lab.agent.is_active() {
            return json_reply(req, 409, json!({"error":"No active agent session"}));
        }
        if lab
            .session
            .as_ref()
            .is_some_and(|s| s.state()["status"] != "running")
        {
            return json_reply(req, 409, json!({"error":"Experiment is paused or stopped"}));
        }
        if lab
            .tool_limit()
            .is_some_and(|limit| lab.tool_calls >= limit)
        {
            return json_reply(
                req,
                429,
                json!({"error":"Agent tool-call budget exhausted. End this session."}),
            );
        }
    }
    let result = (|| -> Result<Value, String> {
        let value = body(&mut req)?;
        if session_control {
            return lab.session_control(&url, &value);
        }
        match url.as_str() {
            "/api/action" => lab.action(&value),
            "/api/tool" => {
                let name = text(&value, "name")?;
                let args = value.get("arguments").cloned().unwrap_or(json!({}));
                lab.tool_calls += 1;
                let result = lab.tool(name, &args);
                lab.enforce_limits(result.as_ref().err().map(String::as_str));
                match &result {
                    Ok(value) => lab.record_detail(
                        "tool",
                        &format!("{name} completed · {} lifetime cycles.", lab.machine.cycles),
                        Some(tool_evidence(name, &args, Some(value))),
                    ),
                    Err(e) => lab.record_detail(
                        "tool-error",
                        &format!("{name}: {e}"),
                        Some(tool_evidence(name, &args, None)),
                    ),
                }
                result
            }
            "/api/agent/start" => {
                if lab.agent.is_active() {
                    return Err("Agent is already running".into());
                }
                let mission = text(&value, "mission")?;
                if mission.trim().is_empty() || mission.len() > 12000 {
                    return Err("Mission must contain 1..12000 bytes".into());
                }
                lab.running = false;
                lab.tool_calls = 0;
                lab.agent_token = token();
                let strict = lab.agent.is_strict();
                lab.agent = AgentRunner::new(
                    lab.base_url.clone(),
                    lab.agent_token.clone(),
                    lab.data_dir.join("agent-work"),
                    lab.event_tx.clone(),
                );
                lab.agent.set_strict(strict)?;
                let max_turns = number(value.get("turns"), 6, 1, 24, "turns")?;
                let status = lab.start_session(
                    mission,
                    value["model"].as_str(),
                    max_turns,
                    RunMode::Standard,
                    true,
                )?;
                lab.record(
                    "agent",
                    if strict {
                        "Started the strict direct ChatGPT harness."
                    } else {
                        "Started Codex using the existing ChatGPT sign-in."
                    },
                );
                Ok(status)
            }
            "/api/agent/stop" => {
                if let Some(session) = lab.session.as_mut() {
                    session.stop("stopped", "Stopped by operator; machine preserved.");
                }
                lab.agent.stop();
                lab.agent_token = token();
                lab.record("agent", "Agent stop requested; machine capability revoked.");
                Ok(json!({"stopped":true}))
            }
            _ => Err("Not found".into()),
        }
    })();
    match result {
        Ok(v) => json_reply(req, 200, v),
        Err(e) => json_reply(req, 400, json!({"error":e})),
    }
}

#[derive(Debug, Clone)]
pub struct ExperimentOptions {
    pub name: String,
    pub data_dir: PathBuf,
    pub port: u16,
    pub config: Option<Config>,
    pub mission: Option<String>,
    pub model: Option<String>,
    pub open_browser: bool,
    pub resume: bool,
    pub max_turns: u64,
    pub mode: Option<RunMode>,
    pub tell_turns: Option<bool>,
    pub strict: bool,
    pub lan: bool,
}

// Hold an OS lock for the process lifetime, including across checkpoint writes.
// A crash releases the Unix lock automatically; stale lock files are harmless.
struct ExperimentLock {
    _file: fs::File,
}
impl ExperimentLock {
    fn acquire(directory: &Path) -> Result<Self, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join(".lock"))
            .map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err("This experiment is already running in another process.".into());
            }
        }
        Ok(Self { _file: file })
    }
}

fn open_observer(url: &str) {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    match result {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("Could not open a browser ({e}). Open {url} manually."),
    }
}

pub fn serve_experiment(options: ExperimentOptions) -> Result<(), String> {
    if !safe_name(&options.name) {
        return Err(
            "Experiment name must contain 1..48 letters, numbers, underscores or hyphens.".into(),
        );
    }
    if let Some(config) = &options.config {
        config.validate()?;
    }
    if let Some(mission) = &options.mission {
        if mission.trim().is_empty() || mission.len() > 12000 {
            return Err("Prompt must contain 1..12000 bytes.".into());
        }
    }
    if !options.resume && options.mission.is_none() {
        return Err("New experiments need a prompt.".into());
    }
    // Fail before creating a new strict experiment when direct subscription
    // authorization is unavailable. Never fall back to Codex or an API key.
    if options.strict && options.mission.is_some() {
        crate::direct_auth::preflight()?;
    }
    // Reserve the requested port before creating any experiment files.
    let lan_addresses = if options.lan {
        local_network_addresses()?
    } else {
        vec![]
    };
    let server = Server::http((
        if options.lan { "0.0.0.0" } else { "127.0.0.1" },
        options.port,
    ))
    .map_err(|e| format!("Cannot listen on local port {}: {e}", options.port))?;
    let bound = server
        .server_addr()
        .to_ip()
        .ok_or("Missing TCP address")?
        .port();
    let base = format!("http://127.0.0.1:{bound}");
    let network = NetworkAccess::new(bound, options.lan, &lan_addresses);
    let observer_url = network.observer_url("127.0.0.1");
    if options.resume {
        if !options.data_dir.join("experiment.json").is_file()
            || !options.data_dir.join("latest.json.gz").is_file()
        {
            return Err(format!(
                "No saved experiment at {}",
                options.data_dir.display()
            ));
        }
    } else {
        if let Some(parent) = options
            .data_dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::create_dir(&options.data_dir).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "Experiment '{}' already exists. Use picos resume {} or choose a new name.",
                    options.name, options.name
                )
            } else {
                e.to_string()
            }
        })?;
    }
    let _lock = ExperimentLock::acquire(&options.data_dir)?;
    let mut lab = Lab::with_config(
        options.data_dir.clone(),
        base.clone(),
        options.config.clone(),
    )?;
    lab.read_only = true;
    lab.terminal_events = true;
    if options.strict {
        lab.agent.set_strict(true)?;
    }
    if !options.resume {
        lab.details = Some(
            json!({"version":1,"name":options.name,"createdAt":now_ms(),"mission":options.mission,"model":options.model,"config":lab.machine.config}),
        );
    } else if let Some(mission) = &options.mission {
        if let Some(details) = lab.details.as_mut() {
            if details.get("initialMission").is_none() {
                details["initialMission"] = details["mission"].clone();
            }
            details["mission"] = json!(mission);
            if let Some(model) = &options.model {
                details["model"] = json!(model);
            }
        }
    }
    if let Some(details) = lab.details.as_mut() {
        details["strict"] = json!(lab.agent.is_strict());
        details["agentBackend"] = json!(if lab.agent.is_strict() {
            "chatgpt-direct"
        } else {
            "codex"
        });
    }
    let mode = options
        .mode
        .or_else(|| lab.session.as_ref().map(AgentSession::mode))
        .unwrap_or_default();
    let tell_turns = options
        .tell_turns
        .or_else(|| lab.session.as_ref().map(AgentSession::tell_turns))
        .unwrap_or(true);
    if options.mission.is_none() {
        if lab.session.is_none() {
            if let Some(details) = lab.details.as_ref() {
                if let Some(mission) = details["mission"].as_str() {
                    let session = AgentSession::dormant(
                        mission.into(),
                        details["model"].as_str().map(str::to_owned),
                        mode,
                        tell_turns,
                    )?;
                    lab.session = Some(session);
                }
            }
        } else if let Some(session) = lab.session.as_mut() {
            if options.mode.is_some() {
                session.set_mode(mode);
            }
            session.set_tell_turns(tell_turns);
        }
    }
    let metadata_file = lab.data_dir.join("experiment.json");
    let metadata_temp = lab.data_dir.join("experiment.json.tmp");
    fs::write(
        &metadata_temp,
        serde_json::to_vec_pretty(&lab.details).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    fs::rename(metadata_temp, metadata_file).map_err(|e| e.to_string())?;
    lab.save()?;
    install_signals();
    // First line is a machine-readable URL, also useful when opening manually.
    println!("{observer_url}");
    if options.lan {
        println!(
            "  Phone access · join the same local network and open one of these private links:"
        );
        for address in &lan_addresses {
            println!("  {}", network.observer_url(&address.to_string()));
        }
        println!("  Anyone with this link can view the experiment and use its session controls.");
    }
    println!(
        "picos / {}\n  CPU {} MHz · RAM {} MB · storage {} MB\n  model {}\n  data {}\n  Ctrl+C saves this experiment and stops its agent.",
        lab.details
            .as_ref()
            .and_then(|m| m["name"].as_str())
            .unwrap_or(&options.name),
        lab.machine.config.clock_hz as f64 / 1_000_000.0,
        lab.machine.config.ram_bytes as f64 / 1_000_000.0,
        lab.machine.config.disk_bytes as f64 / 1_000_000.0,
        options
            .model
            .as_deref()
            .or_else(|| lab.details.as_ref().and_then(|m| m["model"].as_str()))
            .unwrap_or("Codex default"),
        lab.data_dir.display()
    );
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    if options.open_browser {
        open_observer(&observer_url);
    }
    if let Some(mission) = options.mission.as_deref() {
        let model = options.model.clone().or_else(|| {
            lab.details
                .as_ref()
                .and_then(|m| m["model"].as_str().map(str::to_owned))
        });
        if let Err(error) = lab.start_session(
            mission,
            model.as_deref(),
            options.max_turns,
            mode,
            tell_turns,
        ) {
            lab.record("error", &error);
            lab.save()?;
            return Err(format!(
                "{error}\nThe blank experiment is saved; use picos login, then picos resume {} --prompt <mission>.",
                options.name
            ));
        }
        lab.record(
            "agent",
            "Experiment agent started using ChatGPT authentication.",
        );
    } else {
        println!("  Observer resumed. No new model turn was started.");
    }
    run_loop(server, lab, network)
}

pub fn serve(port: u16, data_dir: PathBuf) -> Result<(), String> {
    let server = Server::http(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    let bound = server
        .server_addr()
        .to_ip()
        .ok_or("Missing TCP address")?
        .port();
    let base = format!("http://127.0.0.1:{bound}");
    let lab = Lab::new(data_dir, base.clone())?;
    install_signals();
    println!("{base}");
    println!("picos / machine observer · Rust\nCtrl+C to save and stop.");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    run_loop(server, lab, NetworkAccess::new(bound, false, &[]))
}
fn install_signals() {
    STOP.store(false, Ordering::Relaxed);
    #[cfg(unix)]
    unsafe {
        libc::signal(
            libc::SIGINT,
            shutdown_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            shutdown_signal as *const () as libc::sighandler_t,
        );
    }
}
fn run_loop(server: Server, mut lab: Lab, network: NetworkAccess) -> Result<(), String> {
    while !STOP.load(Ordering::Relaxed) {
        match server.recv_timeout(Duration::from_millis(16)) {
            Ok(Some(req)) => handle(req, &mut lab, &network),
            Ok(None) => {}
            Err(e) => {
                let _ = lab.shutdown();
                return Err(e.to_string());
            }
        }
        lab.tick();
    }
    lab.shutdown()
}

#[cfg(test)]
mod journal_transition_tests {
    use super::*;

    #[test]
    fn strict_checkpoint_is_portable_and_legacy_restore_cannot_downgrade_it() {
        let directory = std::env::temp_dir().join(format!("picos-portable-{}", Uuid::new_v4()));
        let source = directory.join("source");
        let destination = directory.join("destination");
        let mut lab = Lab::with_config(
            source,
            "http://127.0.0.1:1".into(),
            Some(Config {
                ram_bytes: 65536,
                disk_bytes: 4096,
                ..Config::default()
            }),
        )
        .unwrap();
        lab.agent.set_strict(true).unwrap();
        assert_eq!(lab.snapshot()["strict"], true);
        lab.save().unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::copy(lab.latest(), destination.join("latest.json.gz")).unwrap();
        assert!(!destination.join("experiment.json").exists());
        let mut restored =
            Lab::with_config(destination, "http://127.0.0.1:1".into(), None).unwrap();
        assert!(restored.agent.is_strict());

        for boundary in [None, Some(json!(false))] {
            let mut snapshot = restored.snapshot();
            snapshot.as_object_mut().unwrap().remove("strict");
            if let Some(boundary) = boundary {
                snapshot["strict"] = boundary;
            }
            let checkpoint = directory.join("legacy.json.gz");
            let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
            serde_json::to_writer(&mut encoder, &snapshot).unwrap();
            fs::write(&checkpoint, encoder.finish().unwrap()).unwrap();
            restored.restore(&checkpoint, true).unwrap();
            assert!(restored.agent.is_strict());
            assert_eq!(restored.snapshot()["strict"], true);
            restored.clear_agent_history();
            assert!(restored.agent.is_strict());
        }
        drop(restored);
        drop(lab);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn strict_survives_reload_and_runner_recreation_and_cannot_downgrade() {
        let directory = std::env::temp_dir().join(format!("picos-strict-{}", Uuid::new_v4()));
        let mut lab = Lab::with_config(
            directory.clone(),
            "http://127.0.0.1:1".into(),
            Some(Config {
                ram_bytes: 65536,
                disk_bytes: 4096,
                ..Config::default()
            }),
        )
        .unwrap();
        lab.agent.set_strict(true).unwrap();
        lab.details = Some(json!({"strict":true,"agentBackend":"chatgpt-direct"}));
        fs::write(
            directory.join("experiment.json"),
            lab.details.as_ref().unwrap().to_string(),
        )
        .unwrap();
        lab.save().unwrap();
        drop(lab);
        let mut restored =
            Lab::with_config(directory.clone(), "http://127.0.0.1:1".into(), None).unwrap();
        assert!(restored.agent.is_strict());
        restored.clear_agent_history();
        assert!(restored.agent.is_strict());
        assert!(restored.agent.set_strict(false).is_err());
        assert!(
            restored
                .agent
                .submitted_prompt("go")
                .contains("STRICT EXECUTION")
        );
        assert!(
            restored
                .tool("exec_command", &json!({"cmd":"echo forbidden"}))
                .is_err()
        );
        assert!(
            restored
                .tool("machine_info", &json!({"url":"https://example.com"}))
                .is_err()
        );
        drop(restored);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_session_recovers_explicitly_without_refunding_saved_guest_resources() {
        let directory = std::env::temp_dir().join(format!("picos-recovery-{}", Uuid::new_v4()));
        let base = "http://127.0.0.1:1";
        let mut lab = Lab::with_config(
            directory.clone(),
            base.into(),
            Some(Config {
                ram_bytes: 65536,
                disk_bytes: 4096,
                ..Config::default()
            }),
        )
        .unwrap();
        lab.action(&json!({"action":"assemble", "source":"mov r0, 65\nout r0, 0\nstore8 r0, VIDEO_BASE\nmov r1, 2048\nout r1, 2\nout r0, 3\nhalt\n"})).unwrap();
        lab.action(&json!({"action":"run", "cycles":1000})).unwrap();
        lab.tool_calls = 17;
        lab.last_report = Some("Previous verified milestone".into());
        let mut session = AgentSession::new("Continue the saved system".into(), None, 2).unwrap();
        session.begin_turn(lab.progress()).unwrap();
        assert!(!session.finish_turn("failed", lab.progress(), 17, 80, 1_000_000));
        lab.session = Some(session);
        lab.save().unwrap();
        let saved_machine = lab.machine.export_state();
        let saved_source = lab.source.clone();
        let saved_revision = lab.memory_revision;
        let saved_experiment = lab.experiment.clone();
        drop(lab);

        let mut restored = Lab::with_config(directory.clone(), base.into(), None).unwrap();
        assert!(!restored.agent.is_active());
        assert_eq!(restored.state()["agent"]["session"]["status"], "failed");
        assert_eq!(restored.state()["sessionControls"]["canContinue"], true);
        let waiting = restored
            .session_control("/api/session/continue", &json!({}))
            .unwrap();
        assert_eq!(waiting["agent"]["session"]["status"], "waiting");
        assert_eq!(waiting["agent"]["session"]["turn"], 1);
        assert_eq!(waiting["agent"]["session"]["maxTurns"], 2);
        assert_eq!(restored.tool_calls, 17);
        assert_eq!(restored.machine.export_state(), saved_machine);
        assert_eq!(restored.source, saved_source);
        assert_eq!(restored.memory_revision, saved_revision);
        assert_eq!(restored.experiment, saved_experiment);
        assert_eq!(
            restored.last_report.as_deref(),
            Some("Previous verified milestone")
        );

        // Recovery still passes through the same cumulative tool budget check.
        restored.tool_calls = 80;
        restored.enforce_limits(None);
        assert!(
            restored
                .session_control("/api/session/continue", &json!({}))
                .is_err()
        );
        restored
            .session_control("/api/session/turns", &json!({"turns":1}))
            .unwrap();
        assert_eq!(restored.tool_calls, 80);
        assert_eq!(restored.tool_limit(), Some(120));
        assert_eq!(restored.machine.export_state(), saved_machine);
        restored.stop_resource("Guest RAM boundary reached");
        restored.save().unwrap();
        drop(restored);
        let mut stopped = Lab::with_config(directory.clone(), base.into(), None).unwrap();
        assert!(
            stopped
                .session_control("/api/session/continue", &json!({}))
                .is_err()
        );
        assert!(
            stopped
                .session_control("/api/session/turns", &json!({"turns":1}))
                .is_err()
        );
        assert_eq!(stopped.tool_calls, 80);
        assert_eq!(stopped.machine.export_state(), saved_machine);
        drop(stopped);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn inactive_transition_drains_late_events_before_session_finishes() {
        let directory = std::env::temp_dir().join(format!("picos-journal-{}", Uuid::new_v4()));
        let mut lab = Lab::with_config(
            directory.clone(),
            "http://127.0.0.1:1".into(),
            Some(Config {
                ram_bytes: 65536,
                disk_bytes: 4096,
                ..Config::default()
            }),
        )
        .unwrap();
        let mut session = AgentSession::new("Build a terminal".into(), None, 1).unwrap();
        session.begin_turn(lab.progress()).unwrap();
        lab.session = Some(session);
        let session_id = lab.journal_session.clone();

        // Simulate a worker's final event arriving after tick's initial drain
        // and before advance_session observes the worker as inactive.
        lab.drain_agent_events();
        lab.event_tx
            .send(json!({"type":"agent.message","message":"Final public result"}))
            .unwrap();
        assert!(!lab.agent.is_active());
        lab.advance_session();

        let view = lab.journal.view(0);
        let entries = view["entries"].as_array().unwrap();
        let result = entries
            .iter()
            .position(|event| event["message"] == "Final public result")
            .expect("final result must be drained at the inactive transition");
        let finished = entries
            .iter()
            .position(|event| event["type"] == "agent.session")
            .unwrap();
        assert!(result < finished);
        assert_eq!(entries[result]["turn"], 1);
        assert_eq!(entries[result]["sessionId"], session_id);
        drop(lab);
        fs::remove_dir_all(directory).unwrap();
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpStream};

    #[test]
    fn lan_key_is_ephemeral_and_cannot_authorize_remote_machine_tools() {
        let network = NetworkAccess::new(4317, true, &[Ipv4Addr::new(192, 168, 1, 10)]);
        let key = network.access_key.as_deref().unwrap();
        assert_eq!(key.len(), 64);
        assert_ne!(
            network.access_key,
            NetworkAccess::new(4317, true, &[]).access_key
        );
        assert!(network.observer_url("192.168.1.10").contains("/#access="));
        let host = Some("192.168.1.10:4317");
        assert!(
            network
                .authorize(
                    host,
                    Some("http://192.168.1.10:4317"),
                    false,
                    "/api/state",
                    Some(key),
                    (None, "machine")
                )
                .is_ok()
        );
        assert_eq!(
            network
                .authorize(
                    host,
                    None,
                    false,
                    "/api/tool",
                    Some(key),
                    (Some("Bearer machine"), "machine")
                )
                .unwrap_err()
                .0,
            403
        );
        assert_eq!(
            network
                .authorize(
                    Some("127.0.0.1:4317"),
                    None,
                    false,
                    "/api/tool",
                    Some(key),
                    (Some("Bearer machine"), "machine")
                )
                .unwrap_err()
                .0,
            403
        );
        assert!(
            network
                .authorize(
                    Some("127.0.0.1:4317"),
                    Some("http://127.0.0.1:4317"),
                    true,
                    "/api/tool",
                    None,
                    (Some("Bearer machine"), "machine")
                )
                .is_ok()
        );
        assert!(
            network
                .authorize(
                    Some("127.0.0.1:4317"),
                    Some("http://192.168.1.10:4317"),
                    true,
                    "/api/state",
                    Some(key),
                    (None, "machine")
                )
                .is_err()
        );
        assert!(
            network
                .authorize(
                    Some("attacker.example:4317"),
                    None,
                    true,
                    "/api/state",
                    Some(key),
                    (None, "machine")
                )
                .is_err()
        );
        let loopback = NetworkAccess::new(4317, false, &[]);
        assert_eq!(loopback.observer_url("127.0.0.1"), "http://127.0.0.1:4317");
        assert!(
            loopback
                .authorize(
                    Some("127.0.0.1:4317"),
                    None,
                    true,
                    "/api/state",
                    None,
                    (None, "machine")
                )
                .is_ok()
        );
    }

    fn request(
        server: &Server,
        lab: &mut Lab,
        network: &NetworkAccess,
        method: &str,
        path: &str,
        extra: &str,
        body: &str,
    ) -> (u16, Vec<u8>) {
        std::thread::scope(|scope| {
            let task = scope.spawn(|| {
                let mut socket = TcpStream::connect(("127.0.0.1",network.port)).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                write!(socket,"{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",network.port,body.len()).unwrap();
                let mut bytes=vec![];
                socket.read_to_end(&mut bytes).unwrap();
                let split=bytes.windows(4).position(|w|w==b"\r\n\r\n").unwrap();
                let headers=std::str::from_utf8(&bytes[..split]).unwrap();
                assert!(!headers.contains("Access-Control-Allow-Origin"));
                (headers.lines().next().unwrap().split_whitespace().nth(1).unwrap().parse().unwrap(),bytes[split+4..].to_vec())
            });
            let req = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            handle(req, lab, network);
            task.join().unwrap()
        })
    }

    #[test]
    fn actual_lan_http_requires_key_before_exposing_state_and_controls() {
        let server = Server::http(("127.0.0.1", 0)).unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let network = NetworkAccess::new(port, true, &[]);
        let directory = std::env::temp_dir().join(format!("picos-lan-test-{}", Uuid::new_v4()));
        let mut lab = Lab::with_config(
            directory.clone(),
            format!("http://127.0.0.1:{port}"),
            Some(Config {
                ram_bytes: 65536,
                disk_bytes: 4096,
                ..Config::default()
            }),
        )
        .unwrap();
        lab.read_only = true;
        lab.session = Some(
            AgentSession::dormant("Private mission".into(), None, RunMode::Standard, true).unwrap(),
        );
        let access = format!(
            "X-Picos-Access: {}\r\n",
            network.access_key.as_ref().unwrap()
        );
        for path in [
            "/api/state",
            "/api/agent/journal",
            "/api/disassembly",
            "/api/frame",
            "/api/checkpoint/download",
        ] {
            let (status, bytes) = request(&server, &mut lab, &network, "GET", path, "", "");
            assert_eq!(status, 401, "{path}");
            assert!(!String::from_utf8_lossy(&bytes).contains("Private mission"));
            assert_eq!(
                request(
                    &server,
                    &mut lab,
                    &network,
                    "GET",
                    path,
                    "X-Picos-Access: wrong\r\n",
                    ""
                )
                .0,
                401
            );
            assert_eq!(
                request(&server, &mut lab, &network, "GET", path, &access, "").0,
                200,
                "{path}"
            );
        }
        assert_eq!(
            request(&server, &mut lab, &network, "GET", "/", "", "").0,
            200
        );
        assert_eq!(
            request(
                &server,
                &mut lab,
                &network,
                "GET",
                "/api/state",
                &format!("{access}Origin: https://attacker.example\r\n"),
                ""
            )
            .0,
            403
        );
        let controls = format!("{access}X-Picos-Operator: {}\r\n", lab.operator_token);
        assert_eq!(
            request(
                &server,
                &mut lab,
                &network,
                "POST",
                "/api/session/settings",
                &access,
                r#"{"tellTurns":false}"#
            )
            .0,
            401
        );
        assert_eq!(
            request(
                &server,
                &mut lab,
                &network,
                "POST",
                "/api/session/settings",
                &controls,
                r#"{"tellTurns":false}"#
            )
            .0,
            200
        );
        assert!(!lab.session.as_ref().unwrap().tell_turns());
        assert_eq!(
            request(
                &server,
                &mut lab,
                &network,
                "POST",
                "/api/tool",
                &access,
                r#"{"name":"machine_info","arguments":{}}"#
            )
            .0,
            401
        );
        let machine_auth = format!("Authorization: Bearer {}\r\n", lab.agent_token);
        assert_eq!(
            request(
                &server,
                &mut lab,
                &network,
                "POST",
                "/api/tool",
                &machine_auth,
                r#"{"name":"machine_info","arguments":{}}"#
            )
            .0,
            409
        );
        drop(lab);
        fs::remove_dir_all(directory).unwrap();
    }
}
