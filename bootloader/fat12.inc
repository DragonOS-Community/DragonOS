; === 这是FAT12文件系统的信息 ===
RootDirSectors equ 14   ;根目录占用的扇区数
SectorNumOfRootDirStart equ 19  ; 根目录的起始扇区号
SectorNumOfFAT1Start equ 1  ; FAT1表的起始扇区号 （因为前面有一个保留扇区（引导扇区））
SectorBalance equ 17    ;平衡文件/目录的起始簇号与数据区域的起始簇号的差值。

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