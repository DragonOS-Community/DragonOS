**usemod**

- usage：修改用户

  > usermod [options] username

  usermod -a -G<组1,组2,...> -c<备注> -d<登入目录> -g<群组> -l<名称> -s<登入终端> -u<用户 id> username

- 选项:  
   -a -G<组1,组2,...> 将用户添加到其它组中  
   -c<备注> 　修改用户帐号的备注文字。  
   -d 登入目录> 　修改用户登入时的目录。  
   -g<组名> 　修改用户所属的群组。  
   -l<名称> 　修改用户名称。  
   -s\<shell\> 　修改用户登入后所使用的 shell。  
   -u\<uid\> 　修改用户 ID。

- 更新文件：
  > /etc/passwd  
  > /etc/shadow  
  > /etc/group  
  > /etc/gshadow
