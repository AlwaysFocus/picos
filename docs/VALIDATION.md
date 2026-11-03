# Rust validation

## Public agent journal / 2026-10-29

- Full suite: 84 tests passed; the subsequent turn-transition regression test also passed (85 tests total). Clippy with warnings denied, Rust formatting, and browser JavaScript syntax passed.
- Public-event tests reject reasoning/analysis items and untyped provider output. The integration test compares the recorded picos prompt with the exact stdin received by a local Codex stand-in, checks incremental cursors, rejects an invalid cursor, and verifies journal persistence after resume without leaking the operator capability.
- The turn-transition test verifies final queued messages are recorded under their original turn/session before continuation. Journal history is bounded in observer responses, while new public records and submitted prompts append to `journal.jsonl`.
- The existing subscription experiment `experiment-1793326210-5914f6` was saved, upgraded, and resumed on port 60172 with its original gpt-6-luna / 300 MHz / 24 MB / 200 MB configuration and unchanged 30-turn ceiling. The final live check showed turn 25/30, 339 cumulative machine calls, and 125 memory updates. Interrupted turns remain counted.
- Browser acceptance verified the visible current goal, retained history after restart, fresh public progress messages, exact submitted prompt expansion, message/tool filters, expandable arguments/results, unread counts, and Recent changes navigation back to live memory. No console errors were observed during journal interaction.
- Screenshot: [agent-journal.jpg](agent-journal.jpg). This records public explanations and tool evidence, not private chain-of-thought; older runs cannot supply prompt snapshots they never recorded. Guest terminal correctness is outside this journal acceptance check.

## Picos 0.2 CLI and observer

Verified October 29, 2026 (America/Edmonton). The command accepts both flag and keyword forms, uses exact decimal MB, and preserves the requested model. `picos login status` confirmed the existing ChatGPT authentication without changing credentials.

A real `picos new` invocation used `--cpu 300 --ram 24 --storage 200 --model gpt-6-luna` with a bounded mission to draw a small terminal prompt. The run completed through the authenticated MCP bridge:

- RAM: exactly 24,000,000 bytes; raw disk: exactly 200,000,000 bytes.
- Boot image: 1,557 bytes; 246,017 executed instructions; 295,421 virtual cycles.
- Serial: `PICOS READY` followed by a newline; no VM fault; unsealed.
- Framebuffer: 49,152 bytes, with 59 white pixels against black. The agent reported a `smol>` prompt from the earlier machine naming; this demonstration is a rendered prompt, not an interactive terminal.
- The independent observer endpoint decoded the real bytes as `mov r1, 0`, `mov r2, 23950848`, `mov r3, 49152`, and `store8 r1, r2` at the first four instruction addresses.

See [cli-acceptance.json](cli-acceptance.json) for the requested model, successful turn, hardware state, raw-byte disassembly, and mission. This establishes access to the requested model identifier for this turn; it is not an independent verification of the provider's underlying model build.

The process integration test uses the same 24 MB / 200 MB profile with a local Codex stand-in and actual Rust MCP calls. It checks the model argument, real guest execution, read-only observer responses, duplicate-directory refusal, concurrent-open locking, Ctrl+C persistence, and exact resume without another authentication or inference call. Large snapshot round-trips make this test take roughly 42 seconds in debug builds.

The browser implementation makes GET requests only. Syntax, element references, RGB332 rendering code, changed-byte highlighting, manual address navigation, and unaligned-PC following were checked with a mock DOM. Actual browser layout and clicking remain unverified because browser automation was unavailable.

## Earlier engine acceptance

Verified on 2026-10-30 UTC with Rust 1.94.1 and Codex CLI 0.165.0-alpha.1.2. The application uses a custom Rust CPU and assembler; the test below did not execute guest code in a JavaScript runtime.

## Automated checks

`cargo test` passes. Coverage includes unsigned wrapping arithmetic, stack/branch behavior, self-modifying machine code, malformed binaries and snapshots, integer/address overflow, RAM and disk limits, whole-instruction cycle accounting, lifetime-budget retention across reboot and assembly, keyboard/serial/raw-disk devices, sealed-mode writes, checkpoint restore, actual HTTP service behavior, cross-origin rejection, narrow MCP tools, PNG decoding, and subscription-only CLI configuration.

