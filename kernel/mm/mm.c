#include "mm.h"
#include "../common/printk.h"

ul Total_Memory = 0;
void mm_init()
{
    // 实模式下获取到的信息的起始地址，转换为ARDS指针
    struct ARDS *ards_ptr = (struct ARDS *)0xffff800000007e00;

    for (int i = 0; i < 32; ++i)
    {
        printk("Addr = %#10lx,%8lx\tLength = %#10lx,%8lx\tType = %#10lx\n",
               ards_ptr->BaseAddrH, ards_ptr->BaseAddrL, ards_ptr->LengthH, ards_ptr->LengthL, ards_ptr->type);

        //可用的内存
        if (ards_ptr->type == 1)
        {
            Total_Memory += ards_ptr->LengthL;
            Total_Memory += ((ul)(ards_ptr->LengthH)) << 32;
        }

        ++ards_ptr;

        // 脏数据
        if (ards_ptr->type > 4)
            break;
    }
    printk_color(ORANGE, BLACK, "Total amount of RAM DragonOS can use: %ld bytes\n", Total_Memory);
}