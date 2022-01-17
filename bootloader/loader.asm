; |==================|
; |    这是loader程序  |
; |==================|
; Created by longjin, 2022/01/17

; 由于实模式下，物理地址为CS<<4+IP，而从boot的定义中直到，loader的CS为0x1000， 因此loader首地址为0x10000
org 0x10000
    jmp Label_Start

%include 'fat12.inc'    ; 将fat12文件系统的信息包含进来

Base_Of_Kernel_File equ 0x00
Offset_Of_Kernel_File equ 0x100000 ; 设置内核文件的地址空间从1MB处开始。（大于实模式的寻址空间）

Base_Tmp_Of_Kernel_Addr equ 0x00
Offset_Tmp_Of_Kernel_File equ 0x7e00    ; 内核程序的临时转存空间

Memory_Struct_Buffer_Addr equ 0x7e00    ; 内核被转移到最终的内存空间后，原来的临时空间就作为内存结构数据的存储空间


[SECTION gdt]

LABEL_GDT:		dd	0,0
LABEL_DESC_CODE32:	dd	0x0000FFFF,0x00CF9A00
LABEL_DESC_DATA32:	dd	0x0000FFFF,0x00CF9200

GdtLen	equ	$ - LABEL_GDT
GdtPtr	dw	GdtLen - 1
	dd	LABEL_GDT

SelectorCode32	equ	LABEL_DESC_CODE32 - LABEL_GDT
SelectorData32	equ	LABEL_DESC_DATA32 - LABEL_GDT


[SECTION .s16]  ;定义一个名为.s16的段
[BITS 16]   ; 通知nasm，将要运行在16位宽的处理器上

Label_Start:
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
    ;jmp $

    ; 使用A20快速门来开启A20信号线
    push ax
    in al, 0x92 ; A20快速门使用I/O端口0x92来处理A20信号线
    or al, 0x02 ; 通过将0x92端口的第1位置1，开启A20地址线
    out 0x92, al
    pop ax

    cli ; 关闭外部中断

    db 0x66
    lgdt [GdtPtr]   ; LGDT/LIDT - 加载全局/中断描述符表格寄存器

    ; 置位CR0寄存器的第0位，开启保护模式
    mov eax, cr0
    or eax, 1
    mov cr0, eax

    ; 为fs寄存器加载新的数据段的值
    mov ax, SelectorData32
    mov fs, ax

    ; fs寄存器加载完成后，立即从保护模式退出。 这样能使得fs寄存器在实模式下获得大于1MB的寻址能力。
    mov eax, cr0
    and al, 11111110b ; 将第0位置0
    mov cr0, eax

    sti ; 开启外部中断




; =========在文件系统中搜索 kernel.bin==========
    mov word [SectorNo], SectorNumOfRootDirStart    ;保存根目录起始扇区号

Label_Search_In_Root_Dir_Begin:
    cmp word [RootDirSizeForLoop],  0 ; 比较根目录扇区数量和0的关系。 cmp实际上是进行了一个减法运算
    jz Label_No_KernelBin ; 等于0，不存在kernel.bin
    dec word [RootDirSizeForLoop]

    mov ax, 0x00
    mov es, ax
    mov bx, 0x8000
    mov ax, [SectorNo]  ;向函数传入扇区号
    mov cl, 1
    call Func_ReadOneSector
    mov si, Kernel_FileName ;向源变址寄存器传入Loader文件的名字
    mov di, 0x8000
    cld ;由于LODSB的加载方向与DF标志位有关，因此需要用CLD清零DF标志位

    mov dx, 0x10 ; 每个扇区的目录项的最大条数是(512/32=16,也就是0x10)

Label_Search_For_LoaderBin:
    cmp dx, 0
    jz Label_Goto_Next_Sector_In_Root_Dir
    dec dx
    mov cx, 11 ; cx寄存器存储目录项的文件名长度， 11B，包括了文件名和扩展名，但是不包括 分隔符'.'

Label_Cmp_FileName:
    cmp cx, 0
    jz Label_FileName_Found
    dec cx
    lodsb ; 把si对应的字节载入al寄存器中，然后，由于DF为0，si寄存器自增
    cmp al, byte [es:di]    ; 间接取址[es+di]。   也就是进行比较当前文件的名字对应字节和loader文件名对应字节
    jz  Label_Go_On     ; 对应字节相同
    jmp Label_Different     ; 字节不同，不是同一个文件

Label_Go_On:
    inc di
    jmp Label_Cmp_FileName

Label_Different:
    and di, 0xffe0 ;将di恢复到当前目录项的第0字节
    add di, 0x20     ;将di跳转到下一目录项的第0字节
    mov si, Kernel_FileName
    jmp Label_Search_For_LoaderBin  ;继续搜索下一目录项

Label_Goto_Next_Sector_In_Root_Dir:
    add word [SectorNo], 1
    jmp Label_Search_In_Root_Dir_Begin

