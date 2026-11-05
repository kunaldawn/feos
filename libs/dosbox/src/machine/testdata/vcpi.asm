; A VCPI client for the memory manager's tests: it runs under the monitor, takes
; protected mode at level 0 with its own tables, and comes back through the server.
; It ends with exit code 42 if it all worked, and the stage it stopped at if not.
;
;   nasm -f bin vcpi.asm -o vcpi.com
bits 16
org 0x100

%define CODE32 0x08
%define DATA 0x10
%define SERVER 0x18
%define TSS 0x30

start:
    ; The monitor's world: PE set, IOPL 3, and a CR0 read is emulated.
    mov byte [stage], 1
    smsw ax
    test al, 1
    jz near fail
    pushf
    pop ax
    and ax, 0x3000
    cmp ax, 0x3000
    jne near fail
    mov eax, cr0
    test al, 1
    jz near fail

    mov byte [stage], 2
    mov ax, 0xDE00
    int 0x67
    test ah, ah
    jnz near fail
    cmp bx, 0x0100
    jne near fail

    ; Pages.
    mov byte [stage], 3
    mov ax, 0xDE03
    int 0x67
    test ah, ah
    jnz near fail
    test edx, edx
    jz near fail
    mov ax, 0xDE04
    int 0x67
    test ah, ah
    jnz near fail
    mov [page], edx
    mov ax, 0xDE05
    int 0x67
    test ah, ah
    jnz near fail
    mov cx, 0x20
    mov ax, 0xDE06
    int 0x67
    cmp edx, 0x20000
    jne near fail

    ; The tables, on paragraph boundaries in this segment.
    mov byte [stage], 4
    xor eax, eax
    mov ax, cs
    shl eax, 4
    mov [base], eax
    ; The page table and directory are on page boundaries above the program.
    mov ax, cs
    add ax, 0x0200
    and ax, 0xFF00
    mov es, ax
    xor di, di
    mov si, gdt + SERVER
    mov ax, 0xDE01
    int 0x67
    test ah, ah
    jnz near fail
    mov [server_entry], ebx
    ; Past what the server filled in, nothing's mapped.
    xor eax, eax
    mov cx, 0x400
    push cx
    mov cx, di
    shr cx, 2
    neg cx
    add cx, 0x400
    rep stosd
    pop cx
    ; The directory's first entry is that table.
    xor eax, eax
    mov ax, cs
    add ax, 0x0200
    and ax, 0xFF00
    shl eax, 4
    or eax, 7
    mov di, 0x1000
    stosd
    xor eax, eax
    mov cx, 0x3FF
    rep stosd

    ; The descriptors that take this segment's base.
    mov eax, [base]
    mov [gdt + CODE32 + 2], ax
    mov [gdt + DATA + 2], ax
    shr eax, 16
    mov [gdt + CODE32 + 4], al
    mov [gdt + DATA + 4], al
    mov eax, [base]
    add eax, tss
    mov [gdt + TSS + 2], ax
    shr eax, 16
    mov [gdt + TSS + 4], al
    mov eax, [base]
    add eax, gdt
    mov [gdtr + 2], eax
    mov eax, [base]
    add eax, idt
    mov [idtr + 2], eax
    mov eax, [base]
    add eax, gdtr
    mov [switch_gdtr], eax
    mov eax, [base]
    add eax, idtr
    mov [switch_idtr], eax
    xor eax, eax
    mov ax, cs
    add ax, 0x0200
    and ax, 0xFF00
    shl eax, 4
    add eax, 0x1000
    mov [switch_cr3], eax
    mov dword [switch_eip], pm_entry
    mov word [switch_cs], CODE32
    mov [back_cs], cs
    mov [back_ss], ss
    mov [back_ds], ds
    mov [back_es], ds
    mov [back_fs], ds
    mov [back_gs], ds
    mov eax, 0xFFF0
    mov [back_sp], eax

    ; Over the wall.
    mov byte [stage], 5
    cli
    mov esi, [base]
    add esi, switch
    mov ax, 0xDE0C
    int 0x67
    ; Not reached.
    jmp fail

bits 32
pm_entry:
    mov ax, DATA
    mov ds, ax
    mov ss, ax
    mov es, ax
    mov esp, pm_stack_top
    mov dword [pm_marker], 0x12345678
    ; A VCPI call from protected mode.
    mov ax, 0xDE03
    call far [server]
    mov [pm_pages], edx
    ; Back: the stack holds what the far return needs and where to go.
    mov ax, 0xDE0C
    cli
    mov esp, stack_frame + 8
    call far [server]
bits 16
back:
    ; Virtual-8086 mode again.
    mov byte [stage], 6
    sti
    cmp dword [pm_marker], 0x12345678
    jne near fail
    cmp dword [pm_pages], 0
    je near fail
    smsw ax
    test al, 1
    jz near fail
    mov ax, 0x4C2A
    int 0x21

fail:
    mov al, [stage]
    mov ah, 0x4C
    int 0x21

stage: db 0
    align 4
page: dd 0
base: dd 0
pm_marker: dd 0
pm_pages: dd 0
server:
server_entry: dd 0
    dw SERVER

switch:
switch_cr3: dd 0
switch_gdtr: dd 0
switch_idtr: dd 0
    dw 0
    dw TSS
switch_eip: dd 0
switch_cs: dd 0

    align 4
gdtr: dw gdt_end - gdt - 1
    dd 0
    dw 0
idtr: dw 0x7FF
    dd 0
    dw 0

    align 8
gdt:
    dq 0
    ; 32-bit code and data with this segment's base, up to 1 MiB.
    dw 0xFFFF, 0
    db 0, 0x9A, 0x4F, 0
    dw 0xFFFF, 0
    db 0, 0x92, 0x4F, 0
    ; The server's three.
    dq 0, 0, 0
    ; The TSS.
    dw 103, 0
    db 0, 0x89, 0, 0
gdt_end:

tss: times 26 dd 0

    align 16
idt: times 256 dq 0

stack_frame:
    dd 0, 0            ; the far return the call replaces
back_ip: dd back
back_cs: dd 0
    dd 0x0202
back_sp: dd 0
back_ss: dd 0
back_es: dd 0
back_ds: dd 0
back_fs: dd 0
back_gs: dd 0

    times 256 db 0
pm_stack_top:
