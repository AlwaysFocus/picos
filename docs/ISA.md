# smol32 instruction set / version 1

## Machine and binary format

The CPU has sixteen unsigned 32-bit registers, `r0` through `r15`, plus a byte-addressed program counter and a comparison result. Arithmetic wraps modulo 2^32. `div`, `mod`, comparisons and right shift are unsigned. Shift counts use their low five bits. `mul` retains the low 32 bits of the product.

Instructions are eight bytes, with no alignment requirement:

| Byte offset | Field |
| --- | --- |
| 0 | Opcode, unsigned byte |
| 1 | Destination/value register index, 0..15 |
| 2 | Operand kind: 0 = immediate, 1 = register |
| 3 | Reserved; must be zero |
| 4..7 | Immediate value or source-register index, unsigned little-endian 32-bit |

Single-operand jumps, `call` and `push` use the operand field and leave byte 1 zero. `pop` uses byte 1; its operand field is zero. No-operand instructions leave all fields after the opcode zero. Unknown opcodes, invalid register indices, invalid operand kinds and a nonzero reserved byte fault the CPU.

All RAM is mutable, including instructions. Generated code can run from any valid RAM address. Fetch requires eight bytes in bounds. There is no protection ring, alignment trap, hidden allocator, guest filesystem or host-language execution facility.

## Assembly notation

`rD` and `rV` mean a register. `value`, `address`, `target` and `port` each accept an immediate expression or a register. Except for `cmp`, arithmetic changes its first register; for example, `add r0, 5` means `r0 = r0 + 5`. Memory addresses are plain operands, without square brackets. `store8 r1, r2` stores the low byte of r1 at the address in r2.

| Opcode | Instruction | Effect | Cycles |
| --- | --- | --- | ---: |
| 0 / 0x00 | `nop` | No change | 1 |
| 1 / 0x01 | `halt` | Stop until reboot | 1 |
| 2 / 0x02 | `yield` | Pause; the next run resumes at the following instruction | 1 |
| 3 / 0x03 | `mov rD, value` | Copy value | 1 |
| 4 / 0x04 | `add rD, value` | Add | 1 |
| 5 / 0x05 | `sub rD, value` | Subtract | 1 |
| 6 / 0x06 | `mul rD, value` | Multiply | 4 |
| 7 / 0x07 | `div rD, value` | Unsigned integer quotient | 16 |
| 8 / 0x08 | `mod rD, value` | Unsigned remainder | 16 |
| 9 / 0x09 | `and rD, value` | Bitwise AND | 1 |
| 10 / 0x0a | `or rD, value` | Bitwise OR | 1 |
| 11 / 0x0b | `xor rD, value` | Bitwise XOR | 1 |
| 12 / 0x0c | `shl rD, value` | Shift left | 1 |
| 13 / 0x0d | `shr rD, value` | Logical shift right | 1 |
| 14 / 0x0e | `load8 rD, address` | Read one byte, zero-extended | 2 |
| 15 / 0x0f | `load32 rD, address` | Read four bytes, little-endian | 2 |
| 16 / 0x10 | `store8 rV, address` | Write low eight bits | 2 |
| 17 / 0x11 | `store32 rV, address` | Write four bytes, little-endian | 2 |
| 18 / 0x12 | `cmp rV, value` | Set comparison to less/equal/greater; do not modify rV | 1 |
| 19 / 0x13 | `jmp target` | Unconditional jump | 1 |
| 20 / 0x14 | `je target` | Jump if equal | 1 |
| 21 / 0x15 | `jne target` | Jump if not equal | 1 |
| 22 / 0x16 | `jlt target` | Jump if unsigned less | 1 |
| 23 / 0x17 | `jle target` | Jump if unsigned less or equal | 1 |
| 24 / 0x18 | `jgt target` | Jump if unsigned greater | 1 |
| 25 / 0x19 | `jge target` | Jump if unsigned greater or equal | 1 |
| 26 / 0x1a | `call target` | Push next PC, then jump | 3 |
| 27 / 0x1b | `ret` | Pop PC | 3 |
| 28 / 0x1c | `push value` | Push four bytes | 2 |
| 29 / 0x1d | `pop rD` | Pop four bytes into rD | 2 |
| 30 / 0x1e | `in rD, port` | Read device port | 16 |
| 31 / 0x1f | `out rV, port` | Write device port | 16 |

`r15` is an ordinary writable register also used as the descending stack pointer. Reset initializes it to `VIDEO_BASE`. Push subtracts four before writing; pop reads then adds four. The guest must manage stack capacity and avoid collisions with code, data and display memory. Comparisons change only through `cmp`; reset initializes the comparison to equal.