`cargo clippy --all-targets -- -D warnings` passes. `cargo build --release` succeeds. Browser JavaScript syntax and static element references were checked separately; these do not constitute a browser interaction test.

## Real agent acceptance

The operator started the Rust workbench and submitted a small mission through `smol agent`. The runner used the existing ChatGPT sign-in and the CLI's default model. No API key or separate API provider was used. The actual model identity was not separately verified.

The agent made four machine calls: `machine_info`, `machine_assemble`, `machine_run`, and `machine_screen`. The last call returned a PNG produced from the guest RAM framebuffer. The agent completed the turn after observing the output.

| Observation | Verified result |
| --- | --- |
| Guest hardware | 300 MHz virtual, 8 MiB RAM, 32 MiB disk |
| Binary boot image | 181 bytes in RAM and disk |
| Executed instructions | 15,680 |
| Virtual cycles consumed | 18,945 |
| Virtual time | 0.00006315 seconds |
| Serial output | `SMOL ONLINE` followed by a newline |
| Display | Green RGB332 value 28 at x=96..159, y=72..119; all other pixels zero |
| Pixel check | All 49,152 positions checked; exactly 3,072 green pixels |
| RAM bytes touched | 3,253 (181-byte image plus 3,072 pixels) |
| Machine state | Halted, no fault, unsealed |

The complete source and measured state are in [acceptance.json](acceptance.json). A local `rust-first-light` checkpoint preserves the machine under `.smol/checkpoints/`. Observer snapshots and source files are outside the guest resource budget and do not establish self-hosting.

## Remaining validation

The browser workbench could not be visually inspected or clicked through: local browser access was disabled in Work settings, and the native Mac was locked. API integration tests cover the control behavior, and raw pixel checks establish the guest display contents. Visual layout, focus, and browser keyboard handling still need an interactive acceptance pass.

The live test proves subscription authentication and a complete assemble/run/observe loop. It does not establish how far an agent can evolve a filesystem, compiler, or window system; those are experiments to run on this machine. The virtual clock is an abstract instruction-cost model, not a real CPU performance claim.

## Observable sessions / 2026-10-29

- `cargo test`: 66 tests passed (including real loopback CLI/server tests, with Codex stand-ins).
- `cargo clippy --all-targets -- -D warnings`: passed. `cargo fmt` applied; browser JavaScript syntax checked.
- Continuation integration: two separate model processes execute against the same machine; four machine tool calls and36 lifetime cycles remain cumulative. Single-turn save/resume acceptance retains requested24 MB RAM /200 MB disk and launches no inference on plain resume.
- RAM evidence: first writes, guest writes, write-then-restore, unchanged writes, atomic boot/reassembly deltas, bounded previews/ranges, monotonic event IDs, and history save/restore tested.
- Live subscription experiment `experiment-1793326210-5914f6` resumed on port60172 with `gpt-6-luna`,300 MHz,24 MB RAM,200 MB storage and six iterations. Browser inspection confirmed automatic progress to TURN2/6 and eight actual memory-update events, plus guest-rendered text. This verifies continuation, not terminal completeness.
- Browser at863px width: verified visible phase/turn panel, before/after preview, RAM map, recent changes, assembly navigation and manual/Follow writes toggling. No browser console errors observed. Screenshot: `observer-activity.jpg`. Very narrow/mobile layout and reduced-motion emulation were not exercised interactively.

## Experiment modes and turn grants / 2026-10-29

- Full suite:81 tests passed. Clippy with warnings denied, Rust formatting, and browser JavaScript syntax passed.
- Resource-limit integration exceeded its supplied one-turn allowance, then stopped on the next instruction that could not fit its40-cycle ceiling. A separate out-of-bounds RAM-write test stopped after one tool call and rejected the queued follow-up write.
- Browser-control HTTP tests verify operator capability requirements, cross-origin rejection, standard-mode turn grants, cumulative cycles/tools, disclosure persistence, token rotation, and idle save/resume.
- Session policy tests cover all three modes, hidden/disclosed budget prompts, finite maximize behavior, standard grants during active/paused/stopped states, nonrefunded resources, legacy migration, and validated serialized policies.
- Browser verification on the saved terminal: mode selection/apply (standard→resource-limit→standard), disclosure off/on, no implicit inference from settings, enabled additional-turn field/button, no console errors. The field/button's mutation behavior was exercised through the real HTTP integration test rather than spending another live-model turn.
- Fixed intrinsic canvas overflow found during screenshot review; final display is contained above its footer. `experiment-controls.jpg` records the verified desktop layout. The original experiment remains idle in standard mode with disclosure enabled; its RAM/disk and14,462,720 consumed cycles are preserved.


