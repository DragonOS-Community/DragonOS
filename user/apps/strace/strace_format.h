#ifndef _STRACE_FORMAT_H_
#define _STRACE_FORMAT_H_

#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <iomanip>
#include <linux/random.h>
#include <sstream>
#include <sys/mman.h>
#include <sys/ptrace.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/user.h>
#include <sys/utsname.h>
#include <unistd.h>
#include <unordered_map>
#include <vector>

// 根据不同架构定义寄存器访问宏
#if defined(__x86_64__)
#define SYSCALL_REG(regs) (regs.orig_rax)
#define RETURN_REG(regs)  (regs.rax)
#define ARG1(regs)        (regs.rdi)
#define ARG2(regs)        (regs.rsi)
#define ARG3(regs)        (regs.rdx)
#define ARG4(regs)        (regs.r10)
#define ARG5(regs)        (regs.r8)
#define ARG6(regs)        (regs.r9)
#elif defined(__i386__)
#define SYSCALL_REG(regs) (regs.orig_eax)
#define RETURN_REG(regs)  (regs.eax)
#define ARG1(regs)        (regs.ebx)
#define ARG2(regs)        (regs.ecx)
#define ARG3(regs)        (regs.edx)
#define ARG4(regs)        (regs.esi)
#define ARG5(regs)        (regs.edi)
#define ARG6(regs)        (regs.ebp)
#elif defined(__aarch64__)
#define SYSCALL_REG(regs) (regs.regs[8])
#define RETURN_REG(regs)  (regs.regs[0])
#define ARG1(regs)        (regs.regs[0])
#define ARG2(regs)        (regs.regs[1])
#define ARG3(regs)        (regs.regs[2])
#define ARG4(regs)        (regs.regs[3])
#define ARG5(regs)        (regs.regs[4])
#define ARG6(regs)        (regs.regs[5])
#else
#error "Unsupported architecture"
#endif

// 错误码映射
const std::unordered_map<int, std::string> error_names = {
    {EPERM, "EPERM"},     {ENOENT, "ENOENT"}, {ESRCH, "ESRCH"},     {EINTR, "EINTR"},   {EIO, "EIO"},
    {ENXIO, "ENXIO"},     {E2BIG, "E2BIG"},   {ENOEXEC, "ENOEXEC"}, {EBADF, "EBADF"},   {ECHILD, "ECHILD"},
    {EAGAIN, "EAGAIN"},   {ENOMEM, "ENOMEM"}, {EACCES, "EACCES"},   {EFAULT, "EFAULT"}, {ENOTBLK, "ENOTBLK"},
    {EBUSY, "EBUSY"},     {EEXIST, "EEXIST"}, {EXDEV, "EXDEV"},     {ENODEV, "ENODEV"}, {ENOTDIR, "ENOTDIR"},
    {EISDIR, "EISDIR"},   {EINVAL, "EINVAL"}, {ENFILE, "ENFILE"},   {EMFILE, "EMFILE"}, {ENOTTY, "ENOTTY"},
    {ETXTBSY, "ETXTBSY"}, {EFBIG, "EFBIG"},   {ENOSPC, "ENOSPC"},   {ESPIPE, "ESPIPE"}, {EROFS, "EROFS"},
    {EMLINK, "EMLINK"},   {EPIPE, "EPIPE"},   {EDOM, "EDOM"},       {ERANGE, "ERANGE"},
};

