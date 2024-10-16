# 测试Symlink系统调用

由于FAT32不支持符号链接，如需要进行测试的话得先挂载一个`ramfs`到根目录下(命名为`myramfs`)，然后在myramfs目录下创建名为`another`的目录，最后运行`test-symlink`即可