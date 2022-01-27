#pragma once

#include"../common/glib.h"

// Address Range Descriptor Structure 地址范围描述符
struct ARDS
{
    unsigned int BaseAddrL; // 基地址低32位
    unsigned int BaseAddrH; // 基地址高32位
    unsigned int LengthL;   // 内存长度低32位   以字节为单位
    unsigned int LengthH;   // 内存长度高32位
    unsigned int type;      // 本段内存的类型
                            // type=1 表示可以被操作系统使用
                            // type=2 ARR - 内存使用中或被保留，操作系统不能使用
                            // 其他 未定义，操作系统需要将其视为ARR
};




void mm_init();