; Enters protected mode as a 32-bit client, then calls INT 31h with pseudo-random
; registers until the machine stops it. The seed is the immediate at offset 3.
;   nasm -f bin dpmifuzz.asm -o dpmifuzz.com
bits 16
org 0x100
    jmp short start
    db 0
seed: dw 0x1234
start:
    mov ah, 0x4A
    mov bx, 0x1000
    int 0x21
    mov ax, 0x1687
    int 0x2F
    test ax, ax
    jnz done
    mov [entry], di
    mov [entry + 2], es
    mov bx, si
    mov ah, 0x48
    int 0x21
    jc done
    mov es, ax
    mov ax, 1
    call far [entry]
    jc done
    mov ax, ds
    mov es, ax
    mov ax, [seed]
    mov [state], ax
again:
    ; A generator with a period long enough for this.
    mov eax, [state]
    imul eax, eax, 1664525
    add eax, 1013904223
    mov [state], eax
    mov ebx, eax
    rol ebx, 7
    mov ecx, eax
    rol ecx, 13
    mov edx, eax
    rol edx, 21
    mov esi, eax
    rol esi, 3
    and si, 0x7FFF
    mov edi, ebx
    and di, 0x7FFF
    ; A function near the ones that exist, avoiding those that leave for good.
    shr eax, 8
    and ax, 0x0FFF
    cmp ax, 0x0300
    je skip
    cmp ax, 0x0301
    je skip
    cmp ax, 0x0302
    je skip
    int 0x31
skip:
    mov ax, ds
    mov es, ax
    jmp again
done:
    mov ax, 0x4C00
    int 0x21
entry: dd 0
state: dd 0x12345678
