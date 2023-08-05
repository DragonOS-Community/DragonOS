/**
 * @file main.c
 * @author xiaoyez (xiaoyez@zju.edu.cn)
 * @brief 测试kvm的程序
 * @version 0.1
 * @date 2023-07-13
 *
 * @copyright Copyright (c) 2023
 *
 */

/**
 * 测试kvm命令的方法:
 * 1.在DragonOS的控制台输入 exec bin/test_kvm.elf
 *
 */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>

int main()
{
    printf("Test kvm running...\n");
    printf("Open /dev/kvm\n");
    int kvm_fd = open("/dev/kvm", O_RDWR|O_CLOEXEC);
    int vmfd = ioctl(kvm_fd, 0x01, 0);
    ioctl(vmfd, 0xdeadbeef, 0);
    printf("vmfd=%d\n", vmfd);
    return 0;
}