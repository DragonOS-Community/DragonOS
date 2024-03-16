#include <stdio.h>
#include <sys/utsname.h>
#include <stdlib.h>

int main() {
    struct utsname system_info;

    // 调用 uname 函数获取系统信息
    uname(&system_info);

    // 打印系统信息
    printf("System name: %s\n", system_info.sysname);
    printf("Node name: %s\n", system_info.nodename);
    printf("Release: %s\n", system_info.release);
    printf("Version: %s\n", system_info.version);
    printf("Machine: %s\n", system_info.machine);

    return 0;
}