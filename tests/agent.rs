use smol_os::agent::{AgentRunner, agent_prompt, build_codex_args, subscription_environment};
use smol_os::mcp::tools;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

#[test]
fn codex_uses_subscription_and_only_machine_bridge() {
    let args = build_codex_args(
        Path::new("/tmp/a path"),
        Some("chosen-model"),
        Path::new("/tmp/smol"),
    );
    for expected in [
        "--ignore-user-config",
        "--ignore-rules",
        "forced_login_method=\"chatgpt\"",
        "features.shell_tool=false",
        "features.unified_exec=false",
        "features.apps=false",
        "features.plugins=false",
        "features.code_mode_host=true",
        "web_search=\"disabled\"",
        "mcp_servers.smol.command=\"/tmp/smol\"",
        "mcp_servers.smol.args=[\"mcp\"]",
    ] {
        assert!(args.iter().any(|arg| arg == expected), "missing {expected}");
    }
    assert_eq!(args.last().unwrap(), "-");
    let index = args.iter().position(|arg| arg == "-C").unwrap();
    assert_eq!(args[index + 1], "/tmp/a path");
    let index = args.iter().position(|arg| arg == "--model").unwrap();
    assert_eq!(args[index + 1], "chosen-model");
    assert!(
        !build_codex_args(Path::new("/tmp"), None, Path::new("/tmp/smol"))
            .contains(&"--model".into())
    );
    assert!(
        !args
            .iter()
            .any(|arg| arg.contains("Bearer") || arg.contains("API_KEY="))
    );
    assert!(args.contains(&"approval_policy=\"never\"".into()));
    assert!(args.contains(&"mcp_servers.smol.default_tools_approval_mode=\"prompt\"".into()));
    let tool_names: Vec<_> = tools()
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(args.contains(&format!(
        "mcp_servers.smol.enabled_tools={}",
        serde_json::json!(tool_names)
    )));
    let approvals: Vec<_> = args
        .iter()
        .filter(|arg| arg.ends_with("approval_mode=\"approve\""))
        .collect();
    assert_eq!(approvals.len(), 9);
    for name in tool_names {
        assert!(approvals.contains(&&format!(
            "mcp_servers.smol.tools.{name}.approval_mode=\"approve\""
        )));
    }
}

#[test]
fn api_credentials_are_removed_but_existing_auth_location_is_preserved() {
    let pairs = [
        ("PATH", "/bin"),
        ("CODEX_HOME", "/auth"),
        ("OPENAI_API_KEY", "secret"),
        ("CODEX_API_KEY", "secret"),
        ("ANTHROPIC_API_KEY", "secret"),
        ("OPENAI_BASE_URL", "elsewhere"),
        ("CUSTOM_API_TOKEN", "secret"),
    ];
    let environment =
        subscription_environment(pairs.map(|(key, value)| (key.to_owned(), value.to_owned())));
    assert_eq!(environment.len(), 2);
    assert_eq!(environment["CODEX_HOME"], "/auth");
    assert_eq!(environment["PATH"], "/bin");
}

#[test]
fn resident_prompt_distinguishes_real_guest_execution_from_external_orchestration() {
    let prompt = agent_prompt("Build a shell");
    assert!(prompt.contains("First call machine_info"));
    assert!(prompt.contains("machine_screen"));
    assert!(prompt.contains("external orchestration"));
    assert!(prompt.contains("does not refund consumed cycles"));
    assert!(prompt.contains("brief user-facing progress updates"));
    assert!(prompt.contains("observed test result"));
    assert!(prompt.contains("do not include private chain-of-thought"));
    assert!(prompt.ends_with("Build a shell"));
}

#[test]
fn invalid_missions_and_remote_bridge_never_start_a_process() {
    let (events, _receiver) = mpsc::channel();
    let mut runner = AgentRunner::new(
        "https://example.com".into(),
        "test-token".into(),
        PathBuf::from("/tmp/smol-agent-test"),
        events,
    );
    assert_eq!(runner.status()["status"], "idle");
    assert!(runner.start("", None).unwrap_err().contains("Mission"));
    assert!(
        runner
            .start("hello", Some("bad\nname"))
            .unwrap_err()
            .contains("model")
    );
    assert!(
        runner
            .start("hello", None)
            .unwrap_err()
            .contains("loopback")
    );
    assert!(!runner.is_active());
    runner.stop();
    assert_eq!(runner.status()["status"], "idle");
}