## Strict harness and sealed compiler / 2026-10-29

- Full integration suite passed with loopback networking enabled. After the checkpoint portability fix, all43 library tests passed again; combined coverage is111 tests. Clippy with warnings denied, Rust formatting, browser JavaScript syntax, release build, and CLI installation passed.
- Direct provider/authentication fixtures verify bounded completed streams, exact VM tools, rejection of unknown/indirect/incomplete calls, cancellation, model catalog parsing, signed identity claims, PKCE/state/nonce, private credential storage, and refresh recovery. No live account credentials are used by these tests.
- Missing direct authorization fails before creating an experiment even with an API-key environment variable and a Codex binary configured. No fallback executes. Strict mode persists through resume, runner recreation, and portable checkpoint copies; importing legacy/false-marked checkpoints cannot downgrade an existing strict machine.
- The sealed guest fixture uses65,536 bytes RAM,8,192 bytes disk,300 MHz virtual time and100,000 lifetime cycles. Its1,328-byte bootstrap compiler turns fresh `12+30*2` input into32 bytes of native code and prints84 (left-to-right semantics). Parsing/compiling/executing/saving consumes1,413 cycles; guest reload/reexecution after reboot consumes1,247. Further input works without external assembly or writes. Separate tests cover invalid/partial input, code capacity, CPU exhaustion, RAM/disk faults, and blocked external injection.
- Direct ChatGPT subscription support is documented by OpenAI. Account authorization, model visibility and completed live inference remain distinct acceptance steps; the offline tests above do not establish live eligibility. The existing terminal experiment was not changed during this implementation.
- Offline browser check at1280px: strict VM-tools-only/bootstrap label, goal and journal render correctly; no browser console errors. This preview did not invoke a model.


## Direct subscription live-stream repair / 2026-10-29

- Reproduced the reported `gpt-6.1-terra` failure. The actual HTTP200 response omitted Content-Type but contained valid SSE. It also emitted `response.metadata` and delivered completed function items through `response.output_item.done`, with an empty terminal output array.
- The transport now permits an absent Content-Type while retaining bounded SSE parsing and rejection of explicit incompatible types. Metadata remains transport-only. Indexed completed items are sanitized and buffered; they are released only after successful `response.completed`. Duplicate/gapped indices, conflicting terminal output, invalid tools, incomplete/failed responses and premature EOF remain rejected. Safe event-type diagnostics help identify future protocol changes without dumping private content.
- All116 tests passed, including12 provider tests. Clippy with warnings denied, Rust formatting, JavaScript syntax, release build and installation passed.
- Reran the user's exact `picos new programming-language --strict --cpu 300 --ram 24 --storage 200 --model gpt-6.1-terra --prompt "Build and test a basic programming language"` command. Preserved earlier failed directories separately before repeating `new`; no failed run was deleted.
- Direct ChatGPT inference completed the first turn and automatically began turn2/6. It assembled a1,853-byte guest expression interpreter, injected test input, executed guest instructions, inspected memory, received a framebuffer image, rebooted and retested. Serial results included14 and13 for precedence/variable calculations, ERROR for invalid inputs, and42 after reboot. First-turn use was249,990 lifetime guest cycles with no CPU fault. These results establish actual subscription/tool-loop execution; they do not establish a finished general-purpose language or self-hosting.
- The observer at localhost:61033 showed the exact goal, gpt-6.1-terra, strict mode, memory updates and the public journal. The live experiment was left running for the remaining turns. See [direct-subscription-acceptance.json](direct-subscription-acceptance.json) for the recorded command and evidence.

## LAN sharing and phone observer / 2026-10-29

