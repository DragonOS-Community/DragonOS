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

#define O_RDONLY 00000000  // Open Read-only
#define O_WRONLY 00000001  // Open Write-only
#define O_RDWR 00000002    // Open read/write
#define O_ACCMODE 00000003 // Mask for file access modes

#define O_CREAT 00000100  // Create file if it does not exist
#define O_EXCL 00000200   // Fail if file already exists
#define O_NOCTTY 00000400 // Do not assign controlling terminal

#define O_TRUNC 00001000 // 文件存在且是普通文件，并以O_RDWR或O_WRONLY打开，则它会被清空

#define O_APPEND 00002000 // 文件指针会被移动到文件末尾

#define O_NONBLOCK 00004000 // 非阻塞式IO模式

#define O_EXEC 00010000      // 以仅执行的方式打开（非目录文件）
#define O_SEARCH 00020000    // Open the directory for search only
#define O_DIRECTORY 00040000 // 打开的必须是一个目录
#define O_NOFOLLOW 00100000  // Do not follow symbolic links

/*
 * The constants AT_REMOVEDIR and AT_EACCESS have the same value.  AT_EACCESS is
 * meaningful only to faccessat, while AT_REMOVEDIR is meaningful only to
 * unlinkat.  The two functions do completely different things and therefore,
 * the flags can be allowed to overlap.  For example, passing AT_REMOVEDIR to
 * faccessat would be undefined behavior and thus treating it equivalent to
 * AT_EACCESS is valid undefined behavior.
 */
// 作为当前工作目录的文件描述符（用于指代cwd）
#define AT_FDCWD -100
#define AT_SYMLINK_NOFOLLOW 0x100 /* Do not follow symbolic links.  */
#define AT_EACCESS 0x200          /* Test access permitted for effective IDs, not real IDs.  */
#define AT_REMOVEDIR 0x200        /* Remove directory instead of unlinking file.  */
#define AT_SYMLINK_FOLLOW 0x400   /* Follow symbolic links.  */
#define AT_NO_AUTOMOUNT 0x800     /* Suppress terminal automount traversal */
#define AT_EMPTY_PATH 0x1000      /* Allow empty relative pathname */

#define AT_STATX_SYNC_TYPE 0x6000    /* Type of synchronisation required from statx() */
#define AT_STATX_SYNC_AS_STAT 0x0000 /* - Do whatever stat() does */
#define AT_STATX_FORCE_SYNC 0x2000   /* - Force the attributes to be sync'd with the server */
#define AT_STATX_DONT_SYNC 0x4000    /* - Don't sync attributes with the server */

#define AT_RECURSIVE 0x8000 /* Apply to the entire subtree */