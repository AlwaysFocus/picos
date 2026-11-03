use smol_os::cli::{Command, ExperimentCommand, experiment_directory, megabytes, parse};
use std::path::{Path, PathBuf};
fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}
fn experiment(parts: &[&str]) -> ExperimentCommand {
    let Command::Experiment(command) = parse(&args(parts)).unwrap() else {
        panic!("Expected experiment command")
    };
    command
}
#[test]
fn canonical_cli_preserves_model_prompt_and_decimal_units() {
    let parsed = experiment(&[
        "new",
        "terminal",
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
    ]);
    assert_eq!(parsed.name.as_deref(), Some("terminal"));
    assert_eq!(parsed.model.as_deref(), Some("gpt-6-luna"));
    assert_eq!(parsed.mission.as_deref(), Some("build a terminal"));
    let config = parsed.config.unwrap();
    assert_eq!(config.ram_bytes, 24_000_000);
    assert_eq!(config.disk_bytes, 200_000_000);
    assert_eq!(config.clock_hz, 300_000_000);
    assert_eq!(config.max_cycles, 18_000_000_000);
    assert_eq!(parsed.port, 0);
    assert!(parsed.open_browser);
    assert_eq!(parsed.max_turns, 6);
}

#[test]
fn strict_and_direct_subscription_are_explicit_and_never_select_api_keys() {
    assert_eq!(parse(&args(&["models"])).unwrap(), Command::Models);
    let parsed = experiment(&[
        "new",
        "isolated",
        "--strict",
        "--model",
        "gpt-6-luna",
        "--prompt",
        "build a terminal",
    ]);
    assert!(parsed.strict);
    assert_eq!(parsed.model.as_deref(), Some("gpt-6-luna"));
    assert!(!experiment(&["new", "--prompt", "legacy"]).strict);
    assert!(experiment(&["resume", "isolated", "--strict"]).strict);
    assert!(parse(&args(&["new", "--strict", "--prompt", "build"])).is_err());
    assert!(parse(&args(&["new", "--strict=false", "--prompt", "build"])).is_err());
    assert_eq!(
        parse(&args(&["login", "--direct"])).unwrap(),
        Command::DirectLogin {
            status_only: false,
            open_browser: true
        }
    );
    assert_eq!(
        parse(&args(&["login", "--direct", "status"])).unwrap(),
        Command::DirectLogin {
            status_only: true,
            open_browser: false
        }
    );
    assert!(parse(&args(&["login", "--direct", "--api-key", "invalid"])).is_err());
}

