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

; ==== 临时的全局描述符表 =====
[SECTION gdt]

LABEL_GDT:		dd	0,0
LABEL_DESC_CODE32:	dd	0x0000FFFF,0x00CF9A00 ; 代码段和数据段的段基地址都设置在0x00000000处， 把段限长设置为0xffffffff，可以索引32位地址空间
LABEL_DESC_DATA32:	dd	0x0000FFFF,0x00CF9200

GdtLen	equ	$ - LABEL_GDT
; GDTR寄存器是一个6B的结构，低2B保存GDT的长度， 高4B保存GDT的基地址
GdtPtr	dw	GdtLen - 1
	dd	LABEL_GDT

; 这是两个段选择子，是段描述符在GDT表中的索引号
SelectorCode32	equ	LABEL_DESC_CODE32 - LABEL_GDT
SelectorData32	equ	LABEL_DESC_DATA32 - LABEL_GDT

; === IA-32e模式的临时gdt表
[SECTION gdt64]
LABEL_GDT64:        dq 0x0000000000000000
LABEL_DESC_CODE64:  dq 0x0020980000000000
LABEL_DESC_DATA64:  dq 0x0000920000000000

GdtLen64    equ $ - LABEL_GDT64
GdtPtr64 dw GdtLen64-1,
            dd LABEL_GDT64

SelectorCode64 equ LABEL_DESC_CODE64 - LABEL_GDT64
SelectorData64 equ LABEL_DESC_DATA64 - LABEL_GDT64




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
    mov cx, 38
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
    mov cx, 34
    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_Start_Get_SVGA_VBE_Info
    int 0x10

    ; 使用INT0x10的主功能号0x4F00获取SVGA VBE信息
    ; For more information, please visit: https://longjin666.top/?p=1321
    mov ax, 0x00
    mov es, ax
    mov di, 0x8000
    mov ax, 0x4F00
    int 0x10

    cmp ax, 0x004F ; 获取成功
    jz Label_Get_SVGA_VBE_Success

Label_Get_SVGA_VBE_Failed:
    ; 获取SVGA VBE信息失败
    mov ax, 0x1301
    mov bx, 0x008c
    mov dx, 0x0600 ; 在第7行显示
    mov cx, 33
    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_Get_SVGA_VBE_Failed
    int 0x10
    jmp $

Label_Get_SVGA_VBE_Success:
    mov ax, 0x1301
    mov bx, 0x000f
    mov dx, 0x0600 ; 在第7行显示
    mov cx, 38
    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_Get_SVGA_VBE_Success
    int 0x10

Label_Get_SVGA_Mode_Info:
    ; ====== 获取SVGA mode信息 ======
    mov ax, 0x1301
    mov bx, 0x000f
    mov dx, 0x0700 ; 在第8行显示
    mov cx, 35
    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_Start_Get_SVGA_Mode_Info
    int 0x10

    mov ax, 0x00
    mov es, ax
    mov si, 0x800e ; 根据文档可知，偏移量0Eh处，	DWORD	pointer to list of supported VESA and OEM video modes
                    		;(list of words terminated with FFFFh)
    mov esi, dword [es:si]
    mov edi, 0x8200

Label_SVGA_Mode_Info_Get:
    mov cx, word [es:esi]


; ===========显示SVGA mode的信息
    ;push	ax

	;mov	ax,	0x00
	;mov	al,	ch
	;call	Label_DispAL

	;mov	ax,	0x00
	;mov	al,	cl
	;call	Label_DispAL

	;pop	ax
;============

    ;  判断是否获取完毕
    cmp cx, 0xFFFF
    jz Label_SVGA_Mode_Info_Finish

    mov ax, 0x4f01 ; 使用4f01功能，获取SVGA的模式
    int 0x10

    cmp ax, 0x004f ; 判断是否获取成功
    jnz Label_SVGA_Mode_Info_Fail

    add esi, 2
    add edi, 0x100 ; 开辟一个 256-byte 的 buffer

    jmp Label_SVGA_Mode_Info_Get

