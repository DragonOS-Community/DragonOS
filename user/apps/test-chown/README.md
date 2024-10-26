# 一个简单的用于测试chown系列系统调用的程序

### 由于symlink系统调用还未实现，目前只测试chown和fchown

### 测试前需要手动添加nogroup用户组和nobody用户（程序里加不了）
```groupadd -g 65534 nogroup
useradd -d /nonexistent -g 65534 -u 65534 -s /usr/local/bin/false nobody
```