// 系统调用名称映射
const std::unordered_map<int, std::string> syscall_names = {
    {SYS_read, "read"},
    {SYS_write, "write"},
    {SYS_open, "open"},
    {SYS_close, "close"},
    {SYS_stat, "stat"},
    {SYS_fstat, "fstat"},
    {SYS_lstat, "lstat"},
    {SYS_poll, "poll"},
    {SYS_lseek, "lseek"},
    {SYS_mmap, "mmap"},
    {SYS_mprotect, "mprotect"},
    {SYS_munmap, "munmap"},
    {SYS_brk, "brk"},
    {SYS_rt_sigaction, "rt_sigaction"},
    {SYS_ioctl, "ioctl"},
    {SYS_access, "access"},
    {SYS_pipe, "pipe"},
    {SYS_select, "select"},
    {SYS_dup, "dup"},
    {SYS_dup2, "dup2"},
    {SYS_getpid, "getpid"},
    {SYS_socket, "socket"},
    {SYS_connect, "connect"},
    {SYS_bind, "bind"},
    {SYS_listen, "listen"},
    {SYS_accept, "accept"},
    {SYS_execve, "execve"},
    {SYS_exit, "exit"},
    {SYS_wait4, "wait4"},
    {SYS_kill, "kill"},
    {SYS_uname, "uname"},
    {SYS_fcntl, "fcntl"},
    {SYS_fsync, "fsync"},
    {SYS_truncate, "truncate"},
    {SYS_getcwd, "getcwd"},
    {SYS_chdir, "chdir"},
    {SYS_rename, "rename"},
    {SYS_mkdir, "mkdir"},
    {SYS_rmdir, "rmdir"},
    {SYS_creat, "creat"},
    {SYS_link, "link"},
    {SYS_unlink, "unlink"},
    {SYS_readlink, "readlink"},
    {SYS_chmod, "chmod"},
    {SYS_gettimeofday, "gettimeofday"},
    {SYS_getrusage, "getrusage"},
    {SYS_sysinfo, "sysinfo"},
    {SYS_getuid, "getuid"},
    {SYS_getgid, "getgid"},
    {SYS_setuid, "setuid"},
    {SYS_setgid, "setgid"},
    {SYS_geteuid, "geteuid"},
    {SYS_getegid, "getegid"},
    {SYS_setpgid, "setpgid"},
    {SYS_getppid, "getppid"},
    {SYS_arch_prctl, "arch_prctl"},
    {SYS_exit_group, "exit_group"},
    {SYS_openat, "openat"},
    {SYS_newfstatat, "newfstatat"},
    {SYS_unshare, "unshare"},
    {SYS_getrandom, "getrandom"},
};

const std::unordered_map<int, std::string> fcntl_flags = {
    {FD_CLOEXEC, "FD_CLOEXEC"}, {O_RDONLY, "O_RDONLY"},       {O_WRONLY, "O_WRONLY"},       {O_RDWR, "O_RDWR"},
    {O_CREAT, "O_CREAT"},       {O_EXCL, "O_EXCL"},           {O_NOCTTY, "O_NOCTTY"},       {O_TRUNC, "O_TRUNC"},
    {O_APPEND, "O_APPEND"},     {O_NONBLOCK, "O_NONBLOCK"},   {O_DSYNC, "O_DSYNC"},         {O_ASYNC, "O_ASYNC"},
    {O_DIRECT, "O_DIRECT"},     {O_LARGEFILE, "O_LARGEFILE"}, {O_DIRECTORY, "O_DIRECTORY"}, {O_NOFOLLOW, "O_NOFOLLOW"},
    {O_NOATIME, "O_NOATIME"},   {O_CLOEXEC, "O_CLOEXEC"},     {O_SYNC, "O_SYNC"},           {O_PATH, "O_PATH"},
    {O_TMPFILE, "O_TMPFILE"},
};
const std::unordered_map<int, std::string> at_flags = {
    {AT_SYMLINK_NOFOLLOW, "AT_SYMLINK_NOFOLLOW"},
    {AT_REMOVEDIR, "AT_REMOVEDIR"},
    {AT_SYMLINK_FOLLOW, "AT_SYMLINK_FOLLOW"},
    {AT_NO_AUTOMOUNT, "AT_NO_AUTOMOUNT"},
    {AT_EMPTY_PATH, "AT_EMPTY_PATH"},
    {AT_STATX_SYNC_TYPE, "AT_STATX_SYNC_TYPE"},
    {AT_STATX_FORCE_SYNC, "AT_STATX_FORCE_SYNC"},
    {AT_STATX_DONT_SYNC, "AT_STATX_DONT_SYNC"},
    {AT_RECURSIVE, "AT_RECURSIVE"},
};
const std::unordered_map<int, std::string> open_flags = {
    {O_RDONLY, "O_RDONLY"},
    {O_WRONLY, "O_WRONLY"},
    {O_RDWR, "O_RDWR"},
    {O_CREAT, "O_CREAT"},
    {O_EXCL, "O_EXCL"},
    {O_TRUNC, "O_TRUNC"},
    {O_APPEND, "O_APPEND"},
    {O_NONBLOCK, "O_NONBLOCK"},
    {O_DIRECTORY, "O_DIRECTORY"},
    {O_NOFOLLOW, "O_NOFOLLOW"},
    {O_CLOEXEC, "O_CLOEXEC"},
    {AT_FDCWD, "AT_FDCWD"},
    {AT_SYMLINK_NOFOLLOW, "AT_SYMLINK_NOFOLLOW"},
};
const std::unordered_map<int, std::string> mmap_flags = {
    {MAP_SHARED, "MAP_SHARED"},
    {MAP_PRIVATE, "MAP_PRIVATE"},
    {MAP_FIXED, "MAP_FIXED"},
    {MAP_ANONYMOUS, "MAP_ANONYMOUS"},
    {MAP_GROWSDOWN, "MAP_GROWSDOWN"},
    {MAP_DENYWRITE, "MAP_DENYWRITE"},
    {MAP_EXECUTABLE, "MAP_EXECUTABLE"},
    {MAP_LOCKED, "MAP_LOCKED"},
    {MAP_NORESERVE, "MAP_NORESERVE"},
    {MAP_POPULATE, "MAP_POPULATE"},
    {MAP_NONBLOCK, "MAP_NONBLOCK"},
    {MAP_STACK, "MAP_STACK"},
    {MAP_HUGETLB, "MAP_HUGETLB"},
};
const std::unordered_map<int, std::string> grnd_flags = {
    {GRND_NONBLOCK, "GRND_NONBLOCK"},
    {GRND_RANDOM, "GRND_RANDOM"},
};

