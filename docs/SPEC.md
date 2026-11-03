# Picos / experiment specification

## Intent

Give an LLM a deliberately small, blank computer and watch it construct software from machine primitives. The operator supplies hardware limits, a model, and a mission through a CLI. The browser observes the resulting machine without providing a host-built guest interface. A guest can build a window system, language, filesystem, or scheduler. This custom ISA does not execute existing x86 or Windows binaries.

## Implementation architecture

A single Rust binary owns the assembler, disassembler, instruction decoder, CPU, device ports, resource accounting, persistence, HTTP service, stdio MCP bridge, and both the direct subscription harness and legacy Codex process lifecycle. It does not delegate guest execution to another VM or a JavaScript runtime.

The browser uses HTML/CSS and a small JavaScript observer to poll state, render the guest framebuffer, and inspect memory. It does not run instructions or enforce resource limits. The Rust process remains authoritative when the browser disconnects.

## CLI contract

The canonical entry point is:

```sh
picos login
picos new terminal --cpu 300 --ram 24 --storage 200 \
  --model gpt-6-luna --prompt "build a terminal"
```

The equivalent keyword form is accepted:

```sh
picos new experiment cpu 300 ram 24 storage 200 \
  model gpt-6-luna prompt "build a terminal"
```

- `new [NAME]` creates a new experiment and automatically starts the requested agent turn. An omitted name is generated; an existing experiment directory is never overwritten.
- `--cpu` is whole MHz. `--ram` and `--storage` are decimal MB, with exact whole-byte conversion: 1 MB = 1,000,000 bytes. Defaults are 300 MHz, 8 MB RAM, and 32 MB disk.
- `--strict` selects a VM-only Rust harness with separately authorized direct ChatGPT inference; it survives resume and cannot downgrade. `picos login --direct` authorizes it and `picos models` lists eligible model slugs.
- `--model` is passed unchanged to the selected provider. New strict experiments require it; omitting it in legacy mode uses the Codex CLI default. A model's availability is established by a successful turn, not by accepting its name.
- `--prompt` is required for creation. Each agent turn and tool call is bounded; tools cannot alter the experiment's hardware or total cycle allowance.
- The default cycle allowance is 60 virtual seconds (`clockHz × 60`); `--cycles` sets an explicit total.
- An unused port is selected unless `--port` is supplied. The service binds to loopback by default; `--lan` on `new` or `resume` explicitly enables IPv4 network access. The CLI prints private observer links and opens a browser unless `--no-open` is supplied.
- The process remains in the foreground. Ctrl+C saves the current machine and stops its agent. The observer remains available after a model turn finishes.
- `picos login` reuses a confirmed ChatGPT sign-in or delegates to the Codex ChatGPT login flow. `login status` is read-only; `login --device-code` selects device-code authentication when sign-in is needed. In legacy mode Picos does not copy Codex credentials. Direct mode stores its own separately consented OAuth grant with owner-only permissions. Neither path falls back to API-key billing.
- `picos list` enumerates named experiments. `picos resume NAME` restores a machine without starting inference. An explicit resume `--prompt` starts a new turn; optional `--model` selects its model.
- Storage defaults to `~/.picos/experiments/NAME`. `PICOS_HOME` changes the root and `--dir PATH` selects an explicit experiment directory. An OS lock prevents concurrent opens of one experiment.

## Observer contract

The main browser surface is a view of guest framebuffer RAM. It begins blank and changes only when the guest writes pixels. No host-rendered text, windows, sprites, or drawing primitives are injected into it.

The right-hand memory inspector displays disassembly of current RAM, raw bytes, addresses, registers, and the program counter. It can follow the PC or inspect another address, and highlights changed bytes. The inspector does not display the submitted assembly source as a substitute for live memory. Self-modifying code is visible through actual RAM changes.

Disassembly is a byte interpretation, not source recovery. Invalid encodings and incomplete instructions are rendered as data. Valid instruction-shaped data may be decoded as instructions; code/data classification and original labels cannot be inferred reliably. Guest serial output may be shown separately as an observation channel.

The browser has no source editor, assembler, or hardware controls. It has authenticated operator session controls for modes, turn grants, budget disclosure, and stop/continue. Guest mutations remain available only through the authenticated machine tool bridge.

## Hardware contract v1