Label_SVGA_Mode_Info_Fail:
    ; === 获取信息失败 ===
    mov ax, 0x1301
    mov bx, 0x008c
    mov dx, 0x0800 ; 在第9行显示
    mov cx, 34
    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_Get_SVGA_Mode_Failed
    int 0x10

    jmp $

Label_SVGA_Mode_Info_Finish:
    ; === 成功获取SVGA mode信息 ===
    mov ax, 0x1301
    mov bx, 0x000f
    mov dx, 0x0800 ; 在第9行显示
    mov cx, 39
    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_Get_SVGA_Mode_Success
    int 0x10
    jmp Label_Set_SVGA_Mode

Label_SET_SVGA_Mode_VESA_VBE_FAIL:
    ; 设置SVGA显示模式失败
    mov ax, 0x1301
    mov bx, 0x008c
    mov dx, 0x0800 ; 在第10行显示
    mov cx, 29
    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, Message_Set_SVGA_Mode_Failed
    int 0x10

    jmp $


Label_Set_SVGA_Mode:

; ===== 设置SVGA芯片的显示模式(VESA VBE) ===
    mov ax, 0x4f02 ; 使用int0x10 功能号AX=4f02设置SVGA芯片的显示模式
    mov bx, 0x4180 ; 显示模式可以选择0x180(1440*900 32bit)或者0x143(800*600 32bit)
    int 0x10

    cmp ax, 0x004F
    jnz Label_SET_SVGA_Mode_VESA_VBE_FAIL

; ===== 初始化GDT表，切换到保护模式 =====

    cli ; 关闭外部中断
    db 0x66
    lgdt [GdtPtr]

    db 0x66
    lidt [IDT_POINTER]

    mov eax, cr0
    or eax, 1 ; 启用保护模式
    mov cr0, eax

    ; 跳转到保护模式下的第一个程序
    jmp dword SelectorCode32:GO_TO_TMP_Protect


[SECTION .s32]
[BITS 32]
GO_TO_TMP_Protect:
    ; ==== 切换到长模式 =====
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov ss, ax
    mov esp, 0x7e00 ; 将栈指针设置在实模式获取到的数据的基地址上

    call support_long_mode ; 检测是否支持长模式

    test eax, eax ; 将eax自身相与，检测是否为0（test指令不会把结果赋值回去eax）
    jz no_support ; 不支持长模式

    ; 初始化临时页表， 基地址设置为0x90000
    ; 设置各级页表项的值（页表起始地址与页属性组成）
    mov	dword	[0x90000],	0x91007
	mov	dword	[0x90004],	0x00000
	mov	dword	[0x90800],	0x91007
	mov	dword	[0x90804],	0x00000

	mov	dword	[0x91000],	0x92007
	mov	dword	[0x91004],	0x00000

	mov	dword	[0x92000],	0x000083
	mov	dword	[0x92004],	0x000000

	mov	dword	[0x92008],	0x200083
	mov	dword	[0x9200c],	0x000000

	mov	dword	[0x92010],	0x400083
	mov	dword	[0x92014],	0x000000

	mov	dword	[0x92018],	0x600083
	mov	dword	[0x9201c],	0x000000

	mov	dword	[0x92020],	0x800083
	mov	dword	[0x92024],	0x000000

	mov	dword	[0x92028],	0xa00083
	mov	dword	[0x9202c],	0x000000

	; === 加载GDT ===
	db 0x66
	lgdt [GdtPtr64] ; 加载GDT
    ; 把临时gdt的数据段加载到寄存器中(cs除外)
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov fs, ax
    mov gs, ax
    mov ss, ax

    mov esp, 0x7e00

    ; ====== 开启物理地址扩展 =====
    ; 通过bts指令，将cr4第5位置位，开启PAE
    mov eax, cr4
    bts eax, 5
    mov cr4, eax

    ; 将临时页目录的地址设置到CR3控制寄存器中
    mov eax, 0x90000
    mov cr3, eax

    ; ==== 启用长模式 ===
    ; 参见英特尔开发手册合集p4360 volume4, chapter2  页码2-60 Vol. 4
    ; IA32_EFER寄存器的第8位是LME标志位，能启用IA-32e模式
    mov ecx, 0xC0000080
    rdmsr
    bts eax, 8
    wrmsr

    ; === 开启分页机制 ===
    mov eax, cr0
    bts eax, 0 ; 再次开启保护模式
    bts eax, 31 ; 开启分页管理机制
    mov cr0, eax


    ; === 通过此条远跳转指令，处理器跳转到内核文件进行执行，正式进入IA-32e模式

    jmp SelectorCode64:Offset_Of_Kernel_File


