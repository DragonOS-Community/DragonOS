**groupadd**

- usage:添加用户组
 
     > groupadd [options] groupname

    groupadd -g\<gid\> -p\<passwd\> groupname

- 选项:  
    -g\<gid\> 指定组id  
    -p 设置密码

- 更新文件
    > /etc/group  
    > /etc/gshadow