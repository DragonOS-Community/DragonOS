# SSH
## /etc/passwd 的作用

- 存储系统用户信息：它包含了每个用户的基本信息，是系统认证和登录的重要数据来源。

- 用于登录和用户管理：系统和应用程序通过读取该文件来验证用户身份、决定用户权限以及加载相应的设置（如默认 shell、主目录等）。

- 提供支持：对于一些应用程序和命令（如 useradd、passwd、chown），它需要通过 /etc/passwd 来获取用户的相关信息。

```bash
username:password:UID:GID:GECOS:home_directory:shell
```

- username（用户名）：用户的登录名称。例如：root、john、guest 等。

- password（密码）：用户的加密密码。现在大多数系统会将密码的哈希值存储在 /etc/shadow 文件中，因此这个字段通常是一个占位符（如 x 或 *）。在较旧的系统中，密码可能直接存储在此字段中，但这种做法已不安全。

- UID（用户 ID）：用户的唯一标识符。每个用户都有一个唯一的 UID，系统通过它来区分不同用户。通常：

- 0 表示 root 用户（超级用户）。

- 普通用户的 UID 从 1000 开始（在一些 Linux 发行版中，可能从 500 开始）。

- GID（组 ID）：用户所属的主组的 ID。组 ID 是与组名称相关联的数字。通常，每个用户都会有一个与其用户名相同的组。例如，john 用户可能会有一个主组 john，其 GID 可能是 1001。

- GECOS（用户全名或备注信息）：这个字段通常存储用户的全名、电话号码等可选信息。它可以为空或包含一些描述性文字，通常是通过 chfn 命令进行设置。

- home_directory（主目录）：用户的登录主目录，用户登录后会被自动带到这个目录。例如：/home/john、/root。如果用户是 root，其主目录通常是 /root。

- shell（默认 shell）：用户登录时所使用的 shell 程序。通常是 /bin/bash、/bin/sh 或其他 shell 程序。对于没有实际登录权限的系统用户，这个字段可能是 /usr/sbin/nologin 或 /bin/false，以阻止他们登录系统


## /etc/shadow 文件的作用

- 存储用户的加密密码：/etc/shadow 文件中保存了每个用户的 加密密码，而不是明文密码。这是一个系统的安全机制，防止密码泄漏。

- 账户过期管理：它还包含了与账户过期、密码过期、账户锁定等相关的信息，帮助系统管理员管理用户的登录权限。

- 增强安全性：相比于早期系统中把密码直接存储在 /etc/passwd 中，/etc/shadow 将密码从可公开访问的文件中移除，使得系统更加安全

```bash
username:password:lastchg:min:max:warn:inactive:expire:flag
```

- username（用户名）：与 /etc/passwd 中的用户名一致。

- password（密码）：这是用户的加密密码。如果密码为空，通常是 * 或 !，表示禁用该账户。正常情况下，这里存储的是密码的加密哈希值。

- lastchg（上次修改日期）：密码最后一次修改的日期，表示自 1970 年 1 月 1 日以来的天数。通常这个值是通过 chage 命令查看和更新的。

- min（最小密码年龄）：密码的最小使用期限。用户修改密码后，必须等待多少天才能再次修改密码。通常设置为 0，表示没有最小密码年龄限制。

- max（最大密码年龄）：密码的最大使用期限。超过这个期限，用户必须修改密码。设置为 99999 表示密码永不过期。

- warn（警告期限）：密码过期前，系统会提前多少天开始警告用户密码即将过期。

- inactive（非活动期限）：密码过期后，用户仍然有多少天的时间可以继续登录。如果超过该天数，账户将被禁用。

- expire（账户过期日期）：账户的过期日期，表示自 1970 年 1 月 1 日以来的天数。如果账户过期，用户将无法登录。

- flag（账户锁定标志）：这个字段用于存储账户是否被锁定。如果这个字段是 !! 或 *，表示用户账号被锁定，不能登录

## 不同hash算法生成的密码前缀

- \$5$ 前缀（SHA-256）
- \$6$ 前缀（SHA-512）
- \$y$ 前缀（Yarrow）



## dropbear需求

1. /etc/passwd 文件
2. /etc/shadow 文件

由于DragonOS默认的/etc/passwd文件有问题，需要修改为

```bash
root:x:0:0:root:/root:/bin/sh
```

然后使用`busybox passwd` 设置root密码




## 系统调用支持

- fcntl SETLK https://man7.org/linux/man-pages/man2/fcntl.2.html
- unlink
- fsync
- rename https://man7.org/linux/man-pages/man2/rename.2.html
- ioctl TCFLSH
- renameat2: oldfd: -100, filename_from: /etc/shadow+, newfd: -100, filename_to: /etc/shadow 失败



## 文件系统相关
/proc/self目录:/proc/self 是一个 符号链接，始终指向 访问它的进程自己的 /proc/[pid] 目录

| 路径                   | 作用                   |
| -------------------- | -------------------- |
| `/proc/self/cmdline` | 当前进程的命令行参数           |
| `/proc/self/exe`     | 当前进程的可执行文件路径（是个符号链接） |
| `/proc/self/fd/`     | 当前进程打开的所有文件描述符       |
| `/proc/self/environ` | 当前进程的环境变量            |
| `/proc/self/maps`    | 当前进程的内存映射布局          |
| `/proc/self/status`  | 当前进程的状态信息（类似于 ps 命令） |


- /proc/fd/{id} 也是一个符号链接，指向进程打开的文件描述符所对应的文件路径。
- /dev/pts/0 这些伪终端需要等待主设备/dev/ptmx被关闭的时候删除
- /root目录必须存在

### linux的setgroups和getgroups的作用和用法是什么

在 Linux 中，一个进程有：

- 真实用户 ID (real UID)、有效用户 ID (effective UID)
- 真实组 ID (real GID)、有效组 ID (effective GID)
- 附加组 ID 列表（supplementary groups）

附加组 ID 让进程除了主组外，还可以属于其他多个组，从而获得对应组的访问权限



#### 用户 ID (UID) 和 组 ID (GID)
这两个是权限身份标识，用来决定一个进程能做什么。

UID（User ID）表示进程属于哪个用户。
常见种类：

- 真实用户 ID (real UID)：启动该进程的用户是谁。
- 有效用户 ID (effective UID)：实际用于权限检查的 UID（比如 setuid 程序可以让 EUID 暂时变成 root）。
- 保存的 set-user-ID（保存切换前的 EUID，用于临时恢复）。

root 用户的 UID 是 0，其它用户一般是从 1000（或 500）开始分配。

GID（Group ID） 表示进程属于哪个主用户组。同样有 real/effective/saved 三种。
另外还有 附加组列表（supplementary groups），用来赋予额外的组权限。

作用：
当进程访问文件、socket、IPC 等资源时，内核会根据 EUID/EGID + 附加组列表 来判断能否访问。




## Reference
- Linux TTY/PTS概述 https://liujunming.top/2019/09/03/Linux-TTY-PTS%E6%A6%82%E8%BF%B0/
- 伪终端(pseudo terminal) https://zhuanlan.zhihu.com/p/678170056
- 硬件终端 terminal(TTY)
 https://www.cnblogs.com/sparkdev/p/11460821.html