- All119 tests passed, including CLI LAN flags and actual HTTP tests for unauthenticated observer rejection, authenticated state/journal/frame/disassembly, operator controls, Host/Origin policy and the loopback-only machine tool boundary. Clippy with warnings denied, Rust formatting, browser JavaScript syntax, release build and CLI installation passed.
- Verified the observer through the host's Wi-Fi IPv4 address. An anonymous API request returned401/access_required; the private key returned200. A bare address displayed the connection form without guest data. Key entry connected successfully, reload retained tab-session access, and the URL fragment was scrubbed.
- Interactive browser checks at390×844 covered Display, Memory address navigation, journal filtering and Controls. Mode changes were tested against an isolated checkpoint copy without inference. All four workspaces fit320×568 without page overflow;844×390 landscape preserved the guest framebuffer aspect ratio. Inputs and navigation have touch-sized targets. The HTTP clipboard fallback successfully copied the goal.
- Desktop1280×900 retained the two-column observer with no horizontal overflow. No browser console errors were observed. Screenshot: [mobile-observer.jpg](mobile-observer.jpg). These are browser viewport checks, not a physical-phone acceptance test.
- Reopened the actual programming-language experiment on port61033 using `--lan`, preserving19,549,915 cycles,24 MB RAM and200 MB storage. Turn4 had stopped on a direct-provider request timeout before the server upgrade; plain resume started no model turn. No existing experiment was reset or overwritten. The temporary preview server was stopped after validation.

## Provider transport and explicit session recovery / 2026-10-29

- Closer timing inspection corrected the earlier timeout assumption: the generic failure appeared125 ms after the last completed VM tool, in the request-before-headers path. The previous error discarded the underlying transport classification, so the historical network cause cannot be determined exactly. It was not evidence of a300-second timeout, HTTP authentication error or quota rejection.
- Direct transport now permits three attempts for connection/stream interruptions, with cancellable1s/2s backoff inside the original300-second request deadline. Failed attempts discard pending tools. Authentication/quota errors and protocol/schema failures do not retry. Safe diagnostics include phase/category/elapsed time without dumping tokens or provider content. A retry notice cannot masquerade as a model report.
- Failed sessions expose explicit continuation with remaining turns; a failed final standard-mode turn permits a new grant. No implicit whole-turn retry or resource refund occurs. The UI retains the concrete failure and explains the recovery action instead of incorrectly calling every failure a hard resource stop.
- The initial127-test full suite passed, including local HTTP connection-drop/retry fixtures, incomplete-stream buffer discard, bounded attempts, cancellation during backoff, non-retryable errors, abandoned report rejection, and failed-session save/restore/continue with exact VM/tool/cycle preservation. Clippy with warnings denied, formatting, JavaScript syntax and installation passed.
- Clicked **Resume after failure** in the real observer. The preserved programming-language experiment advanced from turn4/6 to5/6 with the same mission, model, strict boundary and hardware limits. Successful direct requests invoked info/screen/reset/input/run/inspect; observed73 cumulative tools and23,549,911 cycles, up from65 tools and19,549,915 cycles before recovery. Agent status was running with no error. This is live recovery evidence, not a guarantee that future provider/network outages cannot occur.
- The live turn5 then exposed a second failure: HTTP503/subscription_sharing_usage_unavailable. Added bounded retries for explicit temporary service errors, honoring short numeric Retry-After values and leaving auth/quota/unknown failures non-retryable. The17-provider-test suite passed including a503→completed recovery and non-retryable quota/auth/long-delay cases. Reinstalled and explicitly resumed turn6 with the same saved VM.
- Final full suite:128 tests passed. Live turn6 completed successfully and the session reached its ordinary six-turn allowance. Its report records reboot/load persistence (40320 and144), a guest-language prime program (15 primes below51), framebuffer inspection, Ctrl-C/BREAK and recovery with PRINT123. Lifetime cycles reached49,049,896 with no CPU fault reported. The observer offers Add turns; no further grant was made. This live completion verifies recovery from the paused state; the synthetic503 fixture establishes automatic temporary-service retry behavior.
- Checked updated controls at390×844 without horizontal overflow. Screenshot: [provider-recovery.jpg](provider-recovery.jpg). The local-network observer was left open with its newly rotated private link.
