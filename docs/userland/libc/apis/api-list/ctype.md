# ctype.h
## 函数列表（这里只列出已实现的函数）：  

    ``int isprint(int c)`` : 传入一个字符，判断是否可以被输出  

    ``int islower(int c)`` : 传入一个字符，判断是否是小写字母  

    ``int isupper(int c)`` : 传入一个字符，判断是否是大写字母  

    ``int isalpha(int c)`` : 传入一个字符，判断是否是字母  

    ``int isdigit(int c)`` : 传入一个字符，判断是否是数字  

    ``int toupper(int c)`` : 传入一个小写字母字符，返回这个字母的大写形式  

    ``int tolower(int c)`` : 传入一个大写字母字符，返回这个字母的小写形式  

    ``int isspace(int c)``  : 传入一个字符，判断是否是空白字符  

## 宏定义：

    ### 暂无用处  

    ``#define _U 01`` 
    
    ``#define _L 02``  

    ``#define _N 04``  

    ``#define _S 010``  

    ``#define _P 020``  

    ``#define _C 040``  

    ``#define _X 0100``  

    ``#define _B 0200`` 