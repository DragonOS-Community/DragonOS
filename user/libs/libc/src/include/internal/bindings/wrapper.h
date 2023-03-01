/**
 * @file wrapper.h
 * @author longjin (longjin@RinGoTek.cn)
 * @brief 这是为libc的C代码的相关接口创建rust绑定的wrapper
 * @version 0.1
 * @date 2023-02-11
 *
 * @copyright Copyright (c) 2023
 *
 */
#pragma once

// 这里导出在include/export文件夹下的头文件
#include <stdio.h>
#include <unistd.h>