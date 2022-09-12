# dirent.h
## 简介
    与文件夹有关的头文件。

## 结构体列表：

    ``struct DIR`` : 
        
        变量列表：

        ``int fd`` : 文件夹id（不推荐修改）

        ``int buf_pos`` : 文件夹缓冲区指针的位置
        
        ``int buf_len`` : 文件夹缓冲区的大小（默认为256）
    
    ``struct dirent`` : 
        
        变量列表： 

        ``ino_t(see libc/sys/types.h) ino`` : 文件序列号（不推荐修改）

        ``off_t d_off`` : dir偏移量（不推荐修改）

        ``unsigned short d_reclen`` : 文件夹中的记录数

        ``unsigned char d_type`` : 目标的类型(有可能是文件，文件夹，磁盘)

        ``char d_name[]`` : 目标的名字

## 函数列表（这里只列出已实现的函数）：

    ``DIR opendir(const char *path)``  
        
        传入文件夹的路径，返回文件夹结构体
    
    ``int closedir(DIR *dirp)`` 

        传入文件夹结构体，关闭文件夹，释放内存

        若失败，返回-1

    ``dirent readdir(DIR *dir)``

        传入文件夹结构体，读入文件夹里的内容，并打包为dirent结构体返回

## 宏定义：

    文件夹类型：

    ``#define VFS_IF_FILE (1UL << 0)``
    
    ``#define VFS_IF_DIR (1UL << 1)``
    
    ``#define VFS_IF_DEVICE (1UL << 2)``
    
    缓冲区长度的默认值
    
    ``#define DIR_BUF_SIZE 256``