Division/remainder by zero, out-of-range memory or disk access and unsupported device operations fault the CPU. A valid decoded instruction spends its full cost even if execution faults. Fetch/decode failures spend no cycles. The program counter remains on a faulting instruction, except that a jump can leave it pointing at an invalid next fetch address.

## Devices

| Port | `in` | `out` |
| --- | --- | --- |
| 0 | Returns zero | Append low byte to serial output |
| 1 | Next keyboard byte; `0xffffffff` if empty | Fault |
| 2 | Current disk byte cursor | Set cursor, from zero through `DISK_BYTES` |
| 3 | Read disk byte and advance cursor | Write low byte to disk and advance cursor |
| 4 | Low 32 bits of lifetime cycle counter, including this instruction | Fault |

Disk I/O at cursor `DISK_BYTES` faults. Cursor movement itself does not allocate storage. The keyboard queue holds at most 4,096 UTF-8 bytes. Serial output retains the most recent 16,384 byte-valued characters for observation. These device queues are bounded host peripherals, not guest-addressable RAM or additional guest files.

The framebuffer is 256 × 192 bytes at `VIDEO_BASE = RAM_BYTES - 49152`. Pixel `(x,y)` is at `VIDEO_BASE + y * 256 + x`. A byte is RGB332: red in bits 7..5, green in bits 4..2, blue in bits 1..0. Zero is black and 255 is white. Drawing, glyphs and windows must be implemented by guest stores. There is no display syscall.

## Assembler

Instructions/registers/directives are case-insensitive; symbols are case-sensitive. Labels such as `loop:` denote byte addresses and can precede a statement on the same line. Comments start with `;` or `//` outside a string. Commas separate operands.

Expressions support decimal, `0x` hexadecimal, `0b` binary, symbols, unary `+`/`-`, addition, subtraction and parentheses. Character literals such as `'A'`, `'\n'`, `'\r'`, `'\t'`, `'\0'`, `'\\'` and `'\''` are accepted as standalone values. There is no expression multiplication, division, bitwise operator or host evaluation. Immediate words accept -2,147,483,648 through 4,294,967,295; negative values encode in two's complement.

Built-in symbols are `RAM_BYTES`, `DISK_BYTES`, `VIDEO_BASE`, `VIDEO_WIDTH` and `VIDEO_HEIGHT`, matched to the current machine configuration.

| Directive | Meaning |
| --- | --- |
| `.equ NAME, expression` | Define a constant; the comma is optional |
| `.byte value, value, ...` | Emit bytes; each value must be 0..255 |
| `.word value, value, ...` | Emit little-endian 32-bit words |
| `.ascii "text"` | Emit UTF-8 bytes from a JSON-style string |
| `.asciz "text"` | Emit UTF-8 bytes followed by a zero byte |
| `.space count` | Emit `count` zero bytes |

Forward label/constant references are supported except where a `.space` count needs a later definition to determine layout. Duplicate/unknown symbols, cyclic constants, malformed instructions and capacity overflow produce diagnostics. Source has a 1 MiB host-side limit. The boot image must fit both the disk and RAM below the framebuffer.

```asm
mov r0, message
next:
load8 r1, r0
cmp r1, 0
je done
out r1, 0
add r0, 1
jmp next
done: halt
message: .asciz "Hello from inside the machine.\n"
```

## Resource accounting and lifecycle

RAM and disk capacities are exact byte-array limits; code, data, stack and video share the same RAM. Assembling/loading persists a boot image on guest disk and copies it into RAM. Reboot clears RAM/registers/input/serial and reloads that disk range; it preserves disk, sealing and lifetime cycle consumption. A guest can overwrite its own boot bytes on disk.

Each run accepts at most 2,000,000 cycles. An instruction executes only if its full cost fits both the slice and the experiment's remaining total allowance. A short slice pauses without partially executing the next instruction. Insufficient lifetime allowance puts the machine in `budget` state. Step executes one complete instruction, subject to the same lifetime limit. `halt` and faults require reboot; `yield` resumes on the next run.

Configured clock Hz converts consumed cycles to simulated seconds; it does not throttle execution or promise host throughput. At 300 MHz, 300,000,000 virtual cycles represent one virtual second. RAM/disk “touched” counts mean distinct bytes written or loaded, including zero writes. They do not measure guest allocator occupancy, free space or reserved address ranges.

The LLM, cross-assembler, editor source, logs and checkpoints are external experiment infrastructure. They do not demonstrate guest self-hosting. Sealing disables external assembly/load and memory/disk injection while preserving guest instructions, guest stores, disk I/O, inspection, execution and keyboard input. Only a new operator-created experiment starts unsealed. Checkpoint restoration is an observer branch and must be labeled when comparing results.
