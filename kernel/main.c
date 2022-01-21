//
// Created by longjin on 2022/1/20.
//
int *address = (int *)0xffff800000a00000; //帧缓存区的地址

void show_color_band(int width, int height, char a, char b, char c, char d)
{
    /** 向帧缓冲区写入像素值
     * @param address: 帧缓存区的地址
     * @param val:像素值
     */

    for (int i = 0; i < width * height; ++i)
    {

        *((char *)address + 0) = d;
        *((char *)address + 1) = c;
        *((char *)address + 2) = b;
        *((char *)address + 3) = a;
        ++address;
    }
}

//操作系统内核从这里开始执行
void Start_Kernel(void)
{
    

    

    show_color_band(1440, 20, 0x00, 0xff, 0x00, 0x00);

    show_color_band(1440, 20, 0x00, 0x00, 0xff, 0x00);

    show_color_band(1440, 20, 0x00, 0x00, 0x00, 0xff);

    show_color_band(1440, 20, 0x00, 0xff, 0xff, 0xff);

    while (1)
        ;
}
