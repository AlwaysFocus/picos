//! CLI-to-server acceptance with a disposable Codex stand-in; no inference call.
#![cfg(unix)]

use serde_json::Value;
use smol_os::mcp;
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

struct Fixture {
    root: PathBuf,
    fake_codex: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("picos-experiment-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let fake_codex = root.join("codex-stub");
        fs::write(
            &fake_codex,
            r##"#!/bin/sh
set -eu
case "$1" in
  login)
    printf 'login\n' >> "$PICOS_TEST_CALLS"
    printf 'Logged in using ChatGPT\n'
    ;;
  exec)
    printf 'exec\n' >> "$PICOS_TEST_CALLS"
    printf '%s\n' "$@" > "$PICOS_TEST_ARGS"
    cat > /dev/null
    printf '%s\n' \
      '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"machine_assemble","arguments":{"source":"mov r0, 65\nout r0, 0\nhalt"}}}' \
      '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"machine_run","arguments":{"cycles":100}}}' \
      | "$PICOS_TEST_BIN" mcp > "$PICOS_TEST_BRIDGE"
    printf '{"type":"turn.completed","usage":{}}\n'
    ;;
  *) exit 3 ;;
esac
"##,
        )
        .unwrap();
        fs::set_permissions(&fake_codex, fs::Permissions::from_mode(0o700)).unwrap();
        Self { root, fake_codex }
    }
    fn experiment(&self) -> PathBuf {
        self.root.join("experiment")
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_picos"));
        command
            .env("SMOL_CODEX_BIN", &self.fake_codex)
            .env("PICOS_CODEX_BIN", &self.fake_codex)
            .env("PICOS_TEST_BIN", env!("CARGO_BIN_EXE_picos"))
            .env("PICOS_TEST_CALLS", self.root.join("calls"))
            .env("PICOS_TEST_ARGS", self.root.join("args"))
            .env("PICOS_TEST_BRIDGE", self.root.join("bridge"))
            .stdin(Stdio::null());
        command
    }
    fn new_command(&self) -> Command {
        let mut command = self.command();
        command
            .args([
                "new",
                "acceptance",
                "--cpu",
                "300",
                "--ram",
                "24",
                "--storage",
                "200",
                "--model",
                "gpt-6-luna",
                "--prompt",
                "build a terminal",
                "--turns",
                "1",
                "--no-open",
                "--dir",
            ])
            .arg(self.experiment());
        command
    }
    fn resume_command(&self) -> Command {
        let mut command = self.command();
        command
            .args(["resume", "acceptance", "--no-open", "--dir"])
            .arg(self.experiment());
        command
    }
    fn calls(&self) -> String {
        fs::read_to_string(self.root.join("calls")).unwrap_or_default()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Process(Child);
impl Process {
    fn launch(mut command: Command) -> (Self, String) {
        let mut process = Self(
            command
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let stdout = process.0.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let result = reader.read_line(&mut line).map(|_| line.trim().to_owned());
            let _ = sender.send(result);
            // Drain terminal events so stdout never blocks the experiment.
            loop {
                line.clear();
                if !matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                    break;
                }
            }
        });
        let base = receiver
            .recv_timeout(Duration::from_secs(60))
            .expect("experiment did not announce its observer URL")
            .unwrap();
        assert!(
            base.starts_with("http://127.0.0.1:"),
            "unexpected startup: {base}"
        );
        (process, base)
    }
    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "process did not exit within {timeout:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn interrupt_and_save(&mut self) {
        assert_eq!(unsafe { libc::kill(self.0.id() as i32, libc::SIGINT) }, 0);
        assert!(self.wait(Duration::from_secs(60)).success());
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if matches!(self.0.try_wait(), Ok(Some(_))) {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn request(base: &str, method: &str, path: &str, body: &str) -> (u16, Value) {
    request_headers(base, method, path, body, "")
}
fn request_headers(
    base: &str,
    method: &str,
    path: &str,
    body: &str,
    headers: &str,
) -> (u16, Value) {
    let address = mcp::loopback_address(base).unwrap();
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    let mut response = Vec::new();
    stream.take(1024 * 1024).read_to_end(&mut response).unwrap();
    let boundary = response.windows(4).position(|p| p == b"\r\n\r\n").unwrap();
    let headers = std::str::from_utf8(&response[..boundary]).unwrap();
    let status = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (
        status,
        serde_json::from_slice(&response[boundary + 4..]).unwrap(),
    )
}
fn state(base: &str) -> Value {
    let (status, value) = request(base, "GET", "/api/state", "");
    assert_eq!(status, 200);
    value
}
fn rejection(mut command: Command, expected: &str) {
    let mut process = Process(
        command
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    assert!(!process.wait(Duration::from_secs(10)).success());
    let mut error = String::new();
    process
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut error)
        .unwrap();
    assert!(error.contains(expected), "unexpected error: {error}");
}
fn file(path: impl AsRef<Path>) -> Vec<u8> {
    fs::read(path).unwrap()
}

#[test]
fn cli_creates_requested_hardware_observes_live_ram_and_resumes_without_inference() {
    // One real 24 MB / 200 MB machine runs at a time; Codex is a local fixture.
    let fixture = Fixture::new();
    let (mut first, base) = Process::launch(fixture.new_command());
    let deadline = Instant::now() + Duration::from_secs(45);
    let completed = loop {
        let value = state(&base);
        if value["agent"]["status"] == "completed" {
            break value;
        }
        assert!(
            Instant::now() < deadline,
            "fixture turn did not complete: {value}"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(completed["machine"]["config"]["clockHz"], 300_000_000_u64);
    assert_eq!(completed["machine"]["config"]["ramBytes"], 24_000_000);
    assert_eq!(completed["machine"]["config"]["diskBytes"], 200_000_000);
    assert_eq!(completed["observerOnly"], true);
    assert_eq!(completed["experimentDetails"]["name"], "acceptance");
    assert_eq!(
        completed["experimentDetails"]["mission"],
        "build a terminal"
    );
    assert_eq!(completed["experimentDetails"]["model"], "gpt-6-luna");
    assert_eq!(completed["agent"]["model"], "gpt-6-luna");
    assert_eq!(completed["machine"]["serial"], "A");
    assert_eq!(completed["machine"]["cycles"], 18);
    assert_eq!(completed["agent"]["toolCalls"], 2);
    assert!(
        completed.get("source").is_none(),
        "observer should expose RAM, not an editable source buffer"
    );
    let arguments = fs::read_to_string(fixture.root.join("args")).unwrap();
    assert!(arguments.contains("--model\ngpt-6-luna\n"));
    let (status, view) = request(&base, "GET", "/api/disassembly?address=0&length=24", "");
    assert_eq!(status, 200);
    assert_eq!(view["rows"][0]["assembly"], "mov r0, 65");
    assert_eq!(
        view["rows"][0]["bytes"],
        serde_json::json!([3, 0, 0, 0, 65, 0, 0, 0])
    );
    assert_eq!(view["rows"][1]["assembly"], "out r0, 0");
    assert_eq!(view["rows"][2]["assembly"], "halt");
    assert_eq!(view["pc"], completed["machine"]["pc"]);
    for (path, body) in [
        ("/api/action", r#"{"action":"reset"}"#),
        (
            "/api/agent/start",
            r#"{"mission":"replace this experiment"}"#,
        ),
    ] {
        assert_eq!(request(&base, "POST", path, body).0, 403);
    }
    let metadata = file(fixture.experiment().join("experiment.json"));
    rejection(fixture.new_command(), "already exists");
    rejection(fixture.resume_command(), "already running");
    assert_eq!(file(fixture.experiment().join("experiment.json")), metadata);
    let calls = fixture.calls();
    assert!(calls.contains("exec\n"));
    first.interrupt_and_save();
    drop(first);

    let (mut resumed, resumed_base) = Process::launch(fixture.resume_command());
    let restored = state(&resumed_base);
    assert_eq!(restored["machine"], completed["machine"]);
    assert_eq!(restored["experiment"], completed["experiment"]);
    assert_eq!(restored["observerOnly"], true);
    assert_eq!(restored["agent"]["status"], "idle");
    assert_eq!(restored["experimentDetails"]["model"], "gpt-6-luna");
    assert_eq!(
        fixture.calls(),
        calls,
        "resume must not invoke login or start another model turn"
    );
    resumed.interrupt_and_save();
}

#[test]
fn session_continues_across_processes_and_preserves_total_budgets() {
    let fixture = Fixture::new();
    let mut command = fixture.command();
    command
        .args([
            "new",
            "iterations",
            "--ram",
            "0.065536",
            "--storage",
            "0.004096",
            "--prompt",
            "build a terminal",
            "--turns",
            "2",
            "--no-open",
            "--dir",
        ])
        .arg(fixture.experiment());
    let (mut process, base) = Process::launch(command);
    let deadline = Instant::now() + Duration::from_secs(30);
    let final_state = loop {
        let value = state(&base);
        if value["agent"]["session"]["status"] == "budget" {
            break value;
        }
        assert!(
            Instant::now() < deadline,
            "session failed to finish: {value}"
        );
        thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(final_state["agent"]["session"]["turn"], 2);
    assert_eq!(final_state["agent"]["toolCalls"], 4);
    assert_eq!(final_state["machine"]["cycles"], 36);
    assert_eq!(
        fixture
            .calls()
            .lines()
            .filter(|line| *line == "exec")
            .count(),
        2
    );
    assert!(
        !final_state["memoryActivity"]["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    process.interrupt_and_save();
}

fn small_experiment(fixture: &Fixture, mode: &str, cycles: &str) -> Command {
    let mut command = fixture.command();
    command
        .args([
            "new",
            "policy",
            "--ram",
            "0.065536",
            "--storage",
            "0.004096",
            "--prompt",
            "build a terminal",
            "--turns",
            "1",
            "--mode",
            mode,
            "--cycles",
            cycles,
            "--no-open",
            "--dir",
        ])
        .arg(fixture.experiment());
    command
}
fn wait_session(base: &str, turn: u64, reason: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let value = state(base);
        if value["agent"]["session"]["turn"] == turn
            && value["agent"]["session"]["stopReason"] == reason
            && !["starting", "running", "stopping"]
                .contains(&value["agent"]["status"].as_str().unwrap_or(""))
        {
            return value;
        }
        assert!(Instant::now() < deadline, "session did not pause: {value}");
        thread::sleep(Duration::from_millis(50));
    }
}
#[test]
fn browser_turn_grants_require_operator_capability_and_preserve_session_on_resume() {
    let fixture = Fixture::new();
    let (mut process, base) = Process::launch(small_experiment(&fixture, "standard", "1000"));
    let finished = wait_session(&base, 1, "turn_limit");
    let headers = format!(
        "X-Picos-Operator: {}\r\n",
        finished["operatorToken"].as_str().unwrap()
    );
    assert_eq!(
        request(&base, "POST", "/api/session/turns", r#"{"turns":2}"#).0,
        401
    );
    assert_eq!(
        request_headers(
            &base,
            "POST",
            "/api/session/turns",
            r#"{"turns":2}"#,
            &format!("{headers}Origin: https://unrelated.example\r\n")
        )
        .0,
        403
    );
    assert_eq!(
        request_headers(
            &base,
            "POST",
            "/api/session/settings",
            r#"{"tellTurns":false}"#,
            &headers
        )
        .0,
        200
    );
    assert_eq!(
        request_headers(
            &base,
            "POST",
            "/api/session/turns",
            r#"{"turns":2}"#,
            &headers
        )
        .0,
        200
    );
    let extended = wait_session(&base, 3, "turn_limit");
    assert_eq!(extended["agent"]["toolCalls"], 6);
    assert_eq!(extended["machine"]["cycles"], 54);
    assert_eq!(extended["agent"]["session"]["tellTurns"], false);
    process.interrupt_and_save();
    let calls = fixture.calls();
    let (mut resumed, resume_base) = Process::launch(fixture.resume_command());
    let restored = state(&resume_base);
    assert_eq!(restored["agent"]["session"]["turn"], 3);
    assert_eq!(restored["agent"]["session"]["tellTurns"], false);
    assert_eq!(restored["agent"]["toolCalls"], 6);
    assert_eq!(restored["machine"]["cycles"], 54);
    assert_eq!(fixture.calls(), calls);
    assert_ne!(restored["operatorToken"], finished["operatorToken"]);
    resumed.interrupt_and_save();
}
#[test]
fn resource_mode_passes_turn_ceiling_and_stops_immediately_at_cycle_boundary() {
    let fixture = Fixture::new();
    let (mut process, base) = Process::launch(small_experiment(&fixture, "resource-limit", "40"));
    let stopped = wait_session(&base, 3, "resource_limit");
    assert!(stopped["agent"]["session"]["maxTurns"].is_null());
    assert!(stopped["agent"]["maxToolCalls"].is_null());
    assert_eq!(stopped["machine"]["status"], "budget");
    assert_eq!(stopped["machine"]["cycles"], 37);
    let headers = format!(
        "X-Picos-Operator: {}\r\n",
        stopped["operatorToken"].as_str().unwrap()
    );
    assert_eq!(
        request_headers(&base, "POST", "/api/session/continue", "{}", &headers).0,
        400
    );
    process.interrupt_and_save();
}
#[test]
fn resource_mode_ram_breach_revokes_further_machine_calls() {
    let fixture = Fixture::new();
    fs::write(&fixture.fake_codex,r##"#!/bin/sh
set -eu
if [ "$1" = login ]; then echo 'Logged in using ChatGPT'; exit 0; fi
cat > /dev/null
printf '%s\n' \
'{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"machine_write","arguments":{"space":"ram","address":65536,"bytes":[123]}}}' \
'{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"machine_write","arguments":{"space":"ram","address":0,"bytes":[123]}}}' \
| "$PICOS_TEST_BIN" mcp > "$PICOS_TEST_BRIDGE"
printf '{"type":"turn.completed"}\n'
"##).unwrap();
    let (mut process, base) = Process::launch(small_experiment(&fixture, "resource-limit", "1000"));
    let stopped = wait_session(&base, 1, "resource_limit");
    assert_eq!(stopped["agent"]["toolCalls"], 1);
    assert_eq!(stopped["machine"]["cycles"], 0);
    assert_eq!(
        request(&base, "GET", "/api/disassembly?address=0&length=8", "").1["rows"][0]["assembly"],
        "halt"
    );
    process.interrupt_and_save();
}

#[test]
fn journal_shows_exact_submitted_goal_public_reports_and_persists_without_reasoning() {
    let fixture = Fixture::new();
    fs::write(&fixture.fake_codex,r##"#!/bin/sh
set -eu
if [ "$1" = login ]; then echo 'Logged in using ChatGPT'; exit 0; fi
cat > "$PICOS_TEST_PROMPT"
printf '%s\n' \
'{"type":"item.completed","item":{"type":"reasoning","text":"PRIVATE_REASONING_SENTINEL"}}' \
'{"type":"item.completed","item":{"type":"agent_message","channel":"analysis","text":"PRIVATE_ANALYSIS_SENTINEL"}}' \
'UNTYPED_PRIVATE_SENTINEL' \
'{"type":"item.completed","item":{"type":"agent_message","text":"Public progress sample: I will test keyboard echo next."}}' \
'{"type":"turn.completed","usage":{}}'
"##).unwrap();
    let mut command = small_experiment(&fixture, "standard", "1000");
    command.env("PICOS_TEST_PROMPT", fixture.root.join("submitted-prompt"));
    let (mut process, base) = Process::launch(command);
    let finished = wait_session(&base, 1, "turn_limit");
    let (status, journal) = request(&base, "GET", "/api/agent/journal", "");
    assert_eq!(status, 200);
    assert_eq!(journal["mission"], "build a terminal");
    assert_eq!(
        journal["turns"][0]["prompt"],
        fs::read_to_string(fixture.root.join("submitted-prompt")).unwrap()
    );
    let entries = journal["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| {
        e["type"] == "agent.message"
            && e["message"]
                .as_str()
                .unwrap()
                .contains("Public progress sample")
    }));
    assert!(!journal.to_string().contains("PRIVATE_"));
    assert!(
        !journal
            .to_string()
            .contains(finished["operatorToken"].as_str().unwrap())
    );
    let cursor = journal["latestId"].as_u64().unwrap();
    assert!(
        request(
            &base,
            "GET",
            &format!("/api/agent/journal?after={cursor}"),
            ""
        )
        .1["entries"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        request(&base, "GET", "/api/agent/journal?after=-1", "").0,
        400
    );
    process.interrupt_and_save();
    let (mut resumed, resume_base) = Process::launch(fixture.resume_command());
    let restored = request(&resume_base, "GET", "/api/agent/journal", "").1;
    assert_eq!(restored["turns"], journal["turns"]);
    assert!(
        restored["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "agent.message")
    );
    assert!(
        fs::read_to_string(fixture.experiment().join("journal.jsonl"))
            .unwrap()
            .contains("Public progress sample")
    );
    resumed.interrupt_and_save();
}
