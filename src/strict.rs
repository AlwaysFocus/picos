//! VM-only agent loop. Model output is data; this module never evaluates code,
//! launches a process, reads model-selected paths, or contacts model-selected URLs.
use crate::{direct_provider, mcp};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub const MAX_CALLS: usize = 40;
const MAX_CONTEXT_BYTES: usize = 4 * 1024 * 1024;
pub const OPENING: &str = "Begin this iteration. Call machine_info first.";

pub fn prompt(mission: &str) -> String {
    format!(
        r#"You are the resident architect of picos, a small resource-limited virtual computer.
STRICT EXECUTION: Your only actions are the enumerated picos machine tools. There is no shell, JavaScript, Python, browser, network, host file access, tool discovery, or external code interpreter. All helpers, languages, compilers, renderers and algorithms you build must execute as guest instructions and use guest RAM/disk/cycles. Tool arguments are data and are never evaluated as host code.

First call machine_info for the authoritative ISA, current program and resources. Preserve useful existing work. Iterate: inspect, assemble, execute a bounded guest slice, observe machine_screen and actual serial/memory results, and improve. Never claim success from code alone or from a non-faulting run. Check whether pixels actually form the intended glyphs, not merely whether some nonzero pixels exist.

BOOTSTRAP CONTRACT: The fixed cross-assembler and bounded byte loader are external experiment infrastructure until machine_seal. They may translate assembly and install data, but cannot run scripts or helper programs. Binaries occupy guest RAM and disk. Bootstrap is not self-hosting. Once a guest input/loader/interpreter works, seal when the mission requests self-hosted execution; after sealing, assembly and external writes are forbidden, and further software must be interpreted or compiled by the guest. Reboot preserves disk, sealing and lifetime cycles. The model's context/inference and observation transport are outside guest resources; they are not a guest CPU or extra guest filesystem.

Give brief public progress updates describing actions, concise rationale, and observed results. Do not emit private chain-of-thought. At completion report actual evidence, failures, resource costs and the next milestone. At most 40 machine calls are allowed in this iteration. Do not change limits. Do not invent tools or use external runtimes.

MISSION:
{}"#,
        mission.trim()
    )
}

fn output_for_tool(name: &str, value: Value) -> Result<Value, String> {
    if name == "machine_screen" {
        let png = mcp::framebuffer_png(&value)?;
        Ok(json!([
            {"type":"input_text","text":"Actual 256x192 RGB332 guest framebuffer. No host text rendering."},
            {"type":"input_image","image_url":format!("data:image/png;base64,{}", STANDARD.encode(png))}
        ]))
    } else {
        Ok(Value::String(value.to_string()))
    }
}

pub fn run(
    base_url: &str,
    token: &str,
    model: &str,
    instructions: &str,
    stopped: &AtomicBool,
    mut event: impl FnMut(&str, &str),
) -> Result<String, String> {
    run_loop(
        model,
        instructions,
        stopped,
        &mut event,
        |model, instructions, input, stopped, message| {
            direct_provider::respond(model, instructions, input, stopped, message)
        },
        |name, arguments| {
            mcp::post(
                base_url,
                "/api/tool",
                &json!({"name":name,"arguments":arguments}),
                Some(token),
            )
        },
    )
}

