:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/net/ssh.md

- Translation time: 2025-11-22 06:51:06

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# SSH Support

Currently, we use a lightweight SSH implementation [dropbear](https://matt.ucc.asn.au/dropbear/dropbear.html).

## Initialization Steps

1. Ensure /bin/sh exists (it should already exist, actually busybox; if not, you can copy the busybox file from the sysroot/bin directory: `cp ./bin/sysroot/bin/busybox ./bin/sysroot/bin/sh`)
2. Ensure the /root directory exists (it should already exist)
3. Ensure the /etc/dropbear directory exists
   ```_translated_label__shell
   mkdir /etc/dropbear
   _en```_translated_label__
4. Ensure the /var/log/lastlog file exists (can be an empty file) (not required)
5. Change the root user's password
    _en```_translated_label__shell
    busybox passwd
    _en```_translated_label__
6. Start the dropbear server after the system boots
    _en```_translated_label__shell
    dropbear -E -F -R -p 12580
    _en```_translated_label__

7. Connect to the server from a local terminal
    _en```_translated_label__shell
    ./dbclient -p 12580 root@localhos
    _en```_translated_label__
    If you encounter an error:
    _en```_translated_label__shell
    ./dbclient: Connection to root@localhost:12580 exited:

    ssh-ed25519 host key mismatch for localhost !
    Fingerprint is SHA256:hK2KR5QQnSHpcHLYzCVe9IyKEw1NwIXx41K/gyv7NKI
    Expected SHA256:W8+kSk+aCm2uoc1ZIKU/RQJSKoqWrOKrFf9URhfFaw8
    If you know that the host key is correct you can
    remove the bad entry from ~/.ssh/known_hosts
    _en```_translated_label__
    Solution: `ssh-keygen -R localhost` Delete the existing incorrect host key
8. Enter the root user's password, and after successful login, you can use the SSH terminal
9. Exit the SSH terminal
    _en```_translated_label__shell
    exit
    _en```

## Other Available Commands

Generate SSH key pair
```shell
# 生成dropbear的ssh密钥对
dropbearkey -t rsa -f ~/.ssh/id_dropbear
```

Add the public key to the authorized list
```shell
# 拷贝公钥到指定目录
cp ~/.ssh/id_dropbear.pub ./bin/sysroot

# 使用私钥连接到服务器
./dbclient -i ~/.ssh/id_dropbear -p 12580 root@localhost
```

## Introduction to SSH-related System Files

### Purpose of /etc/passwd

- Stores system user information: It contains basic information about each user and is an important data source for system authentication and login.

- Used for login and user management: The system and applications read this file to verify user identity, determine user permissions, and load corresponding settings (such as default shell, home directory, etc.).

- Provides support: For some applications and commands (such as useradd, passwd, chown), it needs to obtain user-related information through /etc/passwd.

```bash
username:password:UID:GID:GECOS:home_directory:shell
```

- username (username): The user's login name. For example: root, john, guest, etc.

- password (password): The user's encrypted password. Nowadays, most systems store the hash value of the password in the /etc/shadow file, so this field is usually a placeholder (such as x or *). In older systems, the password might be stored directly in this field, but this practice is no longer secure.

- UID (user ID): The user's unique identifier. Each user has a unique UID, and the system uses it to distinguish different users. Typically:

- 0 represents the root user (superuser).

- Ordinary users' UIDs usually start from 1000 (in some Linux distributions, they may start from 500).

- GID (group ID): The ID of the user's primary group. The group ID is a number associated with the group name. Typically, each user will have a group with the same name as their username. For example, the john user might have a primary group john, whose GID might be 1001.

- GECOS (user's full name or remark information): This field usually stores optional information such as the user's full name, phone number, etc. It can be empty or contain some descriptive text, usually set via the chfn command.

- home_directory (home directory): The user's login home directory, where the user will be automatically taken after logging in. For example: /home/john, /root. If the user is root, their home directory is usually /root.

- shell (default shell): The shell program used when the user logs in. Usually /bin/bash, /bin/sh, or other shell programs. For system users without actual login permissions, this field might be /usr/sbin/nologin or /bin/false to prevent them from logging into the system.

### Purpose of /etc/shadow

- Stores users' encrypted passwords: The /etc/shadow file saves each user's encrypted password, not the plaintext password. This is a system security mechanism to prevent password leakage.

- Account expiration management: It also contains information related to account expiration, password expiration, account locking, etc., helping system administrators manage users' login permissions.

- Enhances security: Compared to early systems that stored passwords directly in /etc/passwd, /etc/shadow removes passwords from publicly accessible files, making the system more secure.

```bash
username:password:lastchg:min:max:warn:inactive:expire:flag
```

- username (username): Consistent with the username in /etc/passwd.

- password (password): This is the user's encrypted password. If the password is empty, it is usually * or !, indicating the account is disabled. Normally, this stores the encrypted hash value of the password.

- lastchg (last change date): The date of the last password change, representing the number of days since January 1, 1970. This value is usually viewed and updated via the chage command.

- min (minimum password age): The minimum period the password must be used. Users must wait how many days after changing the password before they can change it again. Usually set to 0, indicating no minimum password age limit.

- max (maximum password age): The maximum period the password can be used. After this period, users must change the password. Set to 99999 to indicate the password never expires.

- warn (warning period): How many days before the password expires the system will start warning the user that the password is about to expire.

- inactive (inactive period): How many days the user can still log in after the password expires. If this period is exceeded, the account will be disabled.

- expire (account expiration date): The account's expiration date, representing the number of days since January 1, 1970. If the account expires, the user will not be able to log in.

- flag (account lock flag): This field is used to store whether the account is locked. If this field is !! or *, it indicates the user account is locked and cannot log in.

### Password Prefixes Generated by Different Hash Algorithms

- `\$5$` prefix (SHA-256)
- `\$6$` prefix (SHA-512)
- `\$y$` prefix (Yarrow)

### System Call Support

- fcntl SETLK https://man7.org/linux/man-pages/man2/fcntl.2.html
- unlink
- fsync
- rename https://man7.org/linux/man-pages/man2/rename.2.html
- ioctl TCFLSH
- renameat2: oldfd: -100, filename_from: /etc/shadow+, newfd: -100, filename_to: /etc/shadow failed

### File System Related
/proc/self directory: /proc/self is a symbolic link that always points to the /proc/[pid] directory of the accessing process itself.

| Path                 | Purpose                                     |
| -------------------- | ---------------------------------------- |
| `/proc/self/cmdline` | Command-line arguments of the current process                     |
| `/proc/self/exe`     | Executable file path of the current process (a symbolic link) |
| `/proc/self/fd/`     | All file descriptors opened by the current process             |
| `/proc/self/environ` | Environment variables of the current process                       |
| `/proc/self/maps`    | Memory mapping layout of the current process                   |
| `/proc/self/status`  | Status information of the current process (similar to the ps command)     |

- /proc/fd/{id} is also a symbolic link, pointing to the file path corresponding to the file descriptor opened by the process.
- /dev/pts/0 These pseudo-terminals need to wait for the main device /dev/ptmx to be closed before deletion
- The /root directory must exist

### What are the roles and usage of linux's setgroups and getgroups?

In Linux, a process has:

- Real user ID (real UID), effective user ID (effective UID)
- Real group ID (real GID), effective group ID (effective GID)
- Supplementary group ID list (supplementary groups)

The supplementary group ID allows the process to belong to multiple other groups in addition to the primary group, thereby gaining access permissions corresponding to those groups.

#### User ID (UID) and Group ID (GID)
These are permission identity identifiers used to determine what a process can do.

UID (User ID) indicates which user the process belongs to.
Common types:

- Real user ID (real UID): Who started the process.
- Effective user ID (effective UID): The UID actually used for permission checks (for example, a setuid program can temporarily make EUID root).
- Saved set-user-ID (saves the EUID before switching, used for temporary restoration).

The UID of the root user is 0, and other users are generally assigned starting from 1000 (or 500).

GID (Group ID) indicates which primary user group the process belongs to. There are also real/effective/saved types.
Additionally, there is a supplementary group list (supplementary groups), used to grant additional group permissions.

Role:
When a process accesses resources such as files, sockets, IPC, etc., the kernel determines access based on EUID/EGID + supplementary group list.

## Reference
- Linux TTY/PTS Overview https://liujunming.top/2019/09/03/Linux-TTY-PTS%E6%A6%82%E8%BF%B0/
- Pseudo Terminal (pseudo terminal) https://zhuanlan.zhihu.com/p/678170056
- Hardware Terminal terminal (TTY)
 https://www.cnblogs.com/sparkdev/p/11460821.html
