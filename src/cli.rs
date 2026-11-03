//! Human-facing commands. Hardware units are decimal MHz and MB.
use crate::{
    agent::subscription_environment,
    mcp, server,
    session::{DEFAULT_TURNS, MAX_TURNS, RunMode},
    vm::Config,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

pub const HELP: &str = "picos — a small machine, an open-ended experiment

Usage:
  picos login [--device-code]       Sign in with your ChatGPT account
  picos login status                Check the current sign-in
  picos login --direct              Sign in for strict, direct ChatGPT experiments
  picos login --direct status       Check picos' own subscription authorization
  picos models                     List models for direct ChatGPT experiments
  picos new [NAME] [OPTIONS]         Create a blank machine and start its agent
  picos resume NAME [OPTIONS]        Reopen a saved machine without restarting its agent
  picos list                        List saved experiments

Create an experiment:
  picos new terminal --cpu 300 --ram 24 --storage 200 \\\n    --model gpt-6-luna --prompt \"build a terminal\"

  The keyword form works too:
  picos new experiment cpu 300 ram 24 storage 200 model gpt-6-luna prompt \"build a terminal\"

Options:
  --cpu MHZ          Virtual CPU frequency (default: 300 MHz)
  --ram MB           Guest memory (default: 8 MB)
  --storage MB       Raw guest storage (default: 32 MB)
  --model NAME       Exact model slug; required for new strict experiments
  --prompt TEXT      What the agent should build (required for new)
  --cycles COUNT     Total cycle allowance (default: 60 virtual seconds)
  --turns COUNT      Agent iteration allowance, 1..24 (default: 6)
  --mode MODE        standard, resource-limit, or maximize (default: standard)
  --tell-turns BOOL  Tell the agent its turn allowance: true or false (default: true)
  --strict           Use the VM-only Rust harness and direct ChatGPT subscription
  --lan              Share the observer on your local network with a private access link
  --port PORT        Observer port (default: choose an unused local port)
  --no-open          Print the observer URL without opening a browser
  --dir PATH         Use this experiment directory instead of the default

RAM and storage use decimal MB: 1 MB = 1,000,000 bytes.
The virtual clock is an instruction-cost model, not host CPU performance.
The experiment stays in the foreground. Ctrl+C saves it and stops the agent.
Experiments are stored in $PICOS_HOME/experiments or ~/.picos/experiments.
Resource-limit mode runs until machine resources stop it; --turns does not cap this mode.
Resume accepts --lan, --port, --no-open, --dir, and an optional new --prompt/--model/--turns.
--mode and --tell-turns can update saved settings without starting inference.
Strict mode requires picos login --direct and an explicit --model for a new experiment.
Strict experiments never fall back to Codex or API-key billing; resume preserves strict mode.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExperimentCommand {
    pub name: Option<String>,
    pub data_dir: Option<PathBuf>,
    pub port: u16,
    pub config: Option<Config>,
    pub mission: Option<String>,
    pub model: Option<String>,
    pub max_turns: u64,
    pub mode: Option<RunMode>,
    pub tell_turns: Option<bool>,
    pub strict: bool,
    pub lan: bool,
    pub open_browser: bool,
    pub resume: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Version,
    Login {
        device_code: bool,
        status_only: bool,
    },
    DirectLogin {
        status_only: bool,
        open_browser: bool,
    },
    Models,
    Experiment(ExperimentCommand),
    List,
    Mcp,
    LegacyServe {
        port: u16,
        data_dir: PathBuf,
    },
}
fn key(token: &str) -> &str {
    token.strip_prefix("--").unwrap_or(token)
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
fn parse_integer(value: &str, label: &str) -> Result<u64, String> {
    if value.is_empty() || !value.bytes().all(|c| c.is_ascii_digit()) {
        return Err(format!("{label} must be a whole number."));
    }
    value.parse().map_err(|_| format!("{label} is too large."))
}
/// Parse decimal MB without floating-point rounding or ambiguous suffixes.
pub fn megabytes(value: &str, label: &str) -> Result<usize, String> {
    let mut pieces = value.split('.');
    let whole = pieces.next().unwrap_or("");
    let fraction = pieces.next();
    if pieces.next().is_some() || whole.is_empty() || !whole.bytes().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "{label} must be a number in decimal MB, such as 24 or 0.5."
        ));
    }
    let mut bytes = parse_integer(whole, label)?
        .checked_mul(1_000_000)
        .ok_or_else(|| format!("{label} is too large."))?;
    if let Some(fraction) = fraction {
        if fraction.is_empty()
            || fraction.len() > 6
            || !fraction.bytes().all(|c| c.is_ascii_digit())
        {
            return Err(format!(
                "{label} accepts up to six decimal places (whole bytes)."
            ));
        }
        let part = parse_integer(fraction, label)? * 10_u64.pow(6 - fraction.len() as u32);
        bytes = bytes
            .checked_add(part)
            .ok_or_else(|| format!("{label} is too large."))?;
    }
    usize::try_from(bytes).map_err(|_| format!("{label} is too large for this machine."))
}
fn options(
    args: &[String],
    allowed: &[&str],
    allow_name: bool,
) -> Result<(Option<String>, BTreeMap<String, String>), String> {
    let mut values = BTreeMap::new();
    let mut name = None;
    let mut index = 0;
    if let Some(first) = args.first().filter(|_| allow_name) {
        if !first.starts_with('-')
            && !allowed.contains(&key(first.split('=').next().unwrap_or(first)))
        {
            if !valid_name(first) {
                return Err("Experiment names use 1–48 letters, numbers, hyphens or underscores, beginning with a letter or number.".into());
            }
            name = Some(first.clone());
            index += 1;
        }
    }
    while index < args.len() {
        let token = &args[index];
        let (option, inline) = token
            .split_once('=')
            .map_or((token.as_str(), None), |(k, v)| (k, Some(v)));
        let option = key(option);
        if !allowed.contains(&option) {
            return Err(format!(
                "Unknown option {token:?}. Use picos --help for examples."
            ));
        }
        if values.contains_key(option) {
            return Err(format!("Option --{option} was supplied more than once."));
        }
        if matches!(option, "no-open" | "strict" | "lan") {
            if inline.is_some() {
                return Err(format!("--{option} is a switch and takes no value."));
            }
            values.insert(option.into(), "true".into());
        } else {
            let value = match inline {
                Some(value) => value.to_owned(),
                None => {
                    index += 1;
                    let value = args
                        .get(index)
                        .ok_or_else(|| format!("Missing value for --{option}."))?;
                    if value.starts_with("--") {
                        return Err(format!("Missing value for --{option}."));
                    }
                    value.clone()
                }
            };
            if value.is_empty() {
                return Err(format!("--{option} cannot be empty."));
            }
            values.insert(option.into(), value);
        }
        index += 1;
    }
    Ok((name, values))
}
pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some(command) = args.first().map(String::as_str) else {
        return Ok(Command::Help);
    };
    match command {
        "--help" | "-h" | "help" => Ok(Command::Help),
        "--version" | "-V" => Ok(Command::Version),
        "mcp" if args.len() == 1 => Ok(Command::Mcp),
        "list" | "ls" if args.len() == 1 => Ok(Command::List),
        "models" if args.len() == 1 => Ok(Command::Models),
        "login" => match &args[1..] {
            [option] if option == "--direct" => Ok(Command::DirectLogin {
                status_only: false,
                open_browser: true,
            }),
            [option, status] if option == "--direct" && status == "status" => {
                Ok(Command::DirectLogin {
                    status_only: true,
                    open_browser: false,
                })
            }
            [status, option] if status == "status" && option == "--direct" => {
                Ok(Command::DirectLogin {
                    status_only: true,
                    open_browser: false,
                })
            }
            [option, no_open] if option == "--direct" && no_open == "--no-open" => {
                Ok(Command::DirectLogin {
                    status_only: false,
                    open_browser: false,
                })
            }
            [] => Ok(Command::Login {
                device_code: false,
                status_only: false,
            }),
            [option] if option == "--device-code" || option == "--device-auth" => {
                Ok(Command::Login {
                    device_code: true,
                    status_only: false,
                })
            }
            [option] if option == "status" => Ok(Command::Login {
                device_code: false,
                status_only: true,
            }),
            [option] if option == "--help" || option == "-h" => Ok(Command::Help),
            _ => Err("Usage: picos login [--device-code] or picos login status".into()),
        },
        "new" | "resume" => {
            if args.len() == 2 && ["--help", "-h"].contains(&args[1].as_str()) {
                return Ok(Command::Help);
            }
            let resume = command == "resume";
            let allowed = if resume {
                &[
                    "model",
                    "prompt",
                    "turns",
                    "mode",
                    "tell-turns",
                    "strict",
                    "lan",
                    "port",
                    "no-open",
                    "dir",
                ][..]
            } else {
                &[
                    "cpu",
                    "ram",
                    "storage",
                    "model",
                    "prompt",
                    "cycles",
                    "turns",
                    "mode",
                    "tell-turns",
                    "strict",
                    "lan",
                    "port",
                    "no-open",
                    "dir",
                ][..]
            };
            let (name, values) = options(&args[1..], allowed, true)?;
            if resume && name.is_none() {
                return Err(
                    "Choose a saved experiment: picos resume NAME. Use picos list to see names."
                        .into(),
                );
            }
            let mission = values.get("prompt").cloned();
            if mission
                .as_ref()
                .is_some_and(|m| m.trim().is_empty() || m.len() > 12000)
            {
                return Err("The prompt must contain 1–12,000 bytes of text.".into());
            }
            if !resume && mission.is_none() {
                return Err("Give the agent a mission with --prompt \"build a terminal\".".into());
            }
            let model = values.get("model").cloned();
            if !resume && values.contains_key("strict") && model.is_none() {
                return Err("Strict experiments require --model with a model available to your ChatGPT account.".into());
            }
            if model.as_ref().is_some_and(|m| {
                m.trim().is_empty() || m.len() > 120 || m.contains(['\n', '\r', '\0'])
            }) {
                return Err("The model name is invalid.".into());
            }
            if resume && model.is_some() && mission.is_none() {
                return Err(
                    "--model on resume requires --prompt to start a new agent turn.".into(),
                );
            }
            if resume && values.contains_key("turns") && mission.is_none() {
                return Err(
                    "--turns on resume requires --prompt to start an agent session.".into(),
                );
            }
            let mode = values
                .get("mode")
                .map(|value| value.parse::<RunMode>())
                .transpose()?;
            let tell_turns = values
                .get("tell-turns")
                .map(|value| match value.as_str() {
                    "true" => Ok(true),
                    "false" => Ok(false),
                    _ => Err("--tell-turns must be true or false.".to_string()),
                })
                .transpose()?;
            let max_turns = values
                .get("turns")
                .map_or(Ok(DEFAULT_TURNS), |v| parse_integer(v, "Agent turns"))?;
            if !(1..=MAX_TURNS).contains(&max_turns) {
                return Err(format!("Agent turns must be 1..{MAX_TURNS}."));
            }
            let port = values
                .get("port")
                .map(|s| {
                    parse_integer(s, "Port")
                        .and_then(|v| u16::try_from(v).map_err(|_| "Port must be 0..65535.".into()))
                })
                .transpose()?
                .unwrap_or(0);
            let config = if resume {
                None
            } else {
                let mhz = values
                    .get("cpu")
                    .map_or(Ok(300), |v| parse_integer(v, "CPU MHz"))?;
                if mhz == 0 || mhz > 10000 {
                    return Err("CPU frequency must be 1..10000 MHz.".into());
                }
                let clock_hz = mhz * 1_000_000;
                let config = Config {
                    clock_hz,
                    ram_bytes: megabytes(
                        values.get("ram").map(String::as_str).unwrap_or("8"),
                        "RAM",
                    )?,
                    disk_bytes: megabytes(
                        values.get("storage").map(String::as_str).unwrap_or("32"),
                        "Storage",
                    )?,
                    max_cycles: values
                        .get("cycles")
                        .map_or(Ok(clock_hz * 60), |v| parse_integer(v, "Cycle allowance"))?,
                };
                config.validate()?;
                Some(config)
            };
            Ok(Command::Experiment(ExperimentCommand {
                name,
                data_dir: values.get("dir").map(PathBuf::from),
                port,
                config,
                mission,
                model,
                max_turns,
                mode,
                tell_turns,
                strict: values.contains_key("strict"),
                lan: values.contains_key("lan"),
                open_browser: !values.contains_key("no-open"),
                resume,
            }))
        }
        option if option.starts_with("--") => {
            // Retained for existing headless integrations; ordinary starts use new/resume.
            let (_, values) = options(args, &["port", "data-dir"], false)?;
            let port = values
                .get("port")
                .map(|v| {
                    v.parse::<u16>()
                        .map_err(|_| "Port must be 0..65535.".to_string())
                })
                .transpose()?
                .unwrap_or(4317);
            Ok(Command::LegacyServe {
                port,
                data_dir: values
                    .get("data-dir")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(".smol")),
            })
        }
        _ => Err(format!("Unknown command {command:?}. Run picos --help.")),
    }
}
pub fn picos_home() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("PICOS_HOME").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(|home| PathBuf::from(home).join(".picos"))
        .ok_or("Set PICOS_HOME to a directory for your experiments.".into())
}
pub fn experiment_directory(
    root: &Path,
    name: &str,
    override_dir: Option<PathBuf>,
) -> Result<PathBuf, String> {
    if !valid_name(name) {
        return Err("Invalid experiment name.".into());
    }
    Ok(override_dir.unwrap_or_else(|| root.join("experiments").join(name)))
}
fn login(device_code: bool, status_only: bool) -> Result<(), String> {
    let executable = env::var("PICOS_CODEX_BIN")
        .or_else(|_| env::var("SMOL_CODEX_BIN"))
        .unwrap_or_else(|_| "codex".into());
    let environment = subscription_environment(env::vars());
    let status = ProcessCommand::new(&executable)
        .args(["login", "status"])
        .env_clear()
        .envs(&environment)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            format!("Could not start Codex ({e}). Install the Codex CLI, then run picos login.")
        })?;
    let output = format!(
        "{}\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    if status.status.success() && output.contains("Logged in using ChatGPT") {
        println!("Signed in with ChatGPT. You're ready to create an experiment.");
        return Ok(());
    }
    if status_only {
        return Err(
            "No ChatGPT sign-in is active. Run picos login to connect your subscription.".into(),
        );
    }
    println!("Signing in with ChatGPT through the Codex CLI…");
    let mut process = ProcessCommand::new(&executable);
    process
        .args(["login", "-c", "forced_login_method=\"chatgpt\""])
        .env_clear()
        .envs(&environment)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if device_code {
        process.arg("--device-auth");
    }
    if !process
        .status()
        .map_err(|e| format!("Could not start ChatGPT sign-in: {e}"))?
        .success()
    {
        return Err("ChatGPT sign-in did not complete. Run picos login to try again.".into());
    }
    let verified = ProcessCommand::new(&executable)
        .args(["login", "status"])
        .env_clear()
        .envs(&environment)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    let output = format!(
        "{}\n{}",
        String::from_utf8_lossy(&verified.stdout),
        String::from_utf8_lossy(&verified.stderr)
    );
    if !verified.status.success() || !output.contains("Logged in using ChatGPT") {
        return Err(
            "Sign-in finished, but a ChatGPT session was not confirmed. Run picos login status."
                .into(),
        );
    }
    println!("Signed in with ChatGPT. Run picos new --prompt \"build a terminal\" to begin.");
    Ok(())
}
fn list() -> Result<(), String> {
    let root = picos_home()?.join("experiments");
    if !root.exists() {
        println!("No experiments yet. Start one with picos new --prompt \"build a terminal\".");
        return Ok(());
    }
    let mut entries = fs::read_dir(&root)
        .map_err(|e| format!("Cannot read {}: {e}", root.display()))?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .collect::<Vec<_>>();
    entries.sort_by_key(|e| e.file_name());
    if entries.is_empty() {
        println!("No experiments yet. Start one with picos new --prompt \"build a terminal\".");
        return Ok(());
    }
    println!("EXPERIMENT                 MODEL                  MISSION");
    for entry in entries {
        let meta: Value = fs::read(entry.path().join("experiment.json"))
            .ok()
            .and_then(|v| serde_json::from_slice(&v).ok())
            .unwrap_or(Value::Null);
        let name = meta["name"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
        let model = meta["model"].as_str().unwrap_or("Codex default");
        let mission = meta["mission"]
            .as_str()
            .or_else(|| meta["prompt"].as_str())
            .unwrap_or("Saved machine");
        let one_line = |s: &str, max| {
            s.chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .take(max)
                .collect::<String>()
        };
        println!(
            "{:<26} {:<22} {}",
            one_line(&name, 25),
            one_line(model, 21),
            one_line(mission, 80)
        );
    }
    println!("\nOpen a saved machine with picos resume NAME.");
    Ok(())
}
pub fn run() -> Result<(), String> {
    let command = parse(&env::args().skip(1).collect::<Vec<_>>())?;
    match command {
        Command::Help => {
            println!("{HELP}");
            Ok(())
        }
        Command::Version => {
            println!("picos {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Login {
            device_code,
            status_only,
        } => login(device_code, status_only),
        Command::DirectLogin {
            status_only,
            open_browser,
        } => {
            if status_only {
                println!("{}", crate::direct_auth::status()?);
                Ok(())
            } else {
                crate::direct_auth::login(open_browser)
            }
        }
        Command::List => list(),
        Command::Models => {
            let models = crate::direct_provider::models()?;
            if models.is_empty() {
                return Err(
                    "Your direct ChatGPT model catalog is empty. No model was substituted.".into(),
                );
            }
            for (slug, display_name) in models {
                println!("{slug:<32} {display_name}");
            }
            Ok(())
        }
        Command::Mcp => mcp::run_stdio(),
        Command::LegacyServe { port, data_dir } => server::serve(port, data_dir),
        Command::Experiment(command) => {
            let name = command.name.unwrap_or_else(|| {
                let time = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                format!(
                    "experiment-{time}-{}",
                    &uuid::Uuid::new_v4().simple().to_string()[..6]
                )
            });
            let root = if command.data_dir.is_some() {
                PathBuf::new()
            } else {
                picos_home()?
            };
            let data_dir = experiment_directory(&root, &name, command.data_dir)?;
            server::serve_experiment(server::ExperimentOptions {
                name,
                data_dir,
                port: command.port,
                config: command.config,
                mission: command.mission,
                model: command.model,
                max_turns: command.max_turns,
                mode: command.mode,
                tell_turns: command.tell_turns,
                strict: command.strict,
                lan: command.lan,
                open_browser: command.open_browser,
                resume: command.resume,
            })
        }
    }
}
