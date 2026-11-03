; SIGNAL / every pixel is a guest memory write.
; A small RGB332 interference field. No host drawing calls.
mov r0, VIDEO_BASE
mov r1, 0
row:
mov r2, 0
column:
mov r3, r2
xor r3, r1
shr r3, 3
and r3, 7
shl r3, 2
mov r4, r2
add r4, r1
shr r4, 5
and r4, 7
shl r4, 5
or r3, r4
store8 r3, r0
add r0, 1
add r2, 1
cmp r2, VIDEO_WIDTH
jlt column
add r1, 1
cmp r1, VIDEO_HEIGHT
jlt row
mov r0, message
print:
load8 r1, r0
cmp r1, 0
je done
out r1, 0
add r0, 1
jmp print
done:
halt
message:
.asciz "smol / signal acquired\n49,152 pixels. One tiny machine.\n"
