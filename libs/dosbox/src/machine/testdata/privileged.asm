; Tries to take the CPU, as an extender that doesn't know about VCPI would.
;   nasm -f bin privileged.asm -o privileged.com
bits 16
org 0x100
    mov eax, cr0
    or al, 1
    lgdt [gdtr]
    mov cr0, eax
    mov ax, 0x4C2A
    int 0x21
gdtr: dw 0
    dd 0
