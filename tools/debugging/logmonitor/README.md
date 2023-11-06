# 日志监视程序

本程序监视DragonOS内核的环形缓冲区日志，并将其显示在屏幕上。


## 使用方法

1. 默认情况下，DragonOS内核已启用内存分配器的日志记录。
2. 当qemu启动后，在DragonOS项目的根目录中，运行`make log-monitor`。
3. 在`logs`目录查看日志文件。