// 长整型转十六进制字符串
std::string to_hex_string(unsigned long value) {
    std::ostringstream oss;
    oss << "0x" << std::hex << value;
    return oss.str();
}
// 从子进程读取字符串
std::string read_child_string(pid_t pid, unsigned long addr) {
    if (addr == 0)
        return "NULL";
    std::string str;
    long ret;
    unsigned long tmp = 0;

    while (true) {
        // 使用PTRACE_PEEKDATA读取内存
        errno = 0;
        ret = ptrace(PTRACE_PEEKDATA, pid, addr + tmp, nullptr);
        if (ret == -1 && errno != 0) {
            return "<error>";
        }

        // 逐字节读取，直到遇到空字符
        for (int i = 0; i < sizeof(long); ++i) {
            char ch = static_cast<char>((ret >> (i * 8)) & 0xFF);
            if (ch == '\0') {
                return str;
            }
            str += ch;
        }
        tmp += sizeof(long);
    }
}
std::string read_child_buffer(pid_t pid, unsigned long addr, size_t len) {
    if (addr == 0 || len == 0)
        return "";

    std::string buffer;
    long ret;
    unsigned long tmp = 0;
    const size_t max_length = 256; // 最大读取长度

    len = std::min(len, max_length);

    for (size_t i = 0; i < len; i += sizeof(long)) {
        // 使用PTRACE_PEEKDATA读取内存
        errno = 0;
        ret = ptrace(PTRACE_PEEKDATA, pid, addr + tmp, nullptr);
        if (ret == -1 && errno != 0) {
            return "<error>";
        }

        // 逐字节读取
        for (int j = 0; j < sizeof(long) && (i + j) < len; ++j) {
            char ch = static_cast<char>((ret >> (j * 8)) & 0xFF);
            buffer += ch;
        }
        tmp += sizeof(long);
    }
    return buffer;
}
std::string format_printable_string(const std::string& str) {
    std::ostringstream oss;
    oss << "\"";
    for (char c : str) {
        if (c == '\n')
            oss << "\\n";
        else if (c == '\t')
            oss << "\\t";
        else if (c == '\r')
            oss << "\\r";
        else if (c == '\"')
            oss << "\\\"";
        else if (c == '\\')
            oss << "\\\\";
        else if (std::isprint(static_cast<unsigned char>(c)))
            oss << c;
        else
            oss << "\\x" << std::hex << std::setw(2) << std::setfill('0')
                << static_cast<int>(static_cast<unsigned char>(c));
    }
    oss << "\"";
    return oss.str();
}
// 从子进程读取字符串数组
std::vector<std::string> read_child_string_array(pid_t pid, unsigned long addr) {
    std::vector<std::string> result;
    if (addr == 0)
        return result;

    unsigned long ptr;
    unsigned long tmp = 0;

    while (true) {
        // 读取指针值
        errno = 0;
        long ret = ptrace(PTRACE_PEEKDATA, pid, addr + tmp, nullptr);
        if (ret == -1 && errno != 0) {
            break;
        }

        ptr = static_cast<unsigned long>(ret);
        tmp += sizeof(long);

        if (ptr == 0) {
            break; // NULL结尾
        }

        result.push_back(read_child_string(pid, ptr));
    }

    return result;
}
// 解析标志位
std::string parse_flags(const std::unordered_map<int, std::string>& flag_map, long flags) {
    if (flags == 0)
        return "0";
    std::vector<std::string> flag_list;
    for (const auto& flag : flag_map) {
        if (flags & flag.first) {
            flag_list.push_back(flag.second);
        }
    }
    if (flag_list.empty()) {
        return "0x" + to_hex_string(flags);
    }
    std::ostringstream oss;
    for (size_t i = 0; i < flag_list.size(); ++i) {
        if (i > 0)
            oss << "|";
        oss << flag_list[i];
    }
    return oss.str();
}