support_long_mode:
    ; ===== 检测是否支持长模式 ====
    mov eax, 0x80000000
    cpuid ; cpuid指令返回的信息取决于eax的值。当前返回到eax中的是最大的输入参数值。 详见：英特尔开发人员手册卷2A Chapter3 (Page 304)
    cmp eax, 0x80000001
    setnb al ; 当cmp结果为不低于时，置位al
    jb support_long_mode_done ; 当eax小于0x80000001时，跳转

    mov eax, 0x80000001
    cpuid ; 获取特定信息，参照开发人员手册卷2A p304

    bt edx, 29 ; 将edx第29位的值移到CF上。该位指示了CPU是否支持IA-32e模式
                ; Bit 29: Intel® 64 Architecture available if 1.
    setc al ; 若支持则al置位

support_long_mode_done:
     movzx eax, al ; 将al，零扩展为32位赋值给eax
     ret

no_support:
    ; 不支持长模式
    jmp $



[SECTION .s16lib]
[BITS 16]
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

; ==== 显示AL中的信息 ===
Label_DispAL:
    push ecx
    push edx
    push edi

    mov edi, [DisplayPosition]
    mov ah, 0x0F
    mov dl, al  ; 为了先显示al的高4位，因此先将al暂存在dl中，然后把al往右移动4位
    shr al, 4
    mov ecx, 2 ; 计数为2

.begin:
    and al, 0x0F
    cmp al, 9
    ja .1 ; 大于9，跳转到.1
    add al, '0'
    jmp .2
.1:
    sub al, 0x0a
    add al, 'A'
.2:
    ; 移动到显示内存中
    mov [gs:edi], ax
    add edi, 2

    mov al, dl
    loop .begin

    mov [DisplayPosition], edi

    pop edi
    pop edx
    pop ecx

    ret

; === 临时的中断描述符表 ===
; 为临时的IDT开辟空间。
; 由于模式切换过程中已经关闭了外部中断，只要确保模式切换过程中不产生异常，就不用完整的初始化IDT。甚至乎，只要没有异常产生，没有IDT也可以。
IDT:
    times 0x50 dq 0
IDT_END:

IDT_POINTER:
    dw IDT_END - IDT - 1
    dd IDT

;==== 临时变量 =====
RootDirSizeForLoop	dw	RootDirSectors
SectorNo		dw	0
Odd			db	0
OffsetOfKernelFileCount	dd	Offset_Of_Kernel_File

DisplayPosition dd 0

; 要显示的消息文本
Message_Start_Loader: db "[DragonOS] Start Loader"
Message_No_Loader: db "[ERROR] No Kernel Found."
Message_Kernel_Loaded: db "[INFO] Kernel loaded"
Message_Start_Get_Mem_Struct: db "[INFO] Try to get memory struct..."
Message_Get_Mem_Failed: db "[ERROR] Get memory struct failed."
Message_Get_Mem_Success: db "[INFO] Successfully got memory struct."
Message_Start_Get_SVGA_VBE_Info: db "[INFO] Try to get SVGA VBE info..."
Message_Get_SVGA_VBE_Failed: db "[ERROR] Get SVGA VBE info failed."
Message_Get_SVGA_VBE_Success: db "[INFO] Successfully got SVGA VBE info."
Message_Start_Get_SVGA_Mode_Info: db "[INFO] Try to get SVGA mode info..."
Message_Get_SVGA_Mode_Failed: db "[ERROR] Get SVGA Mode info failed."
Message_Get_SVGA_Mode_Success: db "[INFO] Successfully got SVGA Mode info."
Message_Set_SVGA_Mode_Failed: db "[ERROR] Set SVGA Mode failed."

Kernel_FileName: db "KERNEL  BIN", 0