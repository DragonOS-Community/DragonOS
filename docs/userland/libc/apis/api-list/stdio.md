# stdio.h

## 简介：

    向标准输入输出里操作
## 函数列表：

    ``int64_t put_string(char *str, uint64_t front_color, uint64_t bg_color)``

        输出字符串（带有前景色，背景色）
    
    ``int printf(const char *fmt, ...)``
        
        就是正常的 ``printf`` 
    
    ``int sprintf(char *buf,const char *fmt,...)```

        就是正常的 ``sprintf``
    
    ``int vsprintf(char *buf,const char *fmt,va_list args)``

        格式化，不建议调用，请用 printf 或 sprintf 替代。
## 宏定义

    ### 字体颜色的宏定义

        ``#define COLOR_WHITE 0x00ffffff  //白``

        ``#define COLOR_BLACK 0x00000000  //黑``

        ``#define COLOR_RED 0x00ff0000    //红``

        ``#define COLOR_ORANGE 0x00ff8000 //橙``

        ``#define COLOR_YELLOW 0x00ffff00 //黄``

        ``#define COLOR_GREEN 0x0000ff00  //绿``

        ``#define COLOR_BLUE 0x000000ff   //蓝``

        ``#define COLOR_INDIGO 0x0000ffff //靛``

        ``#define COLOR_PURPLE 0x008000ff //紫``
    ### 无需使用
        
        ``#define SEEK_SET 0 /* Seek relative to start-of-file */``
        
        ``#define SEEK_CUR 1 /* Seek relative to current position */``
        
        ``#define SEEK_END 2 /* Seek relative to end-of-file */``

        ``#define SEEK_MAX 3``