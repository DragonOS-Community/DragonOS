#pragma once

#include <common/glib.h>

// ======== PIO端口定义 ========
#define PORT_DISK0_DATA 0x1f0                     // 数据
#define PORT_DISK0_ERR_STATUS 0x1f1               // 错误状态
#define PORT_DISK0_SECTOR_CNT 0x1f2               // 操作扇区数
#define PORT_DISK0_LBA_7_0 0x1f3                  // 扇区号 / LBA[7:0]
#define PORT_DISK0_LBA_15_8 0x1f4                 // 柱面号[7:0] / LBA[15:8]
#define PORT_DISK0_LBA_23_16 0x1f5                // 柱面号[15:8] / LBA[23:16]
#define PORT_DISK0_DEVICE_CONFIGURE_REG 0x1f6 // 设备配置寄存器
#define PORT_DISK0_CONTROLLER_STATUS_CMD 0x1f7    // 控制器状态端口 / 控制器命令端口
#define PORT_DISK0_STATUS_CTRL_REG 0x3f6          // 状态寄存器 / 控制寄存器

#define PORT_DISK1_DATA 0x170                  // 数据
#define PORT_DISK1_ERR_STATUS 0x171            // 错误状态
#define PORT_DISK1_SECTOR_CNT 0x172            // 操作扇区数
#define PORT_DISK1_LBA_7_0 0x173               // 扇区号 / LBA[7:0]
#define PORT_DISK1_LBA_15_8 0x174              // 柱面号[7:0] / LBA[15:8]
#define PORT_DISK1_LBA_23_16 0x175             // 柱面号[15:8] / LBA[23:16]
#define PORT_DISK1_DEVICE_CONFIGURE_REG 0x176  // 设备配置寄存器
#define PORT_DISK1_CONTROLLER_STATUS_CMD 0x177 // 控制器状态端口 / 控制器命令端口
#define PORT_DISK1_STATUS_CTRL_REG 0x376       // 状态寄存器 / 控制寄存器

// ======= 状态寄存器的状态位 ==========
#define DISK_STATUS_BUSY (1 << 7)       // 控制器忙
#define DISK_STATUS_READY (1 << 6)      // 驱动器准备就绪
#define DISK_STATUS_SEEK (1 << 4)       // 驱动器寻道
#define DISK_STATUS_DATA_REQ (1 << 3)   // 数据请求
#define DISK_STATUS_DATA_ERROR (1 << 0) // 命令执行错误

/**
 * @brief 执行0xec指令返回的512bytes的硬件设备识别信息
 * 位于ATA8-ACS中 Table-22
 */
struct ata_identify_device_data
{
    //	0	General configuration bit-significant information
    unsigned short General_Config;

    //	1	Obsolete
    unsigned short Obsolete0;

    //	2	Specific configuration
    unsigned short Specific_Coinfig;

    //	3	Obsolete
    unsigned short Obsolete1;

    //	4-5	Retired
    unsigned short Retired0[2];

    //	6	Obsolete
    unsigned short Obsolete2;

    //	7-8	Reserved for the CompactFlash Association
    unsigned short CompactFlash[2];

    //	9	Retired
    unsigned short Retired1;

    //	10-19	Serial number (20 ASCII characters)
    unsigned short Serial_Number[10];

    //	20-21	Retired
    unsigned short Retired2[2];

    //	22	Obsolete
    unsigned short Obsolete3;

    //	23-26	Firmware revision(8 ASCII characters)
    unsigned short Firmware_Version[4];

    //	27-46	Model number (40 ASCII characters)
    unsigned short Model_Number[20];

    //	47	15:8 	80h
    //		7:0  	00h=Reserved
    //			01h-FFh = Maximumnumber of logical sectors that shall be transferred per DRQ data block on READ/WRITE MULTIPLE commands
    unsigned short Max_logical_transferred_per_DRQ;

    //	48	Trusted Computing feature set options
    unsigned short Trusted_Computing_feature_set_options;

    //	49	Capabilities
    unsigned short Capabilities0;

    //	50	Capabilities
    unsigned short Capabilities1;

    //	51-52	Obsolete
    unsigned short Obsolete4[2];

