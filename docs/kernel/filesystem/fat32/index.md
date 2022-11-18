# FAT32文件系统

## 简介

&emsp;&emsp;FAT32文件系统是一种相对简单的文件系统。

&emsp;&emsp;FAT32文件系统实现在`kernel/filesystem/fat32/`中。

---

## 相关数据结构

### struct fat32_BootSector_t

&emsp;&emsp;fat32启动扇区结构体

```c
struct fat32_BootSector_t
{
    uint8_t BS_jmpBoot[3];    // 跳转指令
    uint8_t BS_OEMName[8];    // 生产厂商名
    uint16_t BPB_BytesPerSec; // 每扇区字节数
    uint8_t BPB_SecPerClus;   // 每簇扇区数
    uint16_t BPB_RsvdSecCnt;  // 保留扇区数
    uint8_t BPB_NumFATs;      // FAT表数量
    uint16_t BPB_RootEntCnt;  // 根目录文件数最大值
    uint16_t BPB_TotSec16;    // 16位扇区总数
    uint8_t BPB_Media;        // 介质描述符
    uint16_t BPB_FATSz16;     // FAT12/16每FAT扇区数
    uint16_t BPB_SecPerTrk;   // 每磁道扇区数
    uint16_t BPB_NumHeads;    // 磁头数
    uint32_t BPB_HiddSec;     // 隐藏扇区数
    uint32_t BPB_TotSec32;    // 32位扇区总数

    uint32_t BPB_FATSz32;   // FAT32每FAT扇区数
    uint16_t BPB_ExtFlags;  // 扩展标志
    uint16_t BPB_FSVer;     // 文件系统版本号
    uint32_t BPB_RootClus;  // 根目录起始簇号
    uint16_t BPB_FSInfo;    // FS info结构体的扇区号
    uint16_t BPB_BkBootSec; // 引导扇区的备份扇区号
    uint8_t BPB_Reserved0[12];

    uint8_t BS_DrvNum; // int0x13的驱动器号
    uint8_t BS_Reserved1;
    uint8_t BS_BootSig;       // 扩展引导标记
    uint32_t BS_VolID;        // 卷序列号
    uint8_t BS_VolLab[11];    // 卷标
    uint8_t BS_FilSysType[8]; // 文件系统类型

    uint8_t BootCode[420]; // 引导代码、数据

    uint16_t BS_TrailSig; // 结束标志0xAA55
} __attribute__((packed));
```

### struct fat32_FSInfo_t

&emsp; &emsp;该扇区存储了FAT32文件系统的一些参考信息。

```c
struct fat32_FSInfo_t
{
    uint32_t FSI_LeadSig;        
    uint8_t FSI_Reserved1[480]; 
    uint32_t FSI_StrucSig;      
    uint32_t FSI_Free_Count;
    uint32_t FSI_Nxt_Free;     
    uint8_t FSI_Reserved2[12];  
    uint32_t FSI_TrailSig; 
} __attribute__((packed));
```

**FSI_LeadSig**

&emsp;&emsp;FS info扇区标志符 数值为0x41615252

**FSI_Reserved1**

&emsp;&emsp;保留使用，全部置为0

**FSI_StrucSig**

&emsp;&emsp;FS_Info扇区的另一个标志符，数值为0x61417272

**FSI_Free_Count**

&emsp;&emsp;上一次记录的空闲簇数量，这是一个参考值

**FSI_Nxt_Free**

&emsp;&emsp;空闲簇的起始搜索位置，这是为驱动程序提供的参考值.

**FSI_Reserved2**
&emsp;&emsp;保留使用，全部置为0

**FSI_TrailSig**

&emsp;&emsp;FS_Info扇区结束标志，数值为0xaa550000

### struct fat32_Directory_t

&emsp;&emsp;短目录项结构体。

```c
struct fat32_Directory_t
{
    unsigned char DIR_Name[11];
    unsigned char DIR_Attr;         
    unsigned char DIR_NTRes;     
    unsigned char DIR_CrtTimeTenth;
    unsigned short DIR_CrtTime;    
    unsigned short DIR_CrtDate;
    unsigned short DIR_LastAccDate; 
    unsigned short DIR_FstClusHI;  
    unsigned short DIR_WrtTime;     
    unsigned short DIR_WrtDate;     
    unsigned short DIR_FstClusLO;   
    unsigned int DIR_FileSize;      
} __attribute__((packed));
```

**DIR_Name**

&emsp;&emsp;目录项名称。前8bytes为基础名，后3bytes为扩展名

**DIRAttr**

&emsp;&emsp;目录项属性。可选值有如下：

> - ATTR_READ_ONLY
> 
> - ATTR_HIDDEN
> 
> - ATTR_SYSTEM
> 
> - ATTR_VOLUME_ID
> 
> - ATTR_DIRECTORY
> 
> - ATTR_ARCHIVE
> 
> - ATTR_LONG_NAME

**DIR_NTRes**

&emsp;&emsp;该项为Windows下特有的表示区域，通过该项的值，表示基础名和扩展名的大小写情况。该项的值为`EXT|BASE`组合而成，其中，具有以下定义：

> BASE:LowerCase(8),UpperCase(0)
> EXT:LowerCase(16),UpperCase(0)

**DIR_CrtTimeTenth**

&emsp;&emsp;文件创建的毫秒级时间戳

**DIR_CrtTime**

&emsp;&emsp;文件创建时间

**DIR_CrtDate**

  文件创建日期

**DIR_LastAccDate**

&emsp;&emsp;文件的最后访问日期

**DIR_FstClusHI**

&emsp;&emsp; 文件起始簇号（高16bit）

**DIR_WrtTime**

&emsp;&emsp;最后写入时间

**DIR_WrtDate**

&emsp;&emsp;最后写入日期

**DIR_FstClusLO**

&emsp;&emsp; 文件起始簇号（低16bit）

**DIR_FileSize**
&emsp;&emsp;文件大小

### struct fat32_partition_info_t

&emsp;&emsp;该数据结构为FAT32分区的信息结构体，并不实际存在于物理磁盘上。这个结构体在挂载文件系统时被创建，作为文件系统的超级块的私有信息的一部分。

### struct fat32_inode_info_t

&emsp;&emsp;该结构体是VFS的inode结构体的私有信息部分的具体实现。

---

## 已知问题

1. 对目录项名称的检查没有按照标准严格实现

2. 当磁盘可用簇数量发生改变时，未更新FS_Info扇区

3. 未填写目录项的时间字段

---

## TODO

- 完全实现VFS定义的文件接口

- 性能优化

---

## 参考资料

[FAT32 File System Specification - from Microsoft](http://download.microsoft.com/download/1/6/1/161ba512-40e2-4cc9-843a-923143f3456c/fatgen103.doc)