Label_No_KernelBin:
    ; 在屏幕上显示 [ERROR] No Kernel Found.
    mov ax, 0x1301
    mov bx, 0x000c  ; 红色闪烁高亮黑底
    mov dx, 0x0200  ; 显示在第3行（前面已经显示过2行了）
    mov cx, 24  ; 字符串长度
    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_No_Loader
    int 0x10

    jmp $

; ========= 找到了 kernel.bin ===========
; 将内核加载到内存中
Label_FileName_Found:
    mov ax, RootDirSectors

    ; 先取得目录项DIR_FstClus字段的值（起始簇号）
    and di, 0xffe0
    add di, 0x1a
    mov cx, word    [es:di]
    push cx


    add cx, ax
    add cx, SectorBalance
    mov eax, Base_Tmp_Of_Kernel_Addr    ; 内核放置的临时地址
    mov es, eax ;配置es和bx，指定kernel.bin在内存中的起始地址
    mov bx, Offset_Tmp_Of_Kernel_File
    mov ax, cx

Label_Go_On_Loading_File:
    push ax
    push bx

    ; 显示字符.
    mov ah, 0x0e
    mov al, "."
    mov bl, 0x0f
    int 0x10

    pop bx
    pop ax


    ; 读取一个扇区
    mov cl, 1
    call Func_ReadOneSector
    pop ax

    ; ======逐字节将内核程序复制到临时空间，然后转存到内核空间===
    push cx
    push eax
    push fs
    push edi
    push ds
    push esi

    mov cx, 0x0200 ; 指定计数寄存器的值为512， 为后面循环搬运这个扇区的数据做准备
    mov ax, Base_Of_Kernel_File
    mov fs, ax ; 这样在物理机上是行不通的，因为这样移动的话，fs就失去了32位寻址能力
    mov edi, dword  [OffsetOfKernelFileCount]   ; 指定目的变址寄存器

    mov ax, Base_Tmp_Of_Kernel_Addr
    mov ds, ax
    mov esi, Offset_Tmp_Of_Kernel_File  ; 指定来源变址寄存器



Label_Move_Kernel:
    ; 真正进行数据的移动
    mov al, byte [ds:esi]   ; 移动到临时区域
    mov byte [fs:edi], al   ; 再移动到目标区域

    inc esi
    inc edi
    loop Label_Move_Kernel

    ; 当前扇区数据移动完毕
    mov eax, 0x1000
    mov ds, eax

    mov dword [OffsetOfKernelFileCount],    edi ; 增加偏移量

    pop esi
    pop ds
    pop edi
    pop fs
    pop eax
    pop cx


    call Func_GetFATEntry

	cmp	ax,	0x0fff
	jz	Label_File_Loaded



	push ax
	mov	dx,	RootDirSectors
	add	ax,	dx
	add	ax,	SectorBalance



    ; 继续读取下一个簇
	jmp Label_Go_On_Loading_File

Label_File_Loaded:


    ;在屏幕上显示 kernel loaded
    mov ax, 0x1301
    mov bx, 0x000f
    mov dx, 0x0200  ;在第3行显示
    mov cx, 20 ;设置消息长度

    push ax

    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_Kernel_Loaded
    int 0x10

     ; ======直接操作显示内存=======
    ; 从内存的0x0B800开始，是一段用于显示字符的内存空间。
    ; 每个字符占用2bytes，低字节保存要显示的字符，高字节保存样式
    mov ax, 0xB800
    mov gs, ax
    mov ah, 0x0F ;黑底白字
    mov al, '.'
    mov [gs:((80 * 2 + 20) * 2)], ax ;在屏幕第0行，39列





Label_Kill_Motor:
    ; =====关闭软驱的马达======
    ; 向IO端口0x03f2写入0，关闭所有软驱
    push dx
    mov dx, 0x03F2
    mov al, 0
    out dx, al
    pop dx

    ; =====获取物理地址空间====

    ; 显示 正在获取内存结构
    mov ax, 0x1301
    mov bx, 0x000F
    mov dx, 0x0300 ; 在第四行显示
    mov cx, 34
    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_Start_Get_Mem_Struct
    int 0x10


    mov ax, 0x00
    mov es, ax

    mov di, Memory_Struct_Buffer_Addr ; 设置内存结构信息存储的地址
    mov ebx, 0 ;第一次调用0x15的时候，ebx要置为0 ebx存储的是下一个待返回的ARDS  （Address Range Descriptor Structure）

Label_Get_Mem_Struct:
    ;==== 获取内存物理地址信息
    ; 使用0x15中断程序的功能号0xe820来获取内存信息
    ; 返回信息在[es:di]指向的内存中
    ; 一共要分5次才能把20个字节的信息获取完成
    ; 这些信息在内核初始化内存管理单元的时候，会去解析它们。

    mov eax, 0xe820
    mov ecx, 20 ; 指定ARDS结构的大小，是固定值20
    mov edx, 0x534d4150 ; 固定签名标记，是字符串“SMAP”的ASCII码
    int 0x15

    jc Label_Get_Mem_Fail ; 若调用出错，则CF=1

    add di, 20
    cmp ebx, 0
    jne Label_Get_Mem_Struct ; ebx不为0
    jmp Label_Get_Mem_OK ; 获取内存信息完成