fn run_loop(
    model: &str,
    instructions: &str,
    stopped: &AtomicBool,
    event: &mut impl FnMut(&str, &str),
    mut respond: impl FnMut(
        &str,
        &str,
        &[Value],
        &AtomicBool,
        &mut dyn FnMut(&str, &str),
    ) -> Result<direct_provider::ProviderResponse, String>,
    mut dispatch: impl FnMut(&str, &Value) -> Result<Value, String>,
) -> Result<String, String> {
    let began = Instant::now();
    let mut input = vec![json!({"role":"user","content":OPENING})];
    let mut seen_calls = HashSet::new();
    let mut calls = 0;
    loop {
        if stopped.load(Ordering::SeqCst) {
            return Err("Experiment stopped.".into());
        }
        if began.elapsed() >= Duration::from_secs(600) {
            return Err("Strict iteration reached its 10-minute wall-time limit.".into());
        }
        if serde_json::to_vec(&input)
            .map_err(|_| "Cannot encode model context.")?
            .len()
            > MAX_CONTEXT_BYTES
        {
            return Err(
                "Strict model context limit reached; no history was silently discarded.".into(),
            );
        }
        let mut last_message = String::new();
        let response = respond(
            model,
            instructions,
            &input,
            stopped,
            &mut |kind, message| {
                if kind == "agent.message" {
                    last_message = message.chars().take(8000).collect();
                } else if kind == "agent.warning" {
                    // A retry discarded the preceding attempt and its partial report.
                    last_message.clear();
                }
                event(kind, message);
            },
        )?;
        // Validate the entire response before dispatching any call. A hosted or
        // unknown tool in the same response cannot execute after an allowed one.
        let mut pending = Vec::new();
        for item in &response.output {
            if let Some((name, call_id, arguments)) = direct_provider::tool_call(item)? {
                mcp::validate_arguments(&name, &arguments)?;
                if !seen_calls.insert(call_id.clone()) {
                    return Err("Duplicate tool call ID rejected.".into());
                }
                pending.push((name, call_id, arguments));
            }
        }
        if calls + pending.len() > MAX_CALLS {
            return Err("Strict iteration tool allowance exhausted before dispatch.".into());
        }
        if pending.is_empty() {
            if last_message.trim().is_empty() {
                return Err("Model completed without a public report or VM action.".into());
            }
            return Ok(last_message);
        }
        input.extend(response.output);
        for (name, call_id, arguments) in pending {
            if stopped.load(Ordering::SeqCst) {
                return Err("Experiment stopped.".into());
            }
            calls += 1;
            event("agent.tool", &format!("{name}: running"));
            let result = match dispatch(&name, &arguments) {
                Ok(value) => output_for_tool(&name, value)?,
                Err(error) => Value::String(json!({"error":error,"isError":true}).to_string()),
            };
            event("agent.tool", &format!("{name}: completed"));
            input.push(json!({"type":"function_call_output","call_id":call_id,"output":result}));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn response(output: Value) -> direct_provider::ProviderResponse {
        direct_provider::ProviderResponse {
            output: output.as_array().unwrap().clone(),
            usage: Value::Null,
        }
    }
    #[test]
    fn only_vm_calls_execute_and_results_are_data() {
        let mut iteration = 0;
        let mut dispatched = vec![];
        let result = run_loop("fixture", "mission", &AtomicBool::new(false), &mut |_,_|{},
            |_,_,input,_,message| {
                iteration += 1;
                if iteration == 1 { Ok(response(json!([{"type":"function_call","id":"fc_a","name":"machine_input","namespace":"picos","call_id":"a","arguments":"{\"text\":\"$(touch /tmp/never)\"}"}]))) }
                else { assert!(input.last().unwrap()["output"].as_str().unwrap().contains("queued")); message("agent.message", "Input delivered; guest must parse it."); Ok(response(json!([]))) }
            },
            |name,args| { dispatched.push((name.to_owned(), args.clone())); Ok(json!({"queued":true})) }).unwrap();
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].1["text"], "$(touch /tmp/never)");
        assert!(result.contains("guest"));
    }
    #[test]
    fn unknown_operations_fail_before_any_dispatch() {
        let mut dispatched = false;
        let result = run_loop(
            "fixture",
            "mission",
            &AtomicBool::new(false),
            &mut |_, _| {},
            |_, _, _, _, _| {
                Ok(response(json!([
                    {"type":"function_call","id":"fc_a","name":"machine_info","namespace":"picos","call_id":"a","arguments":"{}"},
                    {"type":"function_call","id":"fc_b","name":"exec_command","namespace":"picos","call_id":"b","arguments":"{}"}
                ])))
            },
            |_, _| {
                dispatched = true;
                Ok(json!({}))
            },
        );
        assert!(result.is_err());
        assert!(!dispatched);
    }
    #[test]
    fn retry_notice_and_abandoned_report_cannot_complete_a_turn() {
        let mut events = Vec::new();
        let result = run_loop(
            "fixture",
            "mission",
            &AtomicBool::new(false),
            &mut |kind, _| events.push(kind.to_owned()),
            |_, _, _, _, event| {
                event("agent.message", "Partial report from interrupted attempt.");
                event("agent.warning", "Retrying pending inference.");
                Ok(response(json!([])))
            },
            |_, _| panic!("no tools after empty response"),
        );
        assert!(result.unwrap_err().contains("without a public report"));
        assert_eq!(events, ["agent.message", "agent.warning"]);
    }
    #[test]
    fn cancellation_prevents_inference_and_dispatch() {
        assert!(
            run_loop(
                "fixture",
                "mission",
                &AtomicBool::new(true),
                &mut |_, _| {},
                |_, _, _, _, _| panic!("no inference after stop"),
                |_, _| panic!("no tool after stop")
            )
            .is_err()
        );
    }
}
