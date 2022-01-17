; |==================|
; |    这是loader程序  |
; |==================|
; Created by longjin, 2022/01/17

; 由于实模式下，物理地址为CS<<4+IP，而从boot的定义中直到，loader的CS为0x1000， 因此loader首地址为0x10000
org 0x10000
    mov ax, cs
    mov ds, ax ; 初始化数据段寄存器
    mov es, ax ; 初始化附加段寄存器
    mov ax, 0x00
    mov ss, ax ;初始化堆栈段寄存器
    mov sp, 0x7c00

    ;在屏幕上显示 start Loader
    mov ax, 0x1301
    mov bx, 0x000f
    mov dx, 0x0100  ;在第2行显示
    mov cx, 23 ;设置消息长度
    push ax

    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_Start_Loader
    int 0x10

    jmp $


; 要显示的消息文本
Message_Start_Loader: db "[DragonOS] Start Loader"
len_Message_Start_Loader: db 23
