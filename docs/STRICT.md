# Strict execution contract

```sh
picos login --direct
picos models
picos new terminal-strict --strict --cpu 300 --ram 24 --storage 200 \
  --model gpt-6-luna --turns 6 --prompt "Build and test a terminal"
```

The model slug must be available to the signed-in account. Picos passes it unchanged and never substitutes a model, Codex, or API-key billing. Direct authorization is separate from the existing Codex login. An existing experiment can be upgraded with `picos resume NAME --strict`; that opens the observer without inference. Add an explicit `--prompt` to start a new session. Previous external work is not retroactively made strict.

## Boundary

The trusted Rust harness contacts only the fixed OpenAI Responses endpoint and its own authenticated loopback VM service. Its model-facing namespace contains exactly nine functions: info, assemble, run, inspect, write, input, screen, reboot, and seal. It has no execution interpreter, shell, host file access, browser, web search, dynamic tool discovery, hosted tools, plugins, or model-selected URLs. Model strings and tool results are always data. The model cannot change capacity, restore a checkpoint, grant itself turns, or access credentials through these functions.

The provider buffers a bounded response and only accepts completed responses. The runner validates every call in that response before executing any of them: exact function and namespace, JSON schema and bounds, unique call IDs, and a maximum of 40 calls per iteration. Unknown tools, indirect/programmatic callers, explicit incomplete/failed responses, and malformed calls stop the iteration. Cancellation is checked before each dispatch and during streaming. Context is capped at 4 MiB; nothing is silently evicted to fit. Each inference request has a five-minute deadline shared by all attempts; the loop also checks a ten-minute iteration deadline between requests. Authenticated guest limits remain authoritative independently of model obedience.

Transport failures and streams cut off before completion receive at most three attempts, with one- then two-second cancellable backoff. Only the pending inference is retried with the same input; buffered output from the interrupted attempt is discarded and already-executed VM tools are not replayed. Retry notices are public system events, not model messages. Explicit temporary HTTP408/5xx service errors, including subscription usage temporarily unavailable, may retry after a bounded Retry-After delay (five seconds when omitted). Long or date-valued Retry-After headers stop for explicit recovery. Authentication, quota, rate limits, other HTTP errors, protocol validation errors, cancellation and resource limits do not automatically retry. Exhausted retries stop the turn with a sanitized failure category and elapsed time. The operator can explicitly resume using remaining turns, or grant turns after a failed final standard-mode turn. Recovery retains all consumed turns/tools/cycles and never overrides a hard resource stop.

Direct calls use separately consented ChatGPT OAuth. The model receives neither OAuth tokens nor the loopback capability. Authentication and inference ignore proxy settings and reject redirects. Fixed endpoint configuration, credentials, and host paths cannot be changed by a model tool. Private reasoning is not journaled; optional encrypted continuity stays in process memory. The observer shows public messages, exact picos prompts, VM actions, and measured results.

## Bootstrap versus sealed operation

**Strict** removes external model-written code execution. During bootstrap, a fixed Rust cross-assembler and byte loader still translate and install guest programs. They do not evaluate host expressions, invoke commands, load files, or fetch includes. Their work is outside the guest CPU budget, and the observer labels this as assembly bootstrap.

**Sealed** additionally disables assembly and external RAM/disk writes. The model can send bounded input, run the guest, inspect it, and reboot; only guest instructions can translate that input into new code or data. Reboot preserves sealing, disk, and consumed lifetime cycles. All helpers must now execute in the guest and incur its CPU costs and RAM/disk bounds. This is the stronger experimental condition for self-hosting claims.

The model itself still reasons externally, and observations, input transport, logs, snapshots and the fixed broker are host infrastructure. They are not guest computing resources. The guest's finite machine is capable of loops, indirect calls, writable code, stacks, memory, byte I/O and raw persistence, allowing compilers, interpreters, allocators and filesystems within its finite capacity. No claim of an unlimited Turing machine or x86 compatibility is made.

## Evidence and limits

`cargo test --test sealed_interpreter` demonstrates a guest parser/compiler running after sealing: fresh input becomes native VM instructions, executes, persists to guest disk, then reloads and executes after reboot. Negative tests verify rejected external writes, malformed input, bounded code space, lifetime CPU exhaustion, and RAM/disk faults. See [sealed-interpreter.md](sealed-interpreter.md) for exact costs and grammar.

These tests establish the exercised execution paths and invariants. They are not a formal proof that a Rust process, TLS stack, kernel or provider can never have vulnerabilities. The host operator controls experiment files and the executable; strict mode does not defend against a malicious operator rewriting them. For hostile multi-user deployment, isolate the trusted broker/VM process with OS controls in addition to this model capability boundary.