// 格式化参数列表
std::string
format_arguments(pid_t child_pid, int sys_num, long arg1, long arg2, long arg3, long arg4, long arg5, long arg6) {
    // 获取系统调用名称
    std::string syscall_name = "syscall_";
    if (syscall_names.find(sys_num) != syscall_names.end()) {
        syscall_name = syscall_names.at(sys_num);
    } else {
        syscall_name += std::to_string(sys_num);
    }

    std::ostringstream oss;
    if (sys_num == SYS_execve) {
        // 处理execve的特殊格式
        std::string path = read_child_string(child_pid, arg1);
        std::vector<std::string> argv = read_child_string_array(child_pid, arg2);
        std::vector<std::string> envp = read_child_string_array(child_pid, arg3);

        oss << syscall_name << "(\"" << path << "\", [";
        // 格式化argv
        for (size_t i = 0; i < argv.size(); ++i) {
            if (i > 0)
                oss << ", ";
            oss << "\"" << argv[i] << "\"";
        }
        oss << "], ";
        // 格式化envp
        if (envp.empty()) {
            oss << "0x" << std::hex << arg3 << " /* 0 vars */)";
        } else {
            oss << "0x" << std::hex << arg3 << " /* " << std::dec << envp.size() << " vars */";
        }
        return oss.str();
    } else if (sys_num == SYS_brk) {
        oss << syscall_name << "(";
        if (arg1 == 0) {
            oss << "NULL";
        } else {
            oss << to_hex_string(arg1);
        }
        oss << ")";
        return oss.str();
    } else if (sys_num == SYS_open || sys_num == SYS_openat) {
        oss << syscall_name << "(";
        if (sys_num == SYS_openat) {
            oss << "AT_FDCWD, ";
        }
        // 读取路径
        oss << "\"" << read_child_string(child_pid, (sys_num == SYS_openat) ? arg2 : arg1) << "\", ";
        // 解析标志位
        long flags = (sys_num == SYS_openat) ? arg3 : arg2;
        oss << parse_flags(open_flags, flags);
        // 文件权限
        if (arg4 != 0)
            oss << ", 0" << std::oct << arg4;
        return oss.str();
    } else if (sys_num == SYS_write) {
        oss << syscall_name << "(" << arg1 << ", ";
        std::string buffer = read_child_buffer(child_pid, arg2, arg3);
        oss << format_printable_string(buffer) << ", " << arg3 << ")";
        return oss.str();
    } else if (sys_num == SYS_read) {
        oss << syscall_name << "(" << arg1 << ", ";
        std::string buffer = read_child_buffer(child_pid, arg2, arg3);
        oss << format_printable_string(buffer) << ", " << arg3 << ")";
        return oss.str();
    } else if (sys_num == SYS_dup || sys_num == SYS_dup2 || sys_num == SYS_dup3) {
        oss << syscall_name << "(" << arg1;
        if (sys_num == SYS_dup2 || sys_num == SYS_dup3)
            oss << ", " << arg2;
        if (sys_num == SYS_dup3)
            oss << ", " << parse_flags(fcntl_flags, arg3);
        oss << ")";
        return oss.str();
    } else if (sys_num == SYS_newfstatat) {
        oss << syscall_name << "(";
        // 文件描述符
        if (arg1 == AT_FDCWD) {
            oss << "AT_FDCWD";
        } else {
            oss << arg1;
        }
        oss << ", \"" << read_child_string(child_pid, arg2) << "\"";
        // stat结构体指针
        oss << ", " << to_hex_string(arg3);
        oss << ", " << parse_flags(at_flags, arg4);
        oss << ")";
        return oss.str();
    } else if (sys_num == SYS_mmap) {
        oss << syscall_name << "(" << to_hex_string(arg1) << ", " << std::dec << arg2 << ", "
            << parse_flags(mmap_flags, arg3) << ", " << parse_flags(mmap_flags, arg4) << ", " << arg5 << ", "
            << to_hex_string(arg6) << ")";
        return oss.str();
    } else if (sys_num == SYS_arch_prctl) {
        oss << syscall_name << "(" << to_hex_string(arg1) << ", " << to_hex_string(arg2) << ")";
        return oss.str();
    } else if (sys_num == SYS_fcntl) {
        std::ostringstream oss;
        oss << syscall_name << "(" << arg1 << ", ";
        if (fcntl_flags.count(static_cast<int>(arg2))) {
            oss << fcntl_flags.at(static_cast<int>(arg2));
        } else {
            oss << to_hex_string(arg2);
        }
        // 如果有第三个参数
        if (arg3 != 0) {
            oss << ", ";
            switch (static_cast<int>(arg2)) {
            case F_SETFL:
            case F_GETFL:
                oss << parse_flags(fcntl_flags, arg3);
                break;
            default:
                oss << to_hex_string(arg3);
            }
        }

        oss << ")";
        return oss.str();
    } else if (sys_num == SYS_uname) {
        // const size_t utsname_size = sizeof(struct utsname);
        // const size_t words = (utsname_size + sizeof(long) - 1) / sizeof(long);
        // // 分配缓冲区
        // char* buf = new char[words * sizeof(long)]();
        // unsigned long addr = arg1;
        // // 逐字读取子进程内存
        // for (size_t i = 0; i < words; ++i) {
        //     errno = 0;
        //     long ret = ptrace(PTRACE_PEEKDATA, child_pid, addr + i * sizeof(long), nullptr);
        //     if (ret == -1 && errno != 0) {
        //         delete[] buf;
        //         return syscall_name + "(" + to_hex_string(arg1) + ")";
        //     }
        //     memcpy(buf + i * sizeof(long), &ret, sizeof(long));
        // }
        // struct utsname* uname_buf = reinterpret_cast<struct utsname*>(buf);
        struct utsname parent_uname;
        uname(&parent_uname);
        oss << syscall_name << "({";
        oss << "sysname=\"" << parent_uname.sysname << "\", ";
        oss << "nodename=\"" << parent_uname.nodename << "\", ";
        oss << "release=\"" << parent_uname.release << "\", ";
        oss << "version=\"" << parent_uname.version << "\", ";
        oss << "machine=\"" << parent_uname.machine << "\"";
        oss << "})";
        // delete[] buf;
        return oss.str();
    } else if (sys_num == SYS_getrandom) {
        oss << syscall_name << "(" << to_hex_string(arg1) << ", " << arg2 << ", " << parse_flags(grnd_flags, arg3)
            << ")";
        return oss.str();
    } else if (sys_num == SYS_access) {
        return syscall_name + "(\"" + read_child_string(child_pid, arg1) + "\", " + to_hex_string(arg2) + ")";
    } else if (sys_num == SYS_exit || sys_num == SYS_exit_group || sys_num == SYS_close) {
        return syscall_name + "(" + std::to_string(arg1) + ")";
    } else {
        // 其他系统调用通用处理
        oss << syscall_name << "(" << to_hex_string(arg1);
        if (arg2 != 0)
            oss << ", " << to_hex_string(arg2);
        if (arg3 != 0)
            oss << ", " << to_hex_string(arg3);
        if (arg4 != 0)
            oss << ", " << to_hex_string(arg4);
        if (arg5 != 0)
            oss << ", " << to_hex_string(arg5);
        if (arg6 != 0)
            oss << ", " << to_hex_string(arg6);
        oss << ")";
        return oss.str();
    }
}
// 格式化返回值
std::string format_return_value(long ret_val) {
    if (ret_val < 0) {
        int err = static_cast<int>(-ret_val);
        if (error_names.find(err) != error_names.end()) {
            return " = -1 " + error_names.at(err) + " (" + strerror(err) + ")";
        } else {
            return " = -1 (unknown error)";
        }
    }

    return " = " + to_hex_string(ret_val);
}

#endif // _STRACE_FORMAT_H_