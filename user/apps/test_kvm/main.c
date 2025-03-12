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
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define KVM_CREATE_VCPU 0x00
#define KVM_SET_USER_MEMORY_REGION 0x01

#define KVM_RUN 0x00
#define KVM_GET_REGS 0x01
#define KVM_SET_REGS 0x02

struct kvm_userspace_memory_region {
    uint32_t slot; // 要在哪个slot上注册内存区间
    // flags有两个取值，KVM_MEM_LOG_DIRTY_PAGES和KVM_MEM_READONLY，用来指示kvm针对这段内存应该做的事情。
    // KVM_MEM_LOG_DIRTY_PAGES用来开启内存脏页，KVM_MEM_READONLY用来开启内存只读。
    uint32_t flags;
    uint64_t guest_phys_addr; // 虚机内存区间起始物理地址
    uint64_t memory_size;     // 虚机内存区间大小
    uint64_t userspace_addr;  // 虚机内存区间对应的主机虚拟地址
};

struct kvm_regs {
	/* out (KVM_GET_REGS) / in (KVM_SET_REGS) */
	uint64_t rax, rbx, rcx, rdx;
	uint64_t rsi, rdi, rsp, rbp;
	uint64_t r8,  r9,  r10, r11;
	uint64_t r12, r13, r14, r15;
	uint64_t rip, rflags;
};

int guest_code(){
    while (1)
    {
        // printf("guest code\n");
        __asm__ __volatile__ (
            "mov %rax, 0\n\t"
            "mov %rcx, 0\n\t"
            "cpuid\n\t"
        );
    }
    return 0;
}

int main()
{
    printf("Test kvm running...\n");
    printf("Open /dev/kvm\n");
    int kvm_fd = open("/dev/kvm", O_RDWR|O_CLOEXEC);
    int vmfd = ioctl(kvm_fd, 0x01, 0);
    printf("vmfd=%d\n", vmfd);

    /*
         __asm__ __volatile__ (
            "mov %rax, 0\n\t"
            "mov %rcx, 0\n\t"
            "cpuid\n\t"
        ); 
    */
    const uint8_t code[] = {
        0xba, 0xf8, 0x03, /* mov $0x3f8, %dx */
        0x00, 0xd8,       /* add %bl, %al */
        0x04, '0',        /* add $'0', %al */
        0xee,             /* out %al, (%dx) */
        0xb0, '\n',       /* mov $'\n', %al */
        0xee,             /* out %al, (%dx) */
        0xf4,             /* hlt */
    };

    size_t mem_size = 0x4000; // size of user memory you want to assign
    printf("code=%p\n", code);
    // void *mem = mmap(0, mem_size, 0x7, -1, 0);
    // memcpy(mem, code, sizeof(code));
    struct kvm_userspace_memory_region region = {
        .slot = 0,
        .flags = 0,
        .guest_phys_addr = 0,
        .memory_size = mem_size,
        .userspace_addr = (size_t)code
    };
    ioctl(vmfd, KVM_SET_USER_MEMORY_REGION, &region);

    int vcpufd = ioctl(vmfd, KVM_CREATE_VCPU, 0);
    printf("vcpufd=%d\n", vcpufd);
    int user_entry = 0x0;

    struct kvm_regs regs = {0};
    regs.rip = user_entry;
    regs.rsp = 0x3000; // stack address
    regs.rflags = 0x2; // in x86 the 0x2 bit should always be set
    ioctl(vcpufd, KVM_SET_REGS, &regs); // set registers

    ioctl(vcpufd, KVM_RUN, 0);

    return 0;
}


