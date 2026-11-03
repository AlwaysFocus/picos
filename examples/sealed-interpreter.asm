; A tiny guest compiler/REPL, suitable for running after machine_seal.
; Input: unsigned decimal literals joined by + or *, terminated by LF.
; Evaluation is left-to-right; each literal is 0..65535, at most 16 terms.
; Results wrap to 32 bits, as defined by the CPU. Spaces are not accepted.
; The guest parses input, emits native instructions, executes them, prints
; the result, and saves both code and result to raw disk. No host eval.
;
; RAM 4096..4231: generated code (16 arithmetic instructions plus ret).
; Disk 4096: 'S'; 4097: code length; 4098..4101: result; 4102..: code.
; On reboot, S= is the saved result and R= is the result of rerunning the
; recovered machine code. The boot image must remain below address 4096.
.equ CODE, 4096
.equ RECORD, 4096
.equ MAX_CODE, 136

; Reload only a committed record; its marker is written last by save.
mov r1, RECORD
out r1, 2
in r0, 3
cmp r0, 'S'
jne new_line
in r10, 3
cmp r10, 16
jlt new_line
cmp r10, MAX_CODE
jgt new_line
mov r0, r10
and r0, 7
cmp r0, 0
jne new_line
mov r12, 0
mov r2, 0
restore_result:
in r0, 3
shl r0, r2
or r12, r0
add r2, 8
cmp r2, 32
jlt restore_result
mov r1, CODE
mov r2, r10
restore_code:
in r0, 3
store8 r0, r1
add r1, 1
sub r2, 1
cmp r2, 0
jne restore_code
mov r0, 'S'
out r0, 0
mov r0, '='
out r0, 0
mov r0, r12
call print_number
mov r1, CODE
call r1
mov r12, r0
mov r0, 'R'
out r0, 0
mov r0, '='
out r0, 0
mov r0, r12
call print_number

new_line:
mov r4, 0                   ; current literal
mov r5, 3                   ; pending opcode: mov r0, immediate
mov r6, 0                   ; current literal has digits
mov r7, 0                   ; emitted term count
mov r8, CODE                ; code write cursor
read_character:
in r0, 1
cmp r0, 0xffffffff
je wait_character
cmp r0, '0'
jlt separator
cmp r0, '9'
jgt separator
sub r0, '0'
mul r4, 10
add r4, r0
cmp r4, 65535
jgt bad_line
mov r6, 1
jmp read_character
wait_character:
yield
jmp read_character

separator:
cmp r6, 0
je bad_line
cmp r0, '+'
je emit_term
cmp r0, '*'
je emit_term
cmp r0, 10
jne bad_line
emit_term:
cmp r7, 16
jge bad_line
; Encoding is opcode/r0/immediate/reserved, followed by the literal word.
store32 r5, r8
add r8, 4
store32 r4, r8
add r8, 4
add r7, 1
cmp r0, 10
je execute_line
mov r5, 4                   ; add r0, immediate
cmp r0, '+'
je next_number
mov r5, 6                   ; mul r0, immediate
next_number:
mov r4, 0
mov r6, 0
jmp read_character

execute_line:
mov r1, 27                  ; ret
store32 r1, r8
add r8, 4
mov r1, 0
store32 r1, r8
add r8, 4
mov r1, CODE
call r1                     ; generated native instructions execute here
mov r12, r0
mov r0, '='
out r0, 0
mov r0, r12
call print_number
call save
jmp new_line

; Reject the entire line, including overlong code, without executing it.
; The next LF re-synchronizes the grammar. No unbounded input buffer exists.
bad_line:
cmp r0, 10
je report_error
discard:
in r0, 1
cmp r0, 0xffffffff
je wait_discard
cmp r0, 10
jne discard
report_error:
mov r0, 'E'
out r0, 0
mov r0, 'R'
out r0, 0
out r0, 0
mov r0, 10
out r0, 0
jmp new_line
wait_discard:
yield
jmp discard

; Decimal rendering uses the guest stack; at most ten digits for a u32.
; Only r0, r1, r2, r3 are scratch. All disk/compiler registers are preserved.
print_number:
mov r2, 0
digits:
mov r1, r0
mod r1, 10
add r1, '0'
push r1
add r2, 1
div r0, 10
cmp r0, 0
jne digits
print_digit:
pop r3
out r3, 0
sub r2, 1
cmp r2, 0
jne print_digit
mov r0, 10
out r0, 0
ret

save:
; Invalidate any previous record before writing the replacement.
mov r1, RECORD
out r1, 2
mov r0, 0
out r0, 3
mov r10, r8
sub r10, CODE
out r10, 3
mov r0, r12
mov r2, 4
save_result:
out r0, 3
shr r0, 8
sub r2, 1
cmp r2, 0
jne save_result
mov r1, CODE
mov r2, r10
save_code:
load8 r0, r1
out r0, 3
add r1, 1
sub r2, 1
cmp r2, 0
jne save_code
mov r1, RECORD
out r1, 2
mov r0, 'S'
out r0, 3
ret