    //	53	15:8	Free-fall Control Sensitivity
    //		7:3 	Reserved
    //		2 	the fields reported in word 88 are valid
    //		1 	the fields reported in words (70:64) are valid
    unsigned short Report_88_70to64_valid;

    //	54-58	Obsolete
    unsigned short Obsolete5[5];

    //	59	15:9	Reserved
    //		8	Multiple sector setting is valid
    //		7:0	xxh current setting for number of logical sectors that shall be transferred per DRQ data block on READ/WRITE Multiple commands
    unsigned short Mul_Sec_Setting_Valid;

    //	60-61	Total number of user addresssable logical sectors for 28bit CMD
    unsigned short Addressable_Logical_Sectors_for_28[2];

    //	62	Obsolete
    unsigned short Obsolete6;

    //	63	15:11	Reserved
    //		10:8=1 	Multiword DMA mode 210 is selected
    //		7:3 	Reserved
    //		2:0=1 	Multiword DMA mode 210 and below are supported
    unsigned short MultWord_DMA_Select;

    //	64	15:8	Reserved
    //		7:0	PIO mdoes supported
    unsigned short PIO_mode_supported;

    //	65	Minimum Multiword DMA transfer cycle time per word
    unsigned short Min_MulWord_DMA_cycle_time_per_word;

    //	66	Manufacturer`s recommended Multiword DMA transfer cycle time
    unsigned short Manufacture_Recommend_MulWord_DMA_cycle_time;

    //	67	Minimum PIO transfer cycle time without flow control
    unsigned short Min_PIO_cycle_time_Flow_Control;

    //	68	Minimum PIO transfer cycle time with IORDY flow control
    unsigned short Min_PIO_cycle_time_IOREDY_Flow_Control;

    //	69-70	Reserved
    unsigned short Reserved1[2];

    //	71-74	Reserved for the IDENTIFY PACKET DEVICE command
    unsigned short Reserved2[4];

    //	75	Queue depth
    unsigned short Queue_depth;

    //	76	Serial ATA Capabilities
    unsigned short SATA_Capabilities;

    //	77	Reserved for Serial ATA
    unsigned short Reserved3;

    //	78	Serial ATA features Supported
    unsigned short SATA_features_Supported;

    //	79	Serial ATA features enabled
    unsigned short SATA_features_enabled;

    //	80	Major Version number
    unsigned short Major_Version;

    //	81	Minor version number
    unsigned short Minor_Version;

    //	82	Commands and feature sets supported
    unsigned short Cmd_feature_sets_supported0;

    //	83	Commands and feature sets supported
    unsigned short Cmd_feature_sets_supported1;

    //	84	Commands and feature sets supported
    unsigned short Cmd_feature_sets_supported2;

    //	85	Commands and feature sets supported or enabled
    unsigned short Cmd_feature_sets_supported3;

    //	86	Commands and feature sets supported or enabled
    unsigned short Cmd_feature_sets_supported4;

    //	87	Commands and feature sets supported or enabled
    unsigned short Cmd_feature_sets_supported5;

    //	88	15 	Reserved
    //		14:8=1 	Ultra DMA mode 6543210 is selected
    //		7 	Reserved
    //		6:0=1 	Ultra DMA mode 6543210 and below are suported
    unsigned short Ultra_DMA_modes;

    //	89	Time required for Normal Erase mode SECURITY ERASE UNIT command
    unsigned short Time_required_Erase_CMD;

    //	90	Time required for an Enhanced Erase mode SECURITY ERASE UNIT command
    unsigned short Time_required_Enhanced_CMD;

    //	91	Current APM level value
    unsigned short Current_APM_level_Value;

    //	92	Master Password Identifier
    unsigned short Master_Password_Identifier;

    //	93	Hardware resset result.The contents of bits (12:0) of this word shall change only during the execution of a hardware reset.
    unsigned short HardWare_Reset_Result;

    //	94	Current AAM value
    //		15:8 	Vendor’s recommended AAM value
    //		7:0 	Current AAM value
    unsigned short Current_AAM_value;

    //	95	Stream Minimum Request Size
    unsigned short Stream_Min_Request_Size;

    //	96	Streaming Transger Time-DMA
    unsigned short Streaming_Transger_time_DMA;