#[test]
fn agent_turn_allowance_is_bounded_and_explicit_on_resume() {
    assert_eq!(
        experiment(&["new", "--prompt", "go", "--turns", "1"]).max_turns,
        1
    );
    assert_eq!(
        experiment(&["new", "prompt", "go", "turns", "24"]).max_turns,
        24
    );
    assert_eq!(
        experiment(&["resume", "terminal", "--prompt", "continue", "--turns", "3"]).max_turns,
        3
    );
    for invalid in ["0", "25", "1.5", "-1", "99999999999999999999999999999"] {
        assert!(parse(&args(&["new", "--prompt", "go", "--turns", invalid])).is_err());
    }
    assert!(parse(&args(&["resume", "terminal", "--turns", "3"])).is_err());
}
#[test]
fn keyword_and_equals_forms_match_canonical_flags() {
    let keyword = experiment(&[
        "new",
        "experiment",
        "cpu",
        "300",
        "ram",
        "24",
        "storage",
        "200",
        "model",
        "gpt-6-luna",
        "prompt",
        "build a terminal",
        "no-open",
    ]);
    let flags = experiment(&[
        "new",
        "experiment",
        "--cpu=300",
        "--ram=24",
        "--storage=200",
        "--model=gpt-6-luna",
        "--prompt=build a terminal",
        "--no-open",
    ]);
    assert_eq!(keyword, flags);
    assert!(!keyword.open_browser);
    let defaults = experiment(&["new", "--prompt", "draw"]);
    assert!(defaults.name.is_none());
    let config = defaults.config.unwrap();
    assert_eq!(config.ram_bytes, 8_000_000);
    assert_eq!(config.disk_bytes, 32_000_000);
}
#[test]
fn decimal_megabytes_are_exact_and_reject_ambiguous_or_overflowing_inputs() {
    assert_eq!(megabytes("0.065536", "RAM").unwrap(), 65536);
    assert_eq!(megabytes("24.125", "RAM").unwrap(), 24_125_000);
    assert_eq!(megabytes("0.000001", "Storage").unwrap(), 1);
    for invalid in [
        "NaN",
        "inf",
        "-1",
        "1e6",
        "24MB",
        "1.0000001",
        "1.",
        ".5",
        "1.2.3",
        "18446744073709551615",
    ] {
        assert!(megabytes(invalid, "RAM").is_err(), "Accepted {invalid}");
    }
}
#[test]
fn invalid_and_duplicate_options_fail_before_launch() {
    let invalid: Vec<Vec<&str>> = vec![
        vec!["new"],
        vec!["new", "--prompt", " "],
        vec!["new", "--prompt"],
        vec!["new", "--prompt", "go", "cpu", "0"],
        vec!["new", "--prompt", "go", "cpu", "0.5"],
        vec!["new", "--prompt", "go", "--cpu", "300", "cpu", "400"],
        vec!["new", "--prompt", "go", "--port", "65536"],
        vec!["new", "--prompt", "go", "--ram", "0.001"],
        vec!["new", "--prompt", "go", "--cycles", "0"],
        vec!["new", "--prompt", "go", "--no-open=true"],
        vec!["new", "../escape", "--prompt", "go"],
        vec!["new", "--prompt", "go", "--model", "gpt\n6"],
        vec!["new", "--prompt", "go", "--cpus", "2"],
        vec!["unknown"],
        vec!["list", "anything"],
        vec!["login", "--with-api-key"],
    ];
    for input in invalid {
        assert!(parse(&args(&input)).is_err(), "Accepted {input:?}");
    }
}
#[test]
fn resume_is_read_only_until_a_new_prompt_is_explicit() {
    let resume = experiment(&["resume", "terminal", "--no-open", "--port", "4319"]);
    assert!(resume.resume);
    assert!(resume.config.is_none());
    assert!(resume.mission.is_none());
    assert!(resume.model.is_none());
    assert_eq!(resume.port, 4319);
    assert!(parse(&args(&["resume"])).is_err());
    assert!(parse(&args(&["resume", "terminal", "--cpu", "400"])).is_err());
    assert!(parse(&args(&["resume", "terminal", "--model", "gpt-6-luna"])).is_err());
    let resume = experiment(&[
        "resume",
        "terminal",
        "--prompt",
        "continue",
        "--model",
        "gpt-6-luna",
    ]);
    assert_eq!(resume.mission.as_deref(), Some("continue"));
}
#[test]
fn explicit_directories_are_preserved_and_named_directories_cannot_traverse() {
    let root = Path::new("/tmp/picos-home");
    assert_eq!(
        experiment_directory(root, "terminal", None).unwrap(),
        PathBuf::from("/tmp/picos-home/experiments/terminal")
    );
    assert_eq!(
        experiment_directory(root, "terminal", Some("custom path".into())).unwrap(),
        PathBuf::from("custom path")
    );
    assert!(experiment_directory(root, "../escape", None).is_err());
    let parsed = experiment(&["new", "--prompt", "go", "--dir", "/tmp/space in path"]);
    assert_eq!(parsed.data_dir, Some(PathBuf::from("/tmp/space in path")));
}
#[test]
fn login_and_help_are_explicit_and_do_not_accept_api_credentials() {
    assert_eq!(parse(&[]).unwrap(), Command::Help);
    assert_eq!(parse(&args(&["--version"])).unwrap(), Command::Version);
    assert_eq!(
        parse(&args(&["login"])).unwrap(),
        Command::Login {
            device_code: false,
            status_only: false
        }
    );
    assert_eq!(
        parse(&args(&["login", "status"])).unwrap(),
        Command::Login {
            device_code: false,
            status_only: true
        }
    );
    assert_eq!(
        parse(&args(&["login", "--device-code"])).unwrap(),
        Command::Login {
            device_code: true,
            status_only: false
        }
    );
    assert_eq!(parse(&args(&["mcp"])).unwrap(), Command::Mcp);
    assert!(parse(&args(&["login", "--with-api-key"])).is_err());
}

#[test]
fn experiment_modes_and_turn_disclosure_are_explicit_and_validated() {
    use smol_os::session::RunMode;
    let defaults = experiment(&["new", "--prompt", "go"]);
    assert_eq!(defaults.mode, None);
    assert_eq!(defaults.tell_turns, None);
    for (name, mode) in [
        ("standard", RunMode::Standard),
        ("resource-limit", RunMode::ResourceLimit),
        ("maximize", RunMode::Maximize),
    ] {
        let parsed = experiment(&[
            "new",
            "--prompt",
            "go",
            "--mode",
            name,
            "--tell-turns",
            "false",
        ]);
        assert_eq!(parsed.mode, Some(mode));
        assert_eq!(parsed.tell_turns, Some(false));
    }
    let resumed = experiment(&[
        "resume",
        "terminal",
        "mode",
        "maximize",
        "tell-turns",
        "true",
    ]);
    assert_eq!(resumed.mode, Some(RunMode::Maximize));
    assert_eq!(resumed.tell_turns, Some(true));
    assert_eq!(resumed.mission, None);
    for invalid in ["fast", "Standard", ""] {
        assert!(parse(&args(&["new", "--prompt", "go", "--mode", invalid])).is_err());
    }
    for invalid in ["yes", "1", "TRUE", ""] {
        assert!(parse(&args(&["new", "--prompt", "go", "--tell-turns", invalid])).is_err());
    }
    assert!(parse(&args(&["new", "--prompt", "go", "--tell-turns"])).is_err());
    assert!(
        parse(&args(&[
            "new", "--prompt", "go", "--mode", "standard", "--mode", "maximize"
        ]))
        .is_err()
    );
}

#[test]
fn local_network_sharing_is_explicit_for_new_and_resume() {
    assert!(!experiment(&["new", "--prompt", "go"]).lan);
    assert!(!experiment(&["resume", "saved"]).lan);
    assert!(experiment(&["new", "--prompt", "go", "--lan"]).lan);
    let resumed = experiment(&["resume", "saved", "--lan"]);
    assert!(resumed.lan);
    assert!(resumed.mission.is_none());
    assert!(parse(&args(&["resume", "saved", "--lan=false"])).is_err());
    assert!(parse(&args(&["resume", "saved", "--lan", "--lan"])).is_err());
}
