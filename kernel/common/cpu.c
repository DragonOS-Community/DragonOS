#include "cpu.h"
#include "kprint.h"
#include "printk.h"

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
    Cpu_Model_ID = (tmp_info[0]>>4) & 0xf;
    Cpu_Family_ID = (tmp_info[0]>>8) & 0xf;
    Cpu_Processor_Type = (tmp_info[0]>>12)& 0x3;
    // 14-15位保留
    Cpu_Extended_Model_ID = (tmp_info[0]>>16)&0xf;
    Cpu_Extended_Family_ID = (tmp_info[0]>>20)&0xff;
    //31-25位保留
    kinfo("Family ID=%#03lx\t Extended Family ID=%#03lx\t Processor Type=%#03lx\t",Cpu_Family_ID, Cpu_Extended_Family_ID, Cpu_Processor_Type);
    kinfo("Model ID=%#03lx\t Extended Model ID=%#03lx\tStepping ID=%#03lx\t",Cpu_Model_ID, Cpu_Extended_Model_ID,Cpu_Stepping_ID);

    // 使用0x80000008主功能号，查询处理器支持的最大可寻址地址线宽度
    cpu_cpuid(0x80000008, 0, &tmp_info[0], &tmp_info[1], &tmp_info[2], &tmp_info[3]);
    Cpu_max_phys_addrline_size = tmp_info[0]&0xff;
    Cpu_max_linear_addrline_size = (tmp_info[0]>>8)&0xff;

    kinfo("Cpu_max_phys_addrline_size = %d", Cpu_max_phys_addrline_size);
    kinfo("Cpu_max_linear_addrline_size = %d", Cpu_max_linear_addrline_size);
    
    cpu_cpuid(0x80000000, 0, &tmp_info[0], &tmp_info[1], &tmp_info[2], &tmp_info[3]);
    Cpu_cpuid_max_Extended_mop = tmp_info[0];

    kinfo("Max basic mop=%#05lx",Cpu_cpuid_max_Basic_mop);
    kinfo("Max extended mop=%#05lx",Cpu_cpuid_max_Extended_mop);
    return;
}