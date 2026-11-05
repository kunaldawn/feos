; A DPMI client for the host tests: it goes through the host's services and ends
; with an exit code that says how far it got, 42 for all of them. It prints OK
; with INT 21h through a selector, as a translated call.
;
;   nasm -f bin dpmi.asm -o dpmi16.com
;   nasm -f bin -DCLIENT32 dpmi.asm -o dpmi32.com
bits 16
org 0x100

%ifdef CLIENT32
  %define IRET_ iretd
  %define RETF_ o32 retf
  %define FRAME 4
%else
  %define IRET_ iret
  %define RETF_ retf
  %define FRAME 2
%endif

%macro check 1
    mov byte [stage], %1
%endmacro

start:
    ; Shrink to 64 KiB, as a stack and a buffer are at the top of this block.
    mov ah, 0x4A
    mov bx, 0x1000
    int 0x21
    mov ax, 0x1687
    int 0x2F
    test ax, ax
    jnz near fail
    mov [entry], di
    mov [entry + 2], es
    mov [rm_seg], cs
    ; The host's private memory.
    mov bx, si
    mov ah, 0x48
    int 0x21
    jc near fail
    mov es, ax
%ifdef CLIENT32
    mov ax, 1
%else
    xor ax, ax
%endif
    call far [entry]
    jc near fail

    ; Protected mode now.
    check 1
    mov [ds_sel], ds
    mov [cs_sel], cs
    mov [ss_sel], ss
    mov [ds_sel_cs], ds
    mov ax, cs
    and ax, 3
    cmp ax, 3
    jne near pm_fail

    ; Version.
    check 2
    mov ax, 0x0400
    int 0x31
    jc near pm_fail
    cmp ax, 0x005A
    jne near pm_fail

    ; A descriptor with a base: the video buffer's first character.
    check 3
    xor ax, ax
    mov cx, 1
    int 0x31
    jc near pm_fail
    mov [sel_a], ax
    mov bx, ax
    mov ax, 0x0007
    mov cx, 0x000B
    mov dx, 0x8000
    int 0x31
    jc near pm_fail
    mov ax, 0x0008
    mov cx, 0
    mov dx, 0xFFFF
    int 0x31
    jc near pm_fail
    mov ax, 0x0006
    int 0x31
    cmp cx, 0x000B
    jne near pm_fail
    cmp dx, 0x8000
    jne near pm_fail
    mov es, [sel_a]
    mov byte [es:0x100], 0x5A
    mov ax, [ds_sel]
    mov es, ax

    ; The selector for a real-mode segment.
    check 4
    mov ax, 0x0002
    mov bx, 0xB800
    int 0x31
    jc near pm_fail
    mov fs, ax
    cmp byte [fs:0x100], 0x5A
    jne near pm_fail

    ; Extended memory, reached through the descriptor.
    check 5
    mov ax, 0x0501
    mov bx, 0x0002
    xor cx, cx
    int 0x31
    jc near pm_fail
    mov [mem_si], si
    mov [mem_di], di
    mov dx, cx
    mov cx, bx
    mov bx, [sel_a]
    mov ax, 0x0007
    int 0x31
    jc near pm_fail
    mov es, bx
    mov word [es:0xFFFE], 0xBEEF
    mov ax, [ds_sel]
    mov es, ax
    mov es, bx
    cmp word [es:0xFFFE], 0xBEEF
    jne near pm_fail
    mov ax, [ds_sel]
    mov es, ax
    mov si, [mem_si]
    mov di, [mem_di]
    mov ax, 0x0502
    int 0x31
    jc near pm_fail

    ; A software interrupt to a handler here, returned from with IRET.
    check 6
    mov ax, 0x0204
    mov bl, 0x60
    int 0x31
    jc near pm_fail
    mov ax, 0x0205
    mov bl, 0x60
    mov cx, [cs_sel]
%ifdef CLIENT32
    mov edx, soft_handler
%else
    mov dx, soft_handler
%endif
    int 0x31
    jc near pm_fail
    mov word [count], 0
    int 0x60
    cmp word [count], 1
    jne near pm_fail

    ; A real-mode interrupt through the registers structure: DOS's version.
    check 7
    mov word [rm_ax], 0x3000
    mov word [rm_ss], 0
    mov word [rm_sp], 0
    mov word [rm_fl], 0x0202
    mov ax, ds
    mov es, ax
%ifdef CLIENT32
    mov edi, rm_regs
%else
    mov di, rm_regs
%endif
    mov ax, 0x0300
    mov bl, 0x21
    mov bh, 0
    xor cx, cx
    int 0x31
    jc near pm_fail
    cmp byte [rm_ax], 5
    jne near pm_fail

    ; An exception handler that skips the faulting instruction.
    check 8
    mov ax, 0x0203
    mov bl, 0x0D
    mov cx, [cs_sel]
%ifdef CLIENT32
    mov edx, gp_handler
%else
    mov dx, gp_handler
%endif
    int 0x31
    jc near pm_fail
    mov word [count], 0
    mov ax, 0xFFF8
faulting:
    mov es, ax
after_fault:
    cmp word [count], 1
    jne near pm_fail
    mov ax, [ds_sel]
    mov es, ax

    ; A hardware interrupt to a handler on the locked stack.
    check 9
    mov ax, 0x0204
    mov bl, 0x08
    int 0x31
    jc near pm_fail
    mov [default_8s], cx
    mov [default_8o], dx
    mov ax, 0x0205
    mov bl, 0x08
    mov cx, [cs_sel]
