#pragma once
/**
 * 请注意！！！由于系统调用模块已经使用Rust重构，当修改系统调用号时，需要同时修改syscall_num.h和syscall/mod.rs中的系统调用号
 * 并且以syscall/mod.rs中的为准！！！
 *
 * TODO：在完成系统的重构后，删除syscall_num.h
 *
 */

// 定义系统调用号
#define SYS_READ 0
#define SYS_WRITE 1
#define SYS_OPEN 2
#define SYS_CLOSE 3

#define SYS_FSTAT 5
#define SYS_LSEEK 8
#define SYS_MMAP 9
#define SYS_MPROTECT 10
#define SYS_MUNMAP 11
#define SYS_BRK 12
#define SYS_SIGACTION 13

#define SYS_RT_SIGRETURN 15
#define SYS_IOCTL 16

#define SYS_DUP 32
#define SYS_DUP2 33

#define SYS_NANOSLEEP 35

#define SYS_GETPID 39

#define SYS_SOCKET 41
#define SYS_CONNECT 42
#define SYS_ACCEPT 43
#define SYS_SENDTO 44
#define SYS_RECVFROM 45

#define SYS_RECVMSG 47
#define SYS_SHUTDOWN 48
#define SYS_BIND 49
#define SYS_LISTEN 50
#define SYS_GETSOCKNAME 51
#define SYS_GETPEERNAME 52

#define SYS_SETSOCKOPT 54
#define SYS_GETSOCKOPT 55
#define SYS_CLONE 56
#define SYS_FORK 57
#define SYS_VFORK 58
#define SYS_EXECVE 59
#define SYS_EXIT 60
#define SYS_WAIT4 61
#define SYS_KILL 62

#define SYS_FCNTL 72

#define SYS_FTRUNCATE 77
#define SYS_GET_DENTS 78

#define SYS_GETCWD 79

#define SYS_CHDIR 80

#define SYS_MKDIR 83

#define SYS_RMDIR 84

#define SYS_GETTIMEOFDAY 96

#define SYS_ARCH_PRCTL 158

#define SYS_REBOOT 169

#define SYS_GETPPID 110
#define SYS_GETPGID 121

#define SYS_MKNOD 133

#define SYS_FUTEX 202

#define SYS_SET_TID_ADDR 218

#define SYS_UNLINK_AT 263

#define SYS_PIPE 293

#define SYS_WRITEV 20

// 与linux不一致的调用，在linux基础上累加
#define SYS_PUT_STRING 100000
#define SYS_SBRK 100001
/// todo: 该系统调用与Linux不一致，将来需要删除该系统调用！！！
/// 删的时候记得改C版本的libc
#define SYS_CLOCK 100002
#define SYS_SCHED 100003