Label_Get_Mem_Fail:
    ; =====获取内存信息失败====
    ; 显示 正在获取内存结构
    mov ax, 0x1301
    mov bx, 0x000c
    mov dx, 0x0400 ; 在第5行显示
    mov cx, 33
    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_Get_Mem_Failed
    int 0x10

    jmp $

Label_Get_Mem_OK:
    ; ==== 成功获取内存信息 ===
    mov ax, 0x1301
    mov bx, 0x000f
    mov dx, 0x0400 ; 在第5行显示
    mov cx, 39
    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_Get_Mem_Success
    int 0x10

    jmp Label_Get_SVGA_Info

Label_Get_SVGA_Info:
    ; ==== 获取SVGA芯片的信息
    mov ax, 0x1301
    mov bx, 0x000f
    mov dx, 0x0500 ; 在第6行显示
    mov cx, 30
    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_Start_Get_SVGA_Info
    int 0x10

    jmp $

; 从软盘读取一个扇区
; AX=待读取的磁盘起始扇区号
; CL=读入的扇区数量
; ES:BX=>目标缓冲区起始地址
Func_ReadOneSector:

    push bp
    mov bp,  sp
    sub esp, 2
    mov byte [bp-2], cl
    push bx
    mov bl, [BPB_SecPerTrk]
    div bl  ;用AX寄存器中的值除以BL，得到目标磁道号(商：AL)以及目标磁道内的起始扇区号(余数：AH)
    inc ah  ; 由于磁道内的起始扇区号从1开始计数，因此将余数+1
    mov cl, ah
    mov dh, al
    shr al, 1 ;计算出柱面号
    mov ch, al
    and dh, 1;计算出磁头号

    pop bx
    mov dl, [BS_DrvNum]
    ;最终，dh存储了磁头号，dl存储驱动器号
    ;   ch存储柱面号，cl存储起始扇区号

Label_Go_On_Reading:
    ; 使用BIOS中断服务程序INT13h的主功能号AH=02h实现软盘读取操作
    mov ah, 2
    mov al, byte [bp-2]
    int 0x13

    jc Label_Go_On_Reading  ;当CF标志位被复位时，说明数据读取完成，恢复调用现场

    add esp, 2
    pop bp

    ret


;   解析FAT表项,根据当前FAT表项索引出下一个FAT表项
Func_GetFATEntry:
    ; AX=FAT表项号（输入、输出参数）
    ; 保存将要被修改的寄存器


    push es
    push bx
    push ax

    ; 扩展段寄存器
    mov ax, 00
    mov es, ax

    pop ax
    mov byte [Odd], 0   ;将奇数标志位置0

    ; 将FAT表项号转换为总的字节号
    mov bx, 3
    mul bx
    mov bx, 2
    div bx

    cmp dx, 0
    jz Label_Even ; 偶数项
    mov byte [Odd], 1

Label_Even:
    xor dx, dx  ;把dx置0

    ; 计算得到扇区号（商）和扇区内偏移（余数）
    mov bx, [BPB_BytesPerSec]
    div bx
    push dx

    ; 读取两个扇区到[es:bx]
    mov bx, 0x8000
    add ax, SectorNumOfFAT1Start
    mov cl, 2 ; 设置读取两个扇区，解决FAT表项跨扇区的问题



    call Func_ReadOneSector

    pop dx
    add bx, dx
    mov ax, [es:bx]
    cmp byte [Odd], 1
    jnz Label_Even_2 ;若是偶数项，则跳转

    shr ax, 4 ; 解决奇偶项错位问题

Label_Even_2:
    and ax, 0x0fff ; 确保表项号在正确的范围内  0x0003~0x0fff
    pop bx
    pop es
    ret





;====临时变量=====
RootDirSizeForLoop	dw	RootDirSectors
SectorNo		dw	0
Odd			db	0
OffsetOfKernelFileCount	dd	Offset_Of_Kernel_File

; 要显示的消息文本
Message_Start_Loader: db "[DragonOS] Start Loader"
Message_No_Loader: db "[ERROR] No Kernel Found."
Message_Kernel_Loaded: db "[INFO] Kernel loaded"
Message_Start_Get_Mem_Struct: db "[INFO] Try to get memory struct..."
Message_Get_Mem_Failed: db "[ERROR] Get memory struct failed."
Message_Get_Mem_Success: db "[INFO] Successful to get memory struct."
Message_Start_Get_SVGA_Info: db "[INFO] Try to get SVGA info..."


Kernel_FileName: db "KERNEL  BIN", 0