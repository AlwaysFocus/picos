use std::{fs, process::Command};

#[test]
fn missing_direct_authorization_fails_before_creation_without_codex_or_api_key_fallback() {
    let directory = std::env::temp_dir().join(format!("picos-no-auth-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&directory).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_picos"))
        .env("PICOS_HOME", directory.join("home"))
        .env("PICOS_CODEX_BIN", directory.join("must-never-be-executed"))
        .env("OPENAI_API_KEY", "fixture-never-authorizes-subscription")
        .args([
            "new",
            "isolated",
            "--strict",
            "--model",
            "gpt-6-luna",
            "--prompt",
            "build a terminal",
            "--no-open",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("picos login --direct"), "{error}");
    assert!(!error.contains("Could not start Codex"));
    assert!(!directory.join("home/experiments").exists());
    fs::remove_dir_all(directory).unwrap();
}
