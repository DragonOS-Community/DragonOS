#pragma once

#define ARCH(arch) (defined(AK_ARCH_##arch) && AK_ARCH_##arch)


#ifdef __i386__
#    define AK_ARCH_I386 1
#endif

#ifdef __x86_64__
#    define AK_ARCH_X86_64 1
#endif

#ifdef __riscv
#    define AK_ARCH_riscv 1
#endif

#ifdef __riscv64
#    define AK_ARCH_riscv64 1
#endif