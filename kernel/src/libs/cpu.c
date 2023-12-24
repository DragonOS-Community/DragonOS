#include <common/cpu.h>
#include <common/kprint.h>
#include <common/printk.h>
// #pragma GCC optimize("O0")
// cpu支持的最大cpuid指令的基础主功能号
uint Cpu_cpuid_max_Basic_mop;
// cpu支持的最大cpuid指令的扩展主功能号
uint Cpu_cpuid_max_Extended_mop;
// cpu制造商信息
char Cpu_Manufacturer_Name[17] = {0};
// 处理器名称信息
char Cpu_BrandName[49] = {0};
// 处理器家族ID
uint Cpu_Family_ID;
// 处理器扩展家族ID
uint Cpu_Extended_Family_ID;
// 处理器模式ID
uint Cpu_Model_ID;
// 处理器扩展模式ID
uint Cpu_Extended_Model_ID;
// 处理器步进ID
uint Cpu_Stepping_ID;
// 处理器类型
uint Cpu_Processor_Type;
// 处理器支持的最大物理地址可寻址地址线宽度
uint Cpu_max_phys_addrline_size;
// 处理器支持的最大线性地址可寻址地址线宽度
uint Cpu_max_linear_addrline_size;
// 处理器的tsc频率（单位：hz）(HPET定时器在测定apic频率时，顺便测定了这个值)
uint64_t Cpu_tsc_freq = 0;

struct cpu_core_info_t cpu_core_info[MAX_CPU_NUM];

#if ARCH(I386) || ARCH(X86_64)

void cpu_init(void)
{
    // 获取处理器制造商信息
    uint tmp_info[4] = {0};
    cpu_cpuid(0, 0, &tmp_info[0], &tmp_info[1], &tmp_info[2], &tmp_info[3]);

    // 保存CPU支持的最大cpuid指令主功能号
    Cpu_cpuid_max_Basic_mop = tmp_info[0];
    // 保存制造商名称
    *(uint *)&Cpu_Manufacturer_Name[0] = tmp_info[1];
    *(uint *)&Cpu_Manufacturer_Name[4] = tmp_info[3];
    *(uint *)&Cpu_Manufacturer_Name[8] = tmp_info[2];
    Cpu_Manufacturer_Name[12] = '\0';
    kinfo("CPU manufacturer: %s", Cpu_Manufacturer_Name);

    // 获取处理器型号信息
    int count = 0;
    for (uint i = 0x80000002; i < 0x80000005; ++i)
    {
        cpu_cpuid(i, 0, &tmp_info[0], &tmp_info[1], &tmp_info[2], &tmp_info[3]);
        for (int j = 0; j <= 3; ++j)
        {
            *(uint *)&Cpu_BrandName[4 * count] = tmp_info[j];
            ++count;
        }
    }
    Cpu_BrandName[48] = '\0';

    kinfo("CPU Brand Name: %s", Cpu_BrandName);

    // 使用cpuid主功能号0x01进行查询(未保存ebx ecx edx的信息，具体参见白皮书)
    cpu_cpuid(1, 0, &tmp_info[0], &tmp_info[1], &tmp_info[2], &tmp_info[3]);

    // EAX中包含 Version Informatin Type,Family,Model,and Stepping ID
    Cpu_Stepping_ID = tmp_info[0] & 0xf;
    Cpu_Model_ID = (tmp_info[0] >> 4) & 0xf;
    Cpu_Family_ID = (tmp_info[0] >> 8) & 0xf;
    Cpu_Processor_Type = (tmp_info[0] >> 12) & 0x3;
    // 14-15位保留
    Cpu_Extended_Model_ID = (tmp_info[0] >> 16) & 0xf;
    Cpu_Extended_Family_ID = (tmp_info[0] >> 20) & 0xff;
    // 31-25位保留
    kinfo("Family ID=%#03lx\t Extended Family ID=%#03lx\t Processor Type=%#03lx\t", Cpu_Family_ID, Cpu_Extended_Family_ID, Cpu_Processor_Type);
    kinfo("Model ID=%#03lx\t Extended Model ID=%#03lx\tStepping ID=%#03lx\t", Cpu_Model_ID, Cpu_Extended_Model_ID, Cpu_Stepping_ID);

    // 使用0x80000008主功能号，查询处理器支持的最大可寻址地址线宽度
    cpu_cpuid(0x80000008, 0, &tmp_info[0], &tmp_info[1], &tmp_info[2], &tmp_info[3]);
    Cpu_max_phys_addrline_size = tmp_info[0] & 0xff;
    Cpu_max_linear_addrline_size = (tmp_info[0] >> 8) & 0xff;

    kinfo("Cpu_max_phys_addrline_size = %d", Cpu_max_phys_addrline_size);
    kinfo("Cpu_max_linear_addrline_size = %d", Cpu_max_linear_addrline_size);

    cpu_cpuid(0x80000000, 0, &tmp_info[0], &tmp_info[1], &tmp_info[2], &tmp_info[3]);
    Cpu_cpuid_max_Extended_mop = tmp_info[0];

    kinfo("Max basic mop=%#05lx", Cpu_cpuid_max_Basic_mop);
    kinfo("Max extended mop=%#05lx", Cpu_cpuid_max_Extended_mop);
    return;
}

void cpu_cpuid(uint32_t mop, uint32_t sop, uint32_t *eax, uint32_t *ebx, uint32_t *ecx, uint32_t *edx)
{
    // 向eax和ecx分别输入主功能号和子功能号
    // 结果输出到eax, ebx, ecx, edx
    __asm__ __volatile__("cpuid \n\t"
                         : "=a"(*eax), "=b"(*ebx), "=c"(*ecx), "=d"(*edx)
                         : "0"(mop), "2"(sop)
                         : "memory");
}

#else
void cpu_init(void){}
#endif