# UNIX 

## unix socket

unix - 用于进程间通信的socket


## 描述

AF_UNIX socket family 用于在同一台机器中的不同进程之间的通信（IPC）。unix socket地址现支持绑定文件地址，未支持绑定abstract namespace抽象命名空间。

目前unix 域中合法的socket type有：SOCK_STREAM, 提供stream-oriented socket，可靠有序传输消息；SOCK_SEQPACKET，提供connection-oriented，消息边界和按发送顺序交付消息保证的socket。

### unix stream socket 进程通信描述

unix stream socket 提供进程间流式传输消息的功能。假设对端进程作为服务端，本端进程作为客户端。进程间使用stream socket通信过程如下：

分别在对端进程和本端进程创建socket，服务端需要bind地址，客户端不必须bind地址。通信过程类似tcp三次握手流程：服务端调用listen系统调用进入监听状态，监听服务端bind的地址；客户端调用connect系统调用连接服务端地址；服务端调用accept系统调用接受来自客户端的连接，返回建立连接的新的socket。成功建立连接后可以调用write\send\sendto\sendmsg进行写操作，调用read\recv\recvfrom\recvmsg进行读操作。目前尚未支持非阻塞式读写，默认为阻塞式读写。读写完毕后调用close系统调用关闭socket连接。

### unix seqpacket socket 进程通信描述


