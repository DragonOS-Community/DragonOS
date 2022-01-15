;将程序开始位置设置为0x7c00处，并给BaseOfStack赋值为0x7c00
    org 0x7c00

BaseOfStack	equ	0x7c00

Label_Start:
    ;初始化寄存器
    mov ax, cs
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, BaseOfStack

    ;清屏
    mov ax, 0x0600  ;AL=0时，清屏，BX、CX、DX不起作用
    mov bx, 0x0700  ;设置白色字体，不闪烁，字体正常亮度，黑色背景
    mov cx, 0
    mov dx, 0184fh
    int 10h

    ;设置屏幕光标位置为左上角(0,0)的位置
    mov ax, 0x0200
    mov bx, 0x0000
    mov dx, 0x0000
    int 10h

    ;在屏幕上显示Start Booting
    mov ax, 0x1301 ;设置显示字符串，显示后，光标移到字符串末端
    mov bx, 0x000f ;设置黑色背景，白色字体，高亮度，不闪烁
    mov dx, 0x0000 ;设置游标行列号均为0
    mov cx, 20 ;设置字符串长度为20

    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, StartBootMessage
    int 10h

    ;软盘驱动器复位
    xor ah, ah
    xor dl, dl
    int 13h

    jmp $

StartBootMessage:   db  "[DragonOS]Start Boot"

;填满整个扇区的512字节
    times 510 - ( $ - $$ ) db 0
    dw 0xaa55 ;===确保以0x55 0xaa为结尾





