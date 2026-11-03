; Keyboard bytes become serial output and framebuffer pixels.
; With no input, yield to the observer; run again after typing.
mov r1, VIDEO_BASE
poll:
in r0, 1
cmp r0, 0xffffffff
je wait
out r0, 0
store8 r0, r1
add r1, 1
cmp r1, RAM_BYTES
jlt poll
mov r1, VIDEO_BASE
jmp poll
wait:
yield
jmp poll