    //	97	Streaming Access Latency-DMA and PIO
    unsigned short Streaming_Access_Latency_DMA_PIO;

    //	98-99	Streaming Performance Granularity (DWord)
    unsigned short Streaming_Performance_Granularity[2];

    //	100-103	Total Number of User Addressable Logical Sectors for 48-bit commands (QWord)
    unsigned short Total_user_LBA_for_48_Address_Feature_set[4];

    //	104	Streaming Transger Time-PIO
    unsigned short Streaming_Transfer_Time_PIO;

    //	105	Reserved
    unsigned short Reserved4;

    //	106	Physical Sector size/Logical Sector Size
    unsigned short Physical_Logical_Sector_Size;

    //	107	Inter-seek delay for ISO-7779 acoustic testing in microseconds
    unsigned short Inter_seek_delay;

    //	108-111	World wide name
    unsigned short World_wide_name[4];

    //	112-115	Reserved
    unsigned short Reserved5[4];

    //	116	Reserved for TLC
    unsigned short Reserved6;

    //	117-118	Logical sector size (DWord)
    unsigned short Words_per_Logical_Sector[2];

    //	119	Commands and feature sets supported (Continued from words 84:82)
    unsigned short CMD_feature_Supported;

    //	120	Commands and feature sets supported or enabled (Continued from words 87:85)
    unsigned short CMD_feature_Supported_enabled;

    //	121-126	Reserved for expanded supported and enabled settings
    unsigned short Reserved7[6];

    //	127	Obsolete
    unsigned short Obsolete7;

    //	128	Security status
    unsigned short Security_Status;

    //	129-159	Vendor specific
    unsigned short Vendor_Specific[31];

    //	160	CFA power mode
    unsigned short CFA_Power_mode;

    //	161-167	Reserved for the CompactFlash Association
    unsigned short Reserved8[7];

    //	168	Device Nominal Form Factor
    unsigned short Dev_from_Factor;

    //	169-175	Reserved
    unsigned short Reserved9[7];

    //	176-205	Current media serial number (ATA string)
    unsigned short Current_Media_Serial_Number[30];

    //	206	SCT Command Transport
    unsigned short SCT_Cmd_Transport;

    //	207-208	Reserved for CE-ATA
    unsigned short Reserved10[2];

    //	209	Alignment of logical blocks within a physical block
    unsigned short Alignment_Logical_blocks_within_a_physical_block;

    //	210-211	Write-Read-Verify Sector Count Mode 3 (DWord)
    unsigned short Write_Read_Verify_Sector_Count_Mode_3[2];

    //	212-213	Write-Read-Verify Sector Count Mode 2 (DWord)
    unsigned short Write_Read_Verify_Sector_Count_Mode_2[2];

    //	214	NV Cache Capabilities
    unsigned short NV_Cache_Capabilities;

    //	215-216	NV Cache Size in Logical Blocks (DWord)
    unsigned short NV_Cache_Size[2];

    //	217	Nominal media rotation rate
    unsigned short Nominal_media_rotation_rate;

    //	218	Reserved
    unsigned short Reserved11;

    //	219	NV Cache Options
    unsigned short NV_Cache_Options;

    //	220	Write-Read-Verify feature set current mode
    unsigned short Write_Read_Verify_feature_set_current_mode;

    //	221	Reserved
    unsigned short Reserved12;

    //	222	Transport major version number.
    //		0000h or ffffh = device does not report version
    unsigned short Transport_Major_Version_Number;

    //	223	Transport Minor version number
    unsigned short Transport_Minor_Version_Number;

    //	224-233	Reserved for CE-ATA
    unsigned short Reserved13[10];

    //	234	Minimum number of 512-byte data blocks per DOWNLOAD MICROCODE command for mode 03h
    unsigned short Mini_blocks_per_CMD;

    //	235	Maximum number of 512-byte data blocks per DOWNLOAD MICROCODE command for mode 03h
    unsigned short Max_blocks_per_CMD;

    //	236-254	Reserved
    unsigned short Reserved14[19];

    //	255	Integrity word
    //		15:8	Checksum
    //		7:0	Checksum Validity Indicator
    unsigned short Integrity_word;
} __attribute__((packed));

/**
 * @brief 初始化ATA磁盘驱动程序
 *
 */
void ata_init();