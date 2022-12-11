/**
 * @file main.c 
 * @author longjin (longjin@RinGoTek.cn)
 * @brief 测试signal用的程序
 * @version 0.1
 * @date 2022-12-06
 * 
 * @copyright Copyright (c) 2022
 * 
 */

/**
 * 测试signal的kill命令的方法:
 * 1.在DragonOS的控制台输入 exec bin/test_signal.elf &
 *      请注意,一定要输入末尾的 '&',否则进程不会后台运行
 * 2.然后kill对应的进程的pid (上一条命令执行后,将会输出这样一行:"[1] 生成的pid")
 * 
*/

#include <libc/src/math.h>
#include <libc/src/stdio.h>
#include <libc/src/stdlib.h>
#include <libc/src/time.h>
#include <libc/src/unistd.h>

int main()
{
    printf("Test signal running...\n");
    clock_t last = clock();
    while (1)
    {
        if ((clock()-last)/CLOCKS_PER_SEC >= 1){
            // printf("Test signal running\n");
            last = clock();
        }
    }
    return 0;
}