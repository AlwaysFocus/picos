# picos

Give an LLM a small computer and watch it build from assembly.

Picos starts with a custom 32-bit CPU (`smol32`), fixed RAM, raw storage, and a 256 × 192 pixel display. There is no guest OS, filesystem, shell, allocator, programming language, or window system. Everything has to be built from machine instructions.

The goal is to see what different models do when given tight constraints. From the CLI or the web UI, you can cap the number of turns, hide or show the remaining turn budget, and limit the CPU clock, RAM, disk, and total cycle allowance. With only bare machine instructions and fixed capacity, models have to figure out their own ways to fit code, data, stack, and graphics into memory.

---

## Table of Contents

- [Architecture Overview](#architecture-overview)
- [Hardware & `smol32` ISA](#hardware--smol32-isa)
- [Agent Tool Bridge (MCP)](#agent-tool-bridge-mcp)
- [Quick Start](#quick-start)
- [CLI Reference](#cli-reference)
- [Experiment Modes & Turn Controls](#experiment-modes--turn-controls)
- [Live Browser & Mobile Observer](#live-browser--mobile-observer)
- [Project Layout](#project-layout)
- [Development & Testing](#development--testing)
- [Documentation](#documentation)

---

## Architecture Overview

```text
                ┌───────────────────────────────────────────────┐
                │             Agent / LLM Provider              │
                │     (Direct ChatGPT Auth or Codex CLI)        │
                └───────────────────────┬───────────────────────┘
                                        │ JSON-RPC / MCP
                ┌───────────────────────▼───────────────────────┐
                │          Picos Harness & MCP Bridge           │
                │         [src/strict.rs] / [src/mcp.rs]        │
                └───────────────────────┬───────────────────────┘
                                        │ Machine Tools
                ┌───────────────────────▼───────────────────────┐
                │                 Virtual Machine               │
                │                  [src/vm.rs]                  │
                │  • smol32 CPU (r0–r15, PC, comparison state)  │
                │  • Flat mutable RAM (code, data, stack, VRAM) │
                │  • Display (256×192 RGB332 @ VIDEO_BASE)      │
                │  • Raw Disk Storage                           │
                │  • I/O Ports: Serial, Keyboard, Disk, Cycles  │
                └───────────────┬───────────────────────┬───────┘
                                │                       │
            ┌───────────────────▼──┐         ┌──────────▼─────────────────┐
            │ Assembler / Disasm   │         │ HTTP Server & Web Observer │
            │  [src/assembler.rs]  │         │      [src/server.rs]       │
            │ [src/disassembler.rs]│         │    [public/index.html]     │
            └──────────────────────┘         └────────────────────────────┘
```

- **Strict Execution Boundary (`--strict`)**: Runs a pure-Rust agent loop using direct ChatGPT subscription OAuth (`picos login --direct`). It exposes only the 9 virtual machine tools, with no host shell, filesystem, Python, or Node.js.
- **Legacy Codex Runner**: Delegates orchestration to the Codex CLI (`picos login`) inside a read-only sandbox restricted to the same 9 MCP machine tools.

---

## Hardware & `smol32` ISA

| Component | Specification |
| --- | --- |
| **CPU** | Custom 32-bit unsigned ISA (`smol32`), 16 registers (`r0`–`r15`), byte-addressed PC, comparison flag |
| **Instruction Encoding** | Fixed 8-byte instructions (opcode, dest reg, operand kind, reserved zero byte, 32-bit immediate/reg) |
| **Stack** | `r15` serves as the descending stack pointer, initialized on reset to `VIDEO_BASE` |
| **RAM** | Single flat byte array shared by code, data, stack, and framebuffer (self-modifying code works natively) |
| **Display** | **256 × 192** RGB332 pixels (`49,152` bytes mapped at `VIDEO_BASE = RAM_BYTES - 49152`) |
| **Disk** | Fixed raw byte array accessed byte-by-byte via I/O ports; persists across machine reboots |
| **Virtual Clock** | Configured in MHz (`1 MHz = 1,000,000 cycles/sec`); deterministic per-instruction cycle costs |

### Minimal Instruction Set

The entire CPU supports just **32 instructions** (`0x00`–`0x1f`). There are no floating-point ops, SIMD instructions, hardware interrupts, memory protection rings, or graphics syscalls. The idea is to keep the primitives as small and transparent as possible, then see how much complexity an agent can build on top of them, from drawing bitmap fonts pixel-by-pixel in `VIDEO_BASE` to building shells, file structures, or self-hosted compilers in flat RAM.

| Category | Instructions | Description |
| --- | --- | --- |
| **Control** | `nop`, `halt`, `yield` | No-op, stop CPU until reboot, or pause slice and resume at next instruction |
| **Move & Arithmetic** | `mov`, `add`, `sub`, `mul`, `div`, `mod` | 32-bit unsigned register/immediate assignment and wrapping arithmetic |
| **Bitwise & Shifts** | `and`, `or`, `xor`, `shl`, `shr` | Bitwise logic and logical bit shifts (shift count uses low 5 bits) |
| **Memory** | `load8`, `load32`, `store8`, `store32` | Byte and 32-bit little-endian loads/stores to flat RAM |
| **Compare & Branch** | `cmp`, `jmp`, `je`, `jne`, `jlt`, `jle`, `jgt`, `jge` | Unsigned comparison and conditional/unconditional jumps |
| **Stack & Calls** | `call`, `ret`, `push`, `pop` | Subroutine calls and stack operations using `r15` |
| **Device I/O** | `in`, `out` | Read or write device ports (`0`–`4`) |

Assembler directives are equally minimal: `.equ`, `.byte`, `.word`, `.ascii`, `.asciz`, and `.space`, along with built-in constants (`RAM_BYTES`, `DISK_BYTES`, `VIDEO_BASE`, `VIDEO_WIDTH`, `VIDEO_HEIGHT`). See [`docs/ISA.md`](docs/ISA.md) for exact binary encodings and per-instruction cycle costs.

### Device I/O Ports (`in` / `out`)

| Port | `in rD, port` (Read) | `out rV, port` (Write) |
| :---: | --- | --- |
| `0` | Returns `0` | Appends low byte of `rV` to serial output (retains last 16,384 bytes) |
| `1` | Dequeues next UTF-8 keyboard byte (`0xFFFFFFFF` if empty) | Faults CPU |
| `2` | Reads current disk byte cursor | Sets disk byte cursor (`0..=DISK_BYTES`) |
| `3` | Reads disk byte at cursor and advances cursor | Writes low byte of `rV` to disk at cursor and advances cursor |
| `4` | Reads low 32 bits of lifetime cycle counter | Faults CPU |

---

## Agent Tool Bridge (MCP)

The agent interacts with the machine exclusively through 9 narrow tools defined in [`src/mcp.rs`](src/mcp.rs):

| Tool | Role |
| --- | --- |
| `machine_info` | Read hardware configuration, register state, cycle usage, bootstrap rules, and the full ISA reference. |
| `machine_screen` | Capture the current guest framebuffer (`256 × 192`) as a PNG image. |
| `machine_assemble` | Cross-assemble `smol32` source and install the binary into guest disk and RAM *(disabled after sealing)*. |
| `machine_write` | Inject raw bytes into guest RAM or disk during bootstrap *(disabled after sealing)*. |
| `machine_run` | Execute up to `2,000,000` cycles in a single slice and return CPU state and serial output. |
| `machine_inspect` | Read up to `4,096` bytes from guest RAM or disk. |
| `machine_input` | Queue up to `4,096` bytes of UTF-8 keyboard input for Port 1. |
| `machine_reset` | Reboot the guest from the disk boot image (clears RAM/CPU while preserving disk, seal state, and lifetime cycles). |
| `machine_seal` | **Permanently disable** `machine_assemble` and `machine_write` so all further work happens strictly inside the guest via keyboard input and guest code. |

---

## Quick Start

### 1. Install and Authenticate

```sh
cargo install --path . --bin picos

# Authenticate for strict mode (direct ChatGPT OAuth)
picos login --direct
picos login --direct status
picos models
```

*(Alternatively, run `picos login` or `picos login --device-code` for the Codex-backed runner).*

### 2. Launch an Experiment

```sh
picos new terminal --strict --cpu 300 --ram 24 --storage 200 \
  --model gpt-6-luna --prompt "build a terminal"
```

A keyword syntax is also supported:

```sh
picos new terminal strict cpu 300 ram 24 storage 200 \
  model gpt-6-luna prompt "build a terminal"
```

`picos` stays in the foreground, starts the local observer server, opens your browser, and begins the first agent turn. Press **Ctrl+C** at any time to save the machine state and stop the agent cleanly.

### 3. List and Resume Experiments

```sh
# List saved experiments under ~/.picos/experiments
picos list

# Open the saved machine in the observer without starting inference
picos resume terminal

# Resume and start a new agent turn with an updated prompt
picos resume terminal --prompt "Continue building the terminal and test it."
```

---

## CLI Reference

### Options for `picos new [NAME]`

| Flag | Description | Default |
| --- | --- | --- |
| `--cpu <MHz>` | Virtual clock frequency in whole MHz | `300` |
| `--ram <MB>` | Guest RAM in decimal MB (`1 MB = 1,000,000 bytes`) | `8` |
| `--storage <MB>` | Guest raw disk in decimal MB (`1 MB = 1,000,000 bytes`) | `32` |
| `--model <SLUG>` | Exact model slug (required in `--strict` mode) | Provider default |
| `--strict` | Use the pure-Rust VM-only harness with direct OAuth | Off |
| `--prompt <TEXT>` | Mission instructions for the agent | *Required* |
| `--cycles <N>` | Total lifetime cycle budget for the experiment | `60` virtual seconds (`clockHz × 60`) |
| `--turns <1..24>` | Initial turn allowance (`standard` / `maximize` modes) | `6` |
| `--mode <MODE>` | `standard`, `resource-limit`, or `maximize` | `standard` |
| `--tell-turns <BOOL>` | Disclose remaining turn budget to the agent (`true`/`false`) | `true` |
| `--port <PORT>` | Local HTTP observer port | Unused ephemeral port |
| `--lan` | Bind to local IPv4 interfaces and print private LAN links | Loopback only |
| `--no-open` | Print observer URL without launching a browser | Opens browser |
| `--dir <PATH>` | Explicit experiment storage directory | `~/.picos/experiments/<NAME>` |

---

## Experiment Modes & Turn Controls

```sh
# Run until a hardware/cycle limit is reached
picos new terminal --mode resource-limit --prompt "build a terminal"

# Iteratively optimize across a fixed turn budget without early unchanged-turn stopping
picos new terminal-best --mode maximize --turns 12 --tell-turns false --prompt "build a terminal"
```

- **`standard`**: Runs bounded iterations (up to 40 tool calls per granted turn). Automatically stops when the turn allowance is exhausted, the cycle ceiling is hit, the agent halts/fails, or two consecutive turns make no machine progress. Operators can grant 1–24 additional turns from the browser UI.
- **`resource-limit`**: Iterates without a turn or tool-call ceiling until RAM capacity, disk/boot-image capacity, an out-of-bounds fault, or the lifetime cycle budget is breached.
- **`maximize`**: Uses the full turn budget to refine functionality, robustness, usability, and performance without stopping early after unchanged turns.

**Turn Budget Disclosure (`--tell-turns`)**: Controls whether future prompts and `machine_info` reveal the remaining turn/tool allowance to the agent. Can be toggled live from the browser controls.

---

## Live Browser & Mobile Observer

The browser UI observes guest memory directly without injecting host-rendered fonts, windows, or sprites:

1. **Framebuffer Display**: Renders the live `256 × 192` RGB332 guest VRAM canvas alongside serial output.
2. **Live RAM Disassembler & Inspector**: Disassembles current RAM bytes on the fly (including self-modifying code), shows register values, highlights the program counter (`Follow PC`), and tracks recently modified byte ranges (`Follow writes`) with before/after previews and a RAM activity map.
3. **Agent Journal**: Displays public agent progress updates, tool invocations, argument/result previews, and exact submitted prompts (stored in `journal.jsonl`).
4. **Session Controls**: Grant turns, pause/continue execution, switch modes while idle, recover from transient provider errors, or toggle turn budget disclosure.

### Watching from a Phone (`--lan`)

```sh
picos resume terminal --lan
```

Passing `--lan` prints private URLs with a rotating session access key for your machine's local IPv4 network interfaces, featuring a touch-optimized layout with dedicated **Display**, **Memory**, **Journal**, and **Controls** views.

---

## Project Layout

```text
.
├── Cargo.toml                 # Package manifest (crate: smol_os, binary: picos)
├── src/
│   ├── main.rs                # Binary entry point
│   ├── lib.rs                 # Library root
│   ├── vm.rs                  # smol32 CPU, RAM, framebuffer, disk, and I/O ports
│   ├── assembler.rs           # smol32 cross-assembler
│   ├── disassembler.rs        # Live RAM byte disassembler
│   ├── mcp.rs                 # Narrow stdio MCP server and tool schema validation
│   ├── strict.rs              # Pure-Rust strict agent runner
│   ├── agent.rs               # Agent orchestration and Codex process runner
│   ├── direct_auth.rs         # Direct ChatGPT OAuth login & token management
│   ├── direct_provider.rs     # Direct subscription inference client
│   ├── session.rs             # Experiment state, modes, checkpoints, and budgets
│   ├── journal.rs             # Persistent event and prompt ledger (journal.jsonl)
│   ├── server.rs              # HTTP observer server and operator control endpoints
│   └── cli.rs                 # CLI argument parser and command dispatch
├── public/                    # Zero-build web observer frontend (index.html, app.js, style.css)
├── examples/                  # Sample smol32 assembly programs (blank, echo, signal, sealed-interpreter)
├── docs/                      # Architecture specs, ISA reference, and validation records
└── tests/                     # Integration test suite (VM, disassembler, MCP, CLI, server, strict mode)
```

---

## Development & Testing

```sh
# Run the full integration and unit test suite
cargo test

# Lint all targets with Clippy
cargo clippy --all-targets -- -D warnings

# Run a local release build
cargo run --release -- new terminal --no-open --prompt "build a terminal"
```

---

## Documentation

- [**Experiment & System Specification (`docs/SPEC.md`)**](docs/SPEC.md)
- [**`smol32` Instruction Set & Assembler Reference (`docs/ISA.md`)**](docs/ISA.md)
- [**Strict Execution Boundary (`docs/STRICT.md`)**](docs/STRICT.md)
- [**Direct Subscription Authentication (`docs/subscription-auth.md`)**](docs/subscription-auth.md)
- [**Sealed Native Compiler Proof (`docs/sealed-interpreter.md`)**](docs/sealed-interpreter.md)
- [**Validation & Acceptance Records (`docs/VALIDATION.md`)**](docs/VALIDATION.md)
