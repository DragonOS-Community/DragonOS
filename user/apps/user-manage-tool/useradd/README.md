# useradd
* usage：添加用户
    > useradd [options] username

    useradd -c \<comment\> -d \<home\> -g \<group\> -s \<shell\> -u \<uid\> username

* 参数说明：
    * 选项:  
    -c comment 指定一段注释性描述  
    -d 目录 指定用户主目录，如果不存在，则创建该目录  
    -g 用户组 指定用户所属的用户组  
    -s Shell文件 指定用户的登录Shell  
    -u 用户号 指定用户的用户号

    * 用户名:  
    指定新账号的登录名。

* 更新文件：
    > /etc/passwd  
    > /etc/shadow  
    > /etc/group  
    > /etc/gshadow

*/etc/passwd文件格式：*
>用户名:口令:用户标识号:组标识号:注释性描述:主目录:登录Shell


*/etc/shadow文件格式：*
>登录名:加密口令:最后一次修改时间:最小时间间隔:最大时间间隔:警告时间:不活动时间:失效时间:标志


*/etc/group文件格式：*
>组名:口令:组标识号:组内用户列表


*/etc/gshadow文件格式：*
> 组名:组密码:组管理员名称:组成员
