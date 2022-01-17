;将程序开始位置设置为0x7c00处，并给BaseOfStack赋值为0x7c00
    org 0x7c00

BaseOfStack	equ	0x7c00
BaseOfLoader equ 0x1000
OffsetOfLoader equ 0x00

RootDirSectors equ 14   ;根目录占用的扇区数
SectorNumOfRootDirStart equ 19  ; 根目录的起始扇区号
SectorNumOfFAT1Start equ 1  ; FAT1表的起始扇区号 （因为前面有一个保留扇区（引导扇区））
SectorBalance equ 17    ;平衡文件/目录的起始簇号与数据区域的起始簇号的差值。


    jmp short Label_Start
    nop
    BS_OEMName  db  'DragonOS'
    BPB_BytesPerSec dw 512
    BPB_SecPerClus db 1
    BPB_RsvdSecCnt  dw  1
    BPB_NumFATs db 2
    BPB_RootEntCnt dw 224
    BPB_TotSec16 dw 2880
    BPB_Media db 0xf0
    BPB_FATSz16 dw 9
    BPB_SecPerTrk dw 18
    BPB_NumHeads dw 2
    BPB_HiddSec dd 0
    BPB_TotSec32 dd 0
    BS_DrvNum db 0
    BS_Reserved1 db 0
    BS_BootSig db 0x29
    BS_VolID dd 0
    BS_VolLab db 'boot loader'
    BS_FileSysType db 'FAT12   '



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
    int 0x10

    ;设置屏幕光标位置为左上角(0,0)的位置
    mov ax, 0x0200
    mov bx, 0x0000
    mov dx, 0x0000
    int 10h

    ;在屏幕上显示Start Booting
    mov ax, 0x1301 ;设置显示字符串，显示后，光标移到字符串末端
    mov bx, 0x000a ;设置黑色背景，白色字体，高亮度，不闪烁
    mov dx, 0x0000 ;设置游标行列号均为0
    mov cx, 24 ;设置字符串长度为24

    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, StartBootMessage
    int 0x10

    ;软盘驱动器复位
    xor ah, ah
    xor dl, dl
    int 0x13

; 在文件系统中搜索 loader.bin
    mov word [SectorNo], SectorNumOfRootDirStart    ;保存根目录起始扇区号

Label_Search_In_Root_Dir_Begin:
    cmp word [RootDirSizeForLoop],  0 ; 比较根目录扇区数量和0的关系。 cmp实际上是进行了一个减法运算
    jz Label_No_LoaderBin ; 等于0，不存在Loader.bin
    dec word [RootDirSizeForLoop]

    mov ax, 0x00
    mov es, ax
    mov bx, 0x8000
    mov ax, [SectorNo]  ;向函数传入扇区号
    mov cl, 1
    call Func_ReadOneSector
    mov si, LoaderFileName ;向源变址寄存器传入Loader文件的名字
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
    and di, 0ffe0h ;将di恢复到当前目录项的第0字节
    add di, 20h     ;将di跳转到下一目录项的第0字节
    mov si, LoaderFileName
    jmp Label_Search_For_LoaderBin  ;继续搜索下一目录项

Label_Goto_Next_Sector_In_Root_Dir:
    add word [SectorNo], 1
    jmp Label_Search_In_Root_Dir_Begin

Label_No_LoaderBin:
    ; 在屏幕上显示 [ERROR] No Loader Found.
    mov ax, 0x1301
    mov bx, 0x000c  ; 红色闪烁高亮黑底
    mov dx, 0x0100  ; 显示在第二行（前面已经显示过一行了）
    mov cx, 24  ; 字符串长度
    push ax
    mov ax, ds
    mov es, ax
    pop ax
    mov bp, NoLoaderMessage
    int 0x10
    jmp $


;========== 找到了Loader.Bin

Label_FileName_Found:
    mov ax, RootDirSectors

    ; 先取得目录项DIR_FstClus字段的值（起始簇号）
    and di, 0xffe0
    add di, 0x1a
    mov cx, word    [es:di]
    push cx


    add cx, ax
    add cx, SectorBalance
    mov ax, BaseOfLoader
    mov es, ax ;配置es和bx，指定loader.bin在内存中的起始地址
    mov bx, OffsetOfLoader
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


    ; 每读取一个扇区，就获取下一个表项，然后继续读入下一个簇的数据，直到返回的下一表项为0xfff为止，表示loader.bin完全加载完成
    mov cl, 1
    call Func_ReadOneSector
    pop ax
    call Func_GetFATEntry
    cmp ax, 0xfff
    jz Label_File_Loaded
    push ax
    mov dx, RootDirSectors
    add ax, dx
    add ax, SectorBalance
    add bx, [BPB_BytesPerSec]
    jmp Label_Go_On_Loading_File

Label_File_Loaded:
    ; 跳转到loader
    ; 这个指令结束后，目标段会复制到CS寄存器中
    jmp BaseOfLoader:OffsetOfLoader


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



; 临时变量
RootDirSizeForLoop dw RootDirSectors
SectorNo dw 0
Odd db 0


; 显示的文本
StartBootMessage:   db  "[DragonOS] Start Booting"
NoLoaderMessage: db "[ERROR] No LOADER Found."
LoaderFileName:		db	"LOADER  BIN",0 ;最后这个0是为了填满12字节的宽度

;填满整个扇区的512字节
    times 510 - ( $ - $$ ) db 0
    dw 0xaa55 ;===确保以0x55 0xaa为结尾



