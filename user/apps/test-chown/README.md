# 一个简单的用于测试chown系列系统调用的程序

### 由于symlink系统调用还未实现，目前只测试chown和fchown

### 测试前需要手动添加nogroup用户组和nobody用户
```groupadd -g 65534 nogroup
useradd -d /nonexistent -g 65534 -u 65534 -s /bin/false nobody
```

### /bin/false是个不可执行文件，用于测试chown系列系统调用时，文件权限的修改不会生效。
```
#!/bin/bash
exit 1
```