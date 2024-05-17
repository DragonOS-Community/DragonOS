**groupmod**

- usage:修改用户组信息
 
     > groupmod [options] groupname

    groupadd -g\<new gid\> -n\<new groupname\> groupname

- 选项:  
    -g 设置新gid  
    -n 设置新组名

- 更新文件
    > /etc/group  
    > /etc/gshadow  
    > /etc/passwd