- 16 unsigned 32-bit registers r0..r15; r15 is the descending stack pointer. Separate byte-addressed program counter and comparison flags. Arithmetic wraps modulo 2^32.
- Unified mutable RAM holds instructions, data, stack, and framebuffer. No guest allocation services. Out-of-range access, malformed instructions, and divide-by-zero trap.
- Display: 256 × 192 bytes, RGB332, at `VIDEO_BASE = RAM_BYTES - 49152`. Every pixel occupies guest RAM.
- Raw persistent byte-addressed disk with no guest filesystem. The boot loader copies a selected disk range into RAM on reboot.
- Fixed 8-byte instructions: opcode u8, destination register u8, operand-kind u8 (0 immediate, 1 register), reserved u8 (zero), operand u32 little endian.
- Binary operations use `op rN, valueOrRegister`; stores use `store8/store32 valueRegister, addressOrRegister`. I/O uses `out valueRegister, port` and `in destinationRegister, port`.
- Arithmetic: mov/add/sub/mul/div/mod/and/or/xor/shl/shr. Memory: load8/load32/store8/store32. Control: cmp/jmp/je/jne/jlt/jle/jgt/jge/call/ret/push/pop/nop/halt/yield. Comparisons are unsigned. Branches/call/push take one immediate or register; pop takes a destination register.
- Ports: 0 serial output (in returns 0); 1 keyboard input (returns 0xffffffff when empty); 2 disk cursor (read/write); 3 disk byte (read/write and advance); 4 cycle counter low 32 bits (read only).
- Costs: ordinary instruction 1 cycle; loads/stores/push/pop 2; call/ret 3; mul 4; div/mod 16; in/out 16. The full instruction cost must fit the run budget. `yield` returns control without halting. One run call cannot exceed 2,000,000 cycles.
- CPU MHz is a conversion between virtual cycles and simulated seconds. It does not emulate CPU caches or guarantee equivalent host performance. Total consumed cycles survive reboot and reassembly.

See [ISA.md](ISA.md) for encodings, assembler syntax, and device details.

## Bootstrap and independence

The host assembler is a cross-development aid. Source text, LLM inference, observer logs, and checkpoints are outside guest resources and do not establish self-hosting. Assembled binaries occupy guest disk and RAM. The MCP bridge exposes inspection, assembly/load, execution, keyboard input, and bounded guest memory/disk access. It exposes no host filesystem, shell, or capacity-changing tool.

Sealing irreversibly disables external assembly and RAM/disk injection for the current experiment. Guest code can still write its own RAM and disk; inspection, execution, and keyboard input remain available. A self-hosted language must execute its compiler/interpreter inside guest RAM and spend guest cycles. Generating source or binaries in the LLM does not count as guest compilation.

## Durability and boundaries

Checkpoints preserve CPU, RAM, disk, serial output, limits, seal state, consumed cycles, and source. An append-only local JSONL ledger records tool calls and outcomes. Resuming retains consumption; restoring a historical checkpoint is a separate observer branch. Host artifacts are never readable through guest instructions.

The VM executes numeric bytecode without host syscalls or network access. The HTTP service binds to loopback by default and rejects cross-origin access. Explicit LAN observer access does not expose machine tools to remote clients. In strict mode a Rust loop supplies only the fixed VM tools to direct subscription inference, validates the complete response and arguments before dispatch, and rejects external or unknown calls. Model text is data, never host code. The fixed bootstrap assembler is still external infrastructure until sealed. See [STRICT.md](STRICT.md) and the [sealed interpreter proof](sealed-interpreter.md).

In legacy mode the Rust runner launches Codex with inherited user configuration disabled, a read-only sandbox, shell/apps/plugins/browser tools disabled, and an explicit allowlist of nine machine MCP tools. Codex's discovery and orchestration wrappers may invoke those tools; wrapper computation is external to the guest. This is an experimental boundary, not a hardened adversarial sandbox for the Codex process.

## Version 0.2 acceptance

- `cargo install --path . --bin picos` installs the command; no Node.js runtime or frontend build is required.
- Both CLI styles accept the requested 300 MHz, 24 MB RAM, 200 MB storage, model, and mission, with exact unit conversion and useful errors.
- Login reuses ChatGPT authentication without API-key fallback.
- Creation uses a fresh directory, starts the agent, and publishes a loopback observer URL. Ctrl+C saves and stops; concurrent use and accidental overwrites are rejected.
- Resume restores exact machine state and cycle consumption without implicit inference.
- The browser shows guest pixels and current RAM disassembly, including changed bytes and PC position, with no mutation controls.
- Resource faults, mutable-code execution, raw-disk persistence, sealed mode, and protocol boundaries retain their tested behavior.
- Model execution, browser rendering, and API tests are reported separately; a completed CLI turn is not evidence that its guest mission succeeded.

## Validation records

The existing Rust engine and subscription acceptance are recorded in [VALIDATION.md](VALIDATION.md) and [acceptance.json](acceptance.json). That test proved a real assemble/run/observe loop and independently checked every framebuffer pixel. The version 0.2 CLI and observer have a separate acceptance pass, including the requested 24 MB / 200 MB capacities; the earlier record does not establish access to the example's specific model.

Browser layout and interaction checks require browser access on the host. API and byte-level checks can verify machine behavior without establishing visual layout or focus behavior.


## Observable iteration sessions

