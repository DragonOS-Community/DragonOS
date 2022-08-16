# time.h

## 简介：

    时间相关

时刻以纳秒为单位

## 结构体：

    ``struct timespec`` : 时间戳
        
        ### 变量列表:
            
            ``long int tv_sec`` : 秒
            
            ``long int tv_nsec`` : 纳秒 
## 宏定义：

    ``#define CLOCKS_PER_SEC 1000000`` 每一秒有1000000个时刻（纳秒）

## 函数列表：

    ``int nanosleep(const struct timespec *rdtp,struct timespec *rmtp)``

        休眠指定时间

        rdtp ： 指定休眠的时间

        rmtp ： 返回剩余时间
    
    ``clock_t clock()`` ： 获得当前系统时间