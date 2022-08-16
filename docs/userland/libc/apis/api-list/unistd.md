# unistd.h

## 简介：

    也是一些常用函数

## 函数列表：

    ``int close(int fd)`` ： 关闭文件
    
    ``ssize_t read(int fd,void *buf,size_t count)`` : 从文件读取
        
        传入文件id，缓冲区，以及字节数
        
        返回成功读取的字节数
    
    ``ssize_t write(int fd,void const *buf,size_t count)`` ： 写入文件

        传入文件id，缓冲区，字节数

        返回成功写入的字节数
    
    ``off_t lseek(int fd,off_t offset,int whence)`` : 调整文件访问位置

        传入文件id，偏移量，调整模式

        返回结束后的文件访问位置
    
    ``pid_t fork(void)`` ： fork 当前进程

    ``pid_t vfork(void)`` ： fork 当前进程，与父进程共享 VM,flags,fd

    ``uint64_t brk(uint64_t end_brk)`` : 将堆内存调整为end_brk
        
        若end_brk 为 -1，返回堆区域的起始地址

        若end_brk 为 -2，返回堆区域的结束地址

        否则调整堆区的结束地址域，并返回错误码
    
    ``void *sbrk(int64_t increment)`` :  
        
        将堆内存空间加上offset（注意，该系统调用只应在普通进程中调用，而不能是内核线程）

        increment ： 偏移量
    
    ``int64_t chdir(char *dest_path)``

        切换工作目录（传入目录路径）

    ``int execv(const char* path,char * const argv[])`` : 执行文件
        path ： 路径
        argv ： 执行参数列表
    
    ``extern int usleep(useconds_t usec)`` ： 睡眠usec微秒