CLI sessions default to six turns (`--turns 1..24`). A successful provider completion or legacy Codex process exit completes an iteration, not the experiment mission. The supervisor launches another bounded turn with the original mission, current machine, and a bounded previous report. Each turn inspects authoritative source/state. Tool consumption remains cumulative (40 calls per granted turn in standard/maximize, no cumulative cap in resource-limit); lifetime cycles are never reset. Failures/stops and mode-specific resource limits stop automatic turn continuation. Failed sessions can explicitly resume within the remaining allowance; a failed final standard-mode turn may receive a grant. Consumed turns, tools and cycles are not refunded. Direct inference retries only pending transport failures and explicit temporary service errors, at most three attempts within a shared300-second deadline; protocol, authentication, quota and other HTTP failures remain explicit errors. Standard mode additionally pauses after two turns without RAM changes, cycle consumption, or newly touched disk. Resume without a prompt remains inference-free.

The observer receives a sequence-numbered, bounded history of actual RAM transitions from Rust. Each operation includes its kind, time, byte-transition count, changed ranges, and bounded before/after previews. Reassembly/reboot compare old RAM against final boot RAM atomically. Guest writes that restore a byte within a run remain observable transitions. History retains 24 operations with up to 64 ranges and 32 preview bytes per range; truncation is explicit. History is checkpointed separately from guest RAM and resource accounting. It is not an allocation map or a source-token stream.

Follow writes defaults on. It focuses changed RAM and highlights new writes even if the observer had never visited that address. Manual navigation suspends following; Follow PC remains available. Historical previews are labeled; the disassembly always shows current RAM. A prominent activity panel distinguishes model work without writes, machine tool work, fresh memory updates, and stopped sessions with reasons. Reduced-motion preferences disable decorative animation.


## Mode and operator-control contract

`--mode standard|resource-limit|maximize` and `--tell-turns true|false` configure new sessions. They can also update a saved session during plain resume without starting inference. Standard and maximize honor finite turn/tool allowances. Resource-limit ignores the total-turn allowance, continues after ordinary model-turn completion, and stops immediately on exact RAM/disk/boot-capacity violations or cycle exhaustion (including when the remaining allowance cannot fit the next instruction). Touched-byte counts are historical evidence, not occupancy or fullness tests. Maximize pursues measured functional quality within limits; it cannot certify global optimality.

GET state provides an operator capability distinct from the machine capability. Only `/api/session/settings`, `/api/session/turns`, `/api/session/continue`, `/api/session/stop` accept it via `X-Picos-Operator`; an explicit Host allowlist and same-origin checks remain enforced. LAN mode additionally requires the observer access key. MCP does not expose these routes or their capability. All other guest-mutating browser POSTs remain forbidden. Grants are explicit, bounded, never retried by the browser, and persisted. A grant extends the allowance without resetting existing usage. Mode changes do not auto-start work and are refused while a process is active. Disclosure changes affect the next prompt. Restores pause running/waiting sessions and preserve stopped reasons; resource breaches remain stopped across mode changes and restores.

## LAN observer and phone interaction

`--lan` binds IPv4 on all interfaces and prints links for active private/link-local addresses on macOS/Linux. Only loopback names and discovered local addresses at the selected port are accepted as HTTP Hosts. The agent always uses the loopback address. `/api/tool` requires both a loopback peer/Host and the separate machine bearer token.

LAN observer API requests require a random per-process 256-bit key through `X-Picos-Access`. Links carry it in a URL fragment; the browser transfers it to session storage and scrubs the fragment before polling. Keys rotate on restart. Missing or invalid keys return HTTP401 with `code: access_required`; the public static shell shows a connection form without guest data. Session controls still require their distinct operator capability, and Origin must match Host exactly. Referrer policy is `no-referrer`. This is HTTP for trusted networks, not public hosting or protection from network traffic interception.

At viewport widths up to900px, persistent Display/Memory/Journal/Controls navigation selects one workspace at a time. Touch controls are at least44px high and text inputs use16px fonts. The framebuffer scales with its aspect ratio preserved; memory bytes/instructions wrap and long lists scroll within their panes. Safe-area padding protects navigation on inset screens. Journal follow/unread behavior tracks the visible workspace. Polling slows in background tabs and refreshes on return. Copy goal supports browsers without the secure-context Clipboard API.


## Public study journal

A read-only `/api/agent/journal?after=ID` endpoint exposes current/original goals, public events with stable monotonic IDs, turn/session labels, bounded tool evidence and the last24 exact picos-authored submitted prompts. The observer shows goal text and a tabbed journal with filters and scroll-preserving follow control. It uses text nodes, never executes model-provided markup. Tool preview fields are bounded and explicitly summaries; framebuffer PNG payloads are not duplicated into logs.

Only public agent messages and documented lifecycle/tool events are admitted. Analysis/reasoning events, analysis-channel messages, encrypted reasoning and untyped stdout payloads are excluded; untyped data emits a fixed warning. New prompts ask for brief public progress and concise rationale, not private deliberations. Completion is queued before the runner becomes idle to preserve turn attribution. The journal checkpoint retains recent400 entries/up to2 MiB and24 prompts; new records are appended to `journal.jsonl`. Legacy public events may be imported, but absent exact prompt snapshots are never fabricated.
