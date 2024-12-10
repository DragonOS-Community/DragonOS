## useradd

- usage：添加用户

  > useradd [options] username

  useradd -c \<comment\> -d \<home\> -G \<group\> -g \<gid\> -s \<shell\> -u \<uid\> username

- 参数说明：

  - 选项:  
    -c comment 指定一段注释性描述  
    -d 目录 指定用户主目录，如果不存在，则创建该目录  
    -G 用户组 指定用户所属的用户组  
    -g 组id  
    -s Shell 文件 指定用户的登录 Shell  
    -u 用户号 指定用户的用户号

  - 用户名:  
    指定新账号的登录名。

- 更新文件：
  > /etc/passwd  
  > /etc/shadow  
  > /etc/group  
  > /etc/gshadow

## userdel

- usage：删除用户

  > userdel [options] username

  userdel -r username

- 选项:  
   -r 连同用户主目录一起删除。

- 更新文件：
  > /etc/passwd  
  > /etc/shadow  
  > /etc/group

## usermod

- usage：修改用户

  > usermod [options] username

  usermod -a -G<组 1,组 2,...> -c<备注> -d<登入目录> -G<组名> -l<名称> -s<登入终端> -u<用户 id> username

- 选项:  
   -a -G<组 1,组 2,...> 将用户添加到其它组中  
   -c<备注> 　修改用户帐号的备注文字。  
   -d 登入目录> 　修改用户登入时的目录。  
   -G<组名> 　修改用户所属的群组。  
   -l<名称> 　修改用户名称。  
   -s\<shell\> 　修改用户登入后所使用的 shell。  
   -u\<uid\> 　修改用户 ID。

- 更新文件：
  > /etc/passwd  
  > /etc/shadow  
  > /etc/group  
  > /etc/gshadow

## passwd

- usage:设置密码

  > 普通用户: passwd  
  > root 用户: passwd username

  普通用户只能修改自己的密码，因此不需要指定用户名。

- 更新文件
  > /etc/shadow  
  > /etc/passwd

## groupadd

- usage:添加用户组

  > groupadd [options] groupname

  groupadd -g\<gid\> -p\<passwd\> groupname

- 选项:  
   -g\<gid\> 指定组 id  
   -p 设置密码

- 更新文件
  > /etc/group  
  > /etc/gshadow

## groupdel

- usage:删除用户组

  > groupdel groupname

  groupdel \<groupname\>

- 注意事项：  
   只有当用户组的组成员为空时才可以删除该组

- 更新文件
  > /etc/group  
  > /etc/gshadow

## groupmod

- usage:修改用户组信息

  > groupmod [options] groupname

  groupadd -g\<new gid\> -n\<new groupname\> groupname

- 选项:  
   -g 设置新 gid  
   -n 设置新组名

- 更新文件
  > /etc/group  
  > /etc/gshadow  
  > /etc/passwd

_/etc/passwd 文件格式：_

> 用户名:口令:用户标识号:组标识号:注释性描述:主目录:登录 Shell

_/etc/shadow 文件格式：_

> 登录名:加密口令:最后一次修改时间:最小时间间隔:最大时间间隔:警告时间:不活动时间:失效时间:标志

_/etc/group 文件格式：_

> 组名:口令:组标识号:组内用户列表

_/etc/gshadow 文件格式：_

> 组名:组密码:组管理员名称:组成员
