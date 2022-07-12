#pragma once

#include <common/sys/types.h>
struct dirent
{
    ino_t d_ino;    // 文件序列号
    off_t d_off;    // dir偏移量
    unsigned short d_reclen;    // 目录下的记录数
    unsigned char d_type;   // entry的类型
    char d_name[];   // 文件entry的名字（是一个零长度的数组）
};
