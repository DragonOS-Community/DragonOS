/**
 * @file fcntl.h
 * @author fslongjin (longjin@RinGoTek.cn)
 * @brief
 * @version 0.1
 * @date 2022-04-26
 *
 * @copyright Copyright (c) 2022
 *
 */
#pragma once

#define O_RDONLY 00000000 // Open Read-only
#define O_WRONLY 00000001 // Open Write-only
#define O_RDWR 00000002   // Open read/write
#define O_ACCMODE 00000003 // Mask for file access modes

#define O_CREAT 00000100 // Create file if it does not exist
#define O_EXCL 00000200 // Fail if file already exists
#define O_NOCTTY 00000400 // Do not assign controlling terminal

#define O_TRUNC 00001000 // 文件存在且是普通文件，并以O_RDWR或O_WRONLY打开，则它会被清空

#define O_APPEND 00002000   // 文件指针会被移动到文件末尾

#define O_NONBLOCK 00004000 // 非阻塞式IO模式

#define O_EXEC 00010000 // 以仅执行的方式打开（非目录文件）
#define O_SEARCH 00020000   // Open the directory for search only
#define O_DIRECTORY 00040000 // 打开的必须是一个目录
#define O_NOFOLLOW 00100000 // Do not follow symbolic links


/**
 * @brief 打开文件的接口
 * 
 * @param path 文件路径
 * @param options 打开选项
 * @param ... 
 * @return int 文件描述符
 */
int open(const char * path, int options, ...);