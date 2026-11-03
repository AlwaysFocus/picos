use serde_json::{Value, json};
use smol_os::server::Lab;
use std::{fs, path::PathBuf};
use uuid::Uuid;

struct TestDirectory(PathBuf);
impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("smol-server-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn lab(&self) -> Lab {
        Lab::new(self.0.clone(), "http://127.0.0.1:4317".into()).unwrap()
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const SIGNAL: &str = "mov r0, 65\nout r0, 0\nmov r0, 0xe3\nstore8 r0, VIDEO_BASE\nmov r1, 2048\nout r1, 2\nout r0, 3\nhalt\n";

fn execute_signal(lab: &mut Lab) -> Value {
    lab.action(&json!({"action":"assemble", "source": SIGNAL}))
        .unwrap();
    lab.action(&json!({"action":"run", "cycles":1000})).unwrap()
}
fn inspect(lab: &mut Lab, space: &str, address: u64, length: u64) -> Value {
    lab.action(&json!({"action":"inspect", "space":space, "address":address, "length":length}))
        .unwrap()["bytes"]
        .clone()
}

#[test]
fn assembly_reaches_real_serial_framebuffer_and_raw_disk() {
    let directory = TestDirectory::new();
    let mut lab = directory.lab();
    let result = execute_signal(&mut lab);
    assert_eq!(result["status"], "halted");
    assert_eq!(result["serial"], "A");
    assert_eq!(result["cycles"], 54);
    assert_eq!(result["instructions"], 8);
    let video_base = result["videoBase"].as_u64().unwrap();
    assert_eq!(inspect(&mut lab, "ram", video_base, 2), json!([227, 0]));
    assert_eq!(inspect(&mut lab, "disk", 2048, 2), json!([227, 0]));

    // Reboot clears the display, preserves raw disk, and does not refund cycles.
    let reboot = lab.action(&json!({"action":"reset"})).unwrap();
    assert_eq!(reboot["cycles"], 54);
    assert_eq!(reboot["serial"], "");
    assert_eq!(inspect(&mut lab, "ram", video_base, 1), json!([0]));
    assert_eq!(inspect(&mut lab, "disk", 2048, 1), json!([227]));
    let again = lab.action(&json!({"action":"run", "cycles":1000})).unwrap();
    assert_eq!(again["serial"], "A");
    assert_eq!(again["cycles"], 108);
}

#[test]
fn sealed_checkpoint_retains_guest_writes_cycles_and_seal_when_restored() {
    let directory = TestDirectory::new();
    let mut lab = directory.lab();
    lab.action(&json!({"action":"assemble", "source":SIGNAL}))
        .unwrap();
    lab.action(&json!({"action":"seal"})).unwrap();
    assert!(
        lab.action(&json!({"action":"assemble", "source":"halt"}))
            .is_err()
    );
    // Sealing blocks external bootstrap, not the guest's own device writes.
    let finished = lab.action(&json!({"action":"run", "cycles":1000})).unwrap();
    assert_eq!(finished["serial"], "A");
    assert_eq!(inspect(&mut lab, "disk", 2048, 1), json!([227]));
    let original_experiment = lab.state()["experiment"].clone();
    lab.action(&json!({"action":"checkpoint", "name":"sealed"}))
        .unwrap();
    lab.action(&json!({"action":"reset"})).unwrap();
    lab.action(&json!({"action":"run", "cycles":1000})).unwrap();
    assert_eq!(lab.state()["machine"]["cycles"], 108);

    let restored = lab
        .action(&json!({"action":"restore", "name":"sealed"}))
        .unwrap();
    assert_eq!(restored["machine"]["cycles"], 54);
    assert_eq!(restored["machine"]["sealed"], true);
    assert_eq!(restored["machine"]["serial"], "A");
    assert_eq!(restored["restoredFrom"], "sealed");
    assert_ne!(restored["experiment"], original_experiment);
    assert_eq!(
        inspect(
            &mut lab,
            "ram",
            restored["machine"]["videoBase"].as_u64().unwrap(),
            1
        ),
        json!([227])
    );
    assert!(
        lab.action(&json!({"action":"assemble", "source":"halt"}))
            .is_err()
    );
}

#[test]
fn saved_machine_restarts_with_identical_cpu_ram_disk_source_and_input() {
    let directory = TestDirectory::new();
    let mut first = directory.lab();
    execute_signal(&mut first);
    first
        .action(&json!({"action":"input", "text":"next\n"}))
        .unwrap();
    first.action(&json!({"action":"seal"})).unwrap();
    let before = first.state();
    first.save().unwrap();
    drop(first);

    let mut restarted = directory.lab();
    let after = restarted.state();
    assert_eq!(after["machine"], before["machine"]);
    assert_eq!(after["source"], before["source"]);
    assert_eq!(after["experiment"], before["experiment"]);
    assert_eq!(
        inspect(
            &mut restarted,
            "ram",
            after["machine"]["videoBase"].as_u64().unwrap(),
            1
        ),
        json!([227])
    );
    assert_eq!(inspect(&mut restarted, "disk", 2048, 1), json!([227]));
}

#[test]
fn rejected_configurations_and_source_do_not_replace_the_existing_machine() {
    let directory = TestDirectory::new();
    let mut lab = directory.lab();
    execute_signal(&mut lab);
    let before = lab.state();
    for invalid in [
        json!({"action":"new", "ramBytes":65535}),
        json!({"action":"new", "diskBytes":7}),
        json!({"action":"new", "clockHz":0}),
        json!({"action":"new", "maxCycles":0}),
        json!({"action":"new", "ramBytes":1.5}),
        json!({"action":"new", "diskBytes":-1}),
        json!({"action":"assemble", "source":"not_an_instruction r0, 7"}),
    ] {
        assert!(
            lab.action(&invalid).is_err(),
            "accepted invalid action: {invalid}"
        );
        let after = lab.state();
        for field in ["machine", "source", "experiment", "checkpoints"] {
            assert_eq!(
                after[field], before[field],
                "invalid action changed {field}: {invalid}"
            );
        }
    }
}

#[test]
fn checkpoint_names_cannot_escape_storage_or_overwrite_an_existing_checkpoint() {
    let directory = TestDirectory::new();
    let mut lab = directory.lab();
    for name in [
        "../escape",
        "/tmp/escape",
        "nested/name",
        ".hidden",
        "bad.name",
        "hello world",
        "é",
        &"x".repeat(49),
    ] {
        assert!(
            lab.action(&json!({"action":"checkpoint", "name":name}))
                .is_err(),
            "accepted {name}"
        );
        assert!(
            lab.action(&json!({"action":"restore", "name":name}))
                .is_err(),
            "accepted {name}"
        );
    }
    lab.action(&json!({"action":"checkpoint", "name":"known-good"}))
        .unwrap();
    let file = directory.0.join("checkpoints/known-good.json.gz");
    let original = fs::read(&file).unwrap();
    execute_signal(&mut lab);
    assert!(
        lab.action(&json!({"action":"checkpoint", "name":"known-good"}))
            .is_err()
    );
    assert_eq!(fs::read(file).unwrap(), original);
    assert_eq!(lab.state()["checkpoints"], json!(["known-good"]));
}

#[test]
fn cycle_allowance_is_not_refunded_by_reboot_or_assembly() {
    let directory = TestDirectory::new();
    let mut lab = directory.lab();
    lab.action(&json!({"action":"new", "ramBytes":65536, "diskBytes":4096, "clockHz":300000000, "maxCycles":7})).unwrap();
    lab.action(&json!({"action":"assemble", "source":"again: jmp again"}))
        .unwrap();
    for cycles in [json!(0), json!(-1), json!(1.5), json!(2000001)] {
        assert!(
            lab.action(&json!({"action":"run", "cycles":cycles}))
                .is_err()
        );
        assert_eq!(lab.state()["machine"]["cycles"], 0);
    }
    let exhausted = lab.action(&json!({"action":"run", "cycles":100})).unwrap();
    assert_eq!(exhausted["cycles"], 7);
    assert_eq!(exhausted["status"], "budget");
    lab.action(&json!({"action":"reset"})).unwrap();
    lab.action(&json!({"action":"assemble", "source":"mov r0, 42\nhalt"}))
        .unwrap();
    lab.action(&json!({"action":"step"})).unwrap();
    assert_eq!(lab.state()["machine"]["cycles"], 7);
    assert_eq!(lab.state()["machine"]["registers"][0], 0);
    assert_eq!(lab.state()["machine"]["status"], "budget");
    for request in [
        json!({"action":"inspect", "address":65536, "length":1}),
        json!({"action":"inspect", "space":"disk", "address":4096, "length":1}),
        json!({"action":"inspect", "address":0, "length":4097}),
    ] {
        assert!(lab.action(&request).is_err());
    }
}

#[cfg(unix)]
mod http {
    use super::*;
    use smol_os::mcp;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpStream,
        process::{Child, Command, Stdio},
        sync::mpsc,
        time::Duration,
    };

    struct ServerProcess(Child);
    impl Drop for ServerProcess {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn request(
        base: &str,
        method: &str,
        path: &str,
        origin: Option<&str>,
        body: &str,
    ) -> (u16, Vec<u8>) {
        let address = mcp::loopback_address(base).unwrap();
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let origin = origin
            .map(|value| format!("Origin: {value}\r\n"))
            .unwrap_or_default();
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{origin}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let boundary = response
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap();
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
        // The service promises length-delimited responses to its bounded MCP client.
        assert!(
            !headers
                .lines()
                .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked"))
        );
        (status, response[boundary + 4..].to_vec())
    }

    #[test]
    fn actual_http_server_serves_frame_and_rejects_cross_origin_mutation() {
        let directory = TestDirectory::new();
        let child = Command::new(env!("CARGO_BIN_EXE_picos"))
            .args(["--port", "0", "--data-dir"])
            .arg(&directory.0)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut process = ServerProcess(child);
        let stdout = process.0.stdout.take().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let result = reader.read_line(&mut line).map(|_| line.trim().to_owned());
            let _ = sender.send(result);
            // Keep the pipe open and drain later startup/output lines until exit.
            loop {
                line.clear();
                if !matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
                    break;
                }
            }
        });
        let base = receiver
            .recv_timeout(Duration::from_secs(20))
            .expect("server did not announce its URL")
            .unwrap();
        assert!(
            base.starts_with("http://127.0.0.1:"),
            "unexpected server output: {base}"
        );
        // This symbol table exceeds tiny_http's default chunking threshold.
        // Exercise the actual MCP HTTP client on a large JSON response too.
        let large_source = (0..5000)
            .map(|i| format!("symbol_{i:04}: .byte 0\n"))
            .collect::<String>();
        let assembled = mcp::post(
            &base,
            "/api/action",
            &json!({"action":"assemble", "source":large_source}),
            None,
        )
        .unwrap();
        assert!(assembled["symbols"].as_object().unwrap().len() >= 5000);
        mcp::post(
            &base,
            "/api/action",
            &json!({"action":"assemble", "source":SIGNAL}),
            None,
        )
        .unwrap();
        let result = mcp::post(
            &base,
            "/api/action",
            &json!({"action":"run", "cycles":1000}),
            None,
        )
        .unwrap();
        assert_eq!(result["serial"], "A");
        let (status, frame) = request(&base, "GET", "/api/frame", None, "");
        assert_eq!(status, 200);
        assert_eq!(frame.len(), 256 * 192);
        assert_eq!(&frame[..2], &[227, 0]);
        let (status, body) = request(
            &base,
            "POST",
            "/api/action",
            Some("https://unrelated.example"),
            r#"{"action":"reset"}"#,
        );
        assert_eq!(status, 403);
        assert!(String::from_utf8(body).unwrap().contains("Cross-origin"));
        let (status, body) = request(&base, "GET", "/api/state", None, "");
        assert_eq!(status, 200);
        let state: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(state["machine"]["serial"], "A");
        assert_eq!(state["machine"]["cycles"], 54);
        assert!(
            mcp::post(
                &base,
                "/api/tool",
                &json!({"name":"machine_info","arguments":{}}),
                None
            )
            .unwrap_err()
            .contains("capability")
        );
        // Exercise the server's graceful shutdown persistence, not just process kill.
        assert_eq!(
            unsafe { libc::kill(process.0.id() as libc::pid_t, libc::SIGTERM) },
            0
        );
        assert!(process.0.wait().unwrap().success());
        let restarted = directory.lab();
        assert_eq!(restarted.state()["machine"]["serial"], "A");
        assert_eq!(restarted.state()["machine"]["cycles"], 54);
    }
}

#[test]
fn activity_records_first_writes_guest_writes_and_survives_save() {
    let directory = TestDirectory::new();
    let mut lab = directory.lab();
    assert_eq!(lab.state()["memoryActivity"]["revision"], 0);
    execute_signal(&mut lab);
    let state = lab.state();
    let events = state["memoryActivity"]["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["kind"], "assemble");
    assert!(events[0]["changedBytes"].as_u64().unwrap() > 0);
    assert_eq!(events[1]["kind"], "run");
    assert_eq!(
        events[1]["ranges"][0]["address"],
        state["machine"]["videoBase"]
    );
    assert_eq!(events[1]["ranges"][0]["before"], json!([0]));
    assert_eq!(events[1]["ranges"][0]["after"], json!([227]));
    lab.action(&json!({"action":"inspect","address":0}))
        .unwrap();
    assert_eq!(lab.state()["memoryActivity"], state["memoryActivity"]);
    lab.save().unwrap();
    drop(lab);
    let restored = directory.lab();
    assert_eq!(restored.state()["memoryActivity"], state["memoryActivity"]);
}

#[test]
fn activity_retention_is_bounded_and_ids_monotonic() {
    let directory = TestDirectory::new();
    let mut lab = directory.lab();
    for n in 0..30 {
        lab.action(&json!({"action":"assemble", "source":format!("mov r0, {n}\nhalt")}))
            .unwrap();
    }
    let state = lab.state();
    let events = state["memoryActivity"]["events"].as_array().unwrap();
    assert_eq!(events.len(), 24);
    assert_eq!(events.last().unwrap()["id"], 30);
    assert_eq!(state["memoryActivity"]["revision"], 30);
}
