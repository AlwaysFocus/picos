# Sealed guest compiler acceptance

`examples/sealed-interpreter.asm` is a small compiler and REPL written entirely
in the picos ISA. It is a test fixture, not software secretly supplied to normal
experiments or a claim that the model built a compiler.

The one-time bootstrap assembler loads this program. The test seals the machine
**before supplying the expressions**. After that point the host only delivers
keyboard bytes, requests bounded execution slices, reads evidence, and reboots.
It never evaluates an expression, emits its native code, formats its answer, or
writes its saved result. Those operations all execute as charged guest CPU
instructions.

## Language

An expression consists of decimal literals separated by `+` or `*`, followed by
LF. Evaluation is left-to-right: `12+30*2` means `(12+30)*2` and prints `=84`.
Each literal is 0 through 65535; arithmetic wraps to the ISA's unsigned 32 bits.
The parser permits at most sixteen terms. Spaces, negative literals, other
operators, empty expressions, missing operands, and out-of-range literals yield
`ERR`. An error discards the rest of that line before accepting another one.
Incomplete input yields without losing the parser state.

For each term the guest emits a native `mov r0, literal`, `add r0, literal`, or
`mul r0, literal` instruction, followed by `ret`. It calls the generated RAM
address, formats the returned integer using guest division and stack operations,
and saves the code and result through the raw disk ports.

## Fixed storage and recovery

- The bootstrap image is below address 4096 in RAM and disk.
- RAM addresses 4096 through 4231 hold up to 136 bytes of generated code.
- Disk address 4096 contains a committed-record marker, followed by a byte of
  code length, four little-endian result bytes, and the generated code.
- The stack begins at `VIDEO_BASE`; the compiler's normal input path does not
  draw pixels or allocate framebuffer storage.
- Saving invalidates the old record first and writes the commit marker last.
  A failed partial save therefore does not advertise a valid replacement.
- Reboot clears the generated RAM region. Guest boot instructions read the
  saved record, print its result as `S=84`, reload its native code, execute it,
  and print the recomputed result as `R=84`. Reboot preserves the seal and
  cumulative cycle count.

This demonstrates guest compilation and persistence with bounded resources. It
does not prove every future agent-built helper is correct or isolated; the
strict harness and VM must enforce that independently. Input delivery, initial
assembly/loading, reboot copying, model inference, and observer artifacts remain
declared experiment infrastructure rather than guest CPU instructions.

## Run the acceptance tests

```sh
cargo test --offline --test sealed_interpreter -- --nocapture
```

The tests check exact machine bytes and constant expected answers; they do not
contain a host-side expression interpreter. They also verify malformed and
partial input, the sixteen-term code limit, continued operation after a rejected
line, refusal of external injection after sealing, irreversible cycle
consumption, and RAM/disk faults for deliberately undersized guest allocations.