%ifdef CLIENT32
    mov edx, tick_handler
%else
    mov dx, tick_handler
%endif
    int 0x31
    jc near pm_fail
    mov word [count], 0
    sti
.wait:
    cmp word [count], 2
    jb .wait
    ; And the default back: the mode's real vector works again.
    mov ax, 0x0205
    mov bl, 0x08
    mov cx, [default_8s]
    mov dx, [default_8o]
    int 0x31

    ; A real-mode callback, called from real mode through 0301h.
    check 10
    push ds
    mov ax, cs
    mov ds, ax
    mov ax, ss
    mov es, ax
%ifdef CLIENT32
    mov esi, callback_proc
    mov edi, cb_regs
%else
    mov si, callback_proc
    mov di, cb_regs
%endif
    mov ax, ss
    mov es, ax
    mov ax, 0x0303
    int 0x31
    pop ds
    jc near pm_fail
    mov [rm_cs], cx
    mov [rm_ip], dx
    mov word [count], 0
    mov word [rm_ss], 0
    mov word [rm_sp], 0
    mov ax, ds
    mov es, ax
%ifdef CLIENT32
    mov edi, rm_regs
%else
    mov di, rm_regs
%endif
    mov ax, 0x0301
    xor bx, bx
    xor cx, cx
    int 0x31
    jc near pm_fail
    cmp word [count], 1
    jne near pm_fail

    ; The raw switch to real mode and back.
    check 11
    mov ax, 0x0306
    int 0x31
    jc near pm_fail
    mov [raw_rm], cx
    mov [raw_rm + 2], bx
    mov [raw_pm_s], si
%ifdef CLIENT32
    mov [raw_pm_o], edi
%else
    mov word [raw_pm_o + 2], di
%endif
    mov ax, [rm_seg]
    mov cx, ax
    mov dx, ax
%ifdef CLIENT32
    mov ebx, 0xF000
    mov esi, 0
%else
    mov bx, 0xF000
%endif
    mov si, ax
%ifdef CLIENT32
    mov edi, raw_real
    jmp dword far [raw_pm_o]
%else
    mov di, raw_real
    jmp far [raw_pm_o + 2]
%endif
raw_back:
    cmp ax, 0x1234
    jne near pm_fail

    ; A control register read, which a host at level 3 has to emulate.
    check 12
    mov eax, cr0
    test al, 1
    jz near pm_fail

    ; A DOS call with a selector and a name in it.
    check 13
    mov ah, 0x09
%ifdef CLIENT32
    mov edx, message
%else
    mov dx, message
%endif
    int 0x21

    ; The end.
    mov ax, 0x4C2A
    int 0x21

pm_fail:
    mov al, [stage]
    mov ah, 0x4C
    int 0x21

fail:
    mov ax, 0x4CFE
    int 0x21

soft_handler:
    push ds
    mov ds, [cs:ds_sel_cs]
    inc word [count]
    pop ds
    IRET_

; DS is lost on the handler's entry, so it uses the copy at this label.
ds_sel_cs: dw 0

gp_handler:
    push bp
    mov bp, sp
    push ds
    mov ds, [cs:ds_sel_cs]
    inc word [count]
    ; The return address on the frame: past the error code goes the saved EIP.
    %ifdef CLIENT32
    mov dword [ss:bp + 2 + 3 * 4], after_fault
    %else
    mov word [ss:bp + 2 + 3 * 2], after_fault
    %endif
    pop ds
    pop bp
    RETF_

tick_handler:
    push ax
    push ds
    mov ds, [cs:ds_sel_cs]
    inc word [count]
    mov al, 0x20
    out 0x20, al
    pop ds
    pop ax
    IRET_

; DS:SI is the real-mode stack, ES:DI the registers. It takes the return address
; off that stack into the structure, as the far return to it.
callback_proc:
    push ax
    %ifdef CLIENT32
    mov ax, [si]
    mov [es:di + 0x2A], ax
    mov ax, [si + 2]
    mov [es:di + 0x2C], ax
    add word [es:di + 0x2E], 4
    %else
    mov ax, [si]
    mov [es:di + 0x2A], ax
    mov ax, [si + 2]
    mov [es:di + 0x2C], ax
    add word [es:di + 0x2E], 4
    %endif
    push ds
    mov ds, [cs:ds_sel_cs]
    inc word [count]
    pop ds
    pop ax
    IRET_

raw_real:
    ; Real mode: the stack's at the top of our segment.
    mov ax, [ds_sel]
    mov cx, ax
    mov dx, [ss_sel]
    mov bx, 0xF000
    mov si, [cs_sel]
    mov di, raw_pm_entry
    jmp far [raw_rm]
raw_pm_entry:
    mov ax, 0x1234
    jmp raw_back

message: db 'OK$'

entry: dd 0
rm_seg: dw 0
ds_sel: dw 0
cs_sel: dw 0
ss_sel: dw 0
sel_a: dw 0
stage: db 0
    db 0
count: dw 0
mem_si: dw 0
mem_di: dw 0
default_8s: dw 0
default_8o: dw 0
raw_rm: dd 0
raw_pm_o: dd 0
raw_pm_s: dw 0
rm_regs:
    times 0x1C db 0
rm_ax: dw 0
    dw 0
rm_fl: dw 0
    times 8 db 0
rm_ip: dw 0
rm_cs: dw 0
rm_sp: dw 0
rm_ss: dw 0
cb_regs:
    times 0x32 db 0
