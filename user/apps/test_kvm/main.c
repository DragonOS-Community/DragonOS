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

#define KVM_SET_USER_MEMORY_REGION 0x01

struct kvm_userspace_memory_region {
    uint32_t slot;
    uint32_t flags;
    uint64_t guest_phys_addr;
    uint64_t memory_size;
    uint64_t userspace_addr;
};

// int guest_code(){
//     while (1)
//     {
//         printf("guest code\n");
//         __asm__ __volatile__ (
//             "mov %rax, 0\n\t"
//             "mov %rcx, 0\n\t"
//             "cpuid\n\t"
//         );
//     }
//     return 0;
// }

int main()
{
    printf("Test kvm running...\n");
    printf("Open /dev/kvm\n");
    int kvm_fd = open("/dev/kvm", O_RDWR|O_CLOEXEC);
    int vmfd = ioctl(kvm_fd, 0x01, 0);
    printf("vmfd=%d\n", vmfd);

    uint8_t code[] = "\xB0\x61\xBA\x17\x02\xEE\xB0\n\xEE\xF4";
    size_t mem_size = 0x40000000; // size of user memory you want to assign
    printf("code=%p\n", code);
    struct kvm_userspace_memory_region region = {
        .slot = 0,
        .flags = 0,
        .guest_phys_addr = 0,
        .memory_size = mem_size,
        .userspace_addr = (size_t)code
    };
    ioctl(vmfd, KVM_SET_USER_MEMORY_REGION, &region);

    int vcpufd = ioctl(vmfd, 0x00, 0);
    printf("vcpufd=%d\n", vcpufd);
    ioctl(vcpufd, 0x00, 0);

    return 0;
}


