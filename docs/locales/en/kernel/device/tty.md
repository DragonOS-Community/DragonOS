:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/device/tty.md

- Translation time: 2025-10-09 07:02:59

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Linux tty Devices

`dev/tty` is a very special device file in Linux and other Unix-like systems. Essentially, **it is an alias or shortcut pointing to the controlling terminal of the current process**.

### 1. Core Concept: Terminal

In the early days of computing, users interacted with computers through physical devices called "terminals." A typical physical terminal consisted of a keyboard for input and a screen (or printer) for output.

In modern Linux systems, physical terminals are uncommon, having been replaced by **terminal emulators (Terminal Emulator)** such as GNOME Terminal, Konsole, xterm, iTerm2, etc. These are software programs under graphical interfaces that simulate the behavior of physical terminals.

Additionally, there are **consoles (Console)**, which are terminals directly connected to computer hardware, typically used when there is no graphical interface or when the graphical interface has crashed. In Linux, you can switch to a virtual console using `Ctrl + Alt + F1-F6`.

Regardless of the form, the system manages these terminal sessions through a driver subsystem called **TTY**, named after the early "Teletypewriter."

### 2. TTY Device Files

In Linux, "everything is a file." The system communicates with hardware devices through special files in the `/dev` directory. For terminals, there is also a series of device files, typically located in the `/dev/` directory, such as:

- **/dev/ttyS0, /dev/ttyS1, ...**: Physical serial port devices.
- **/dev/tty1, /dev/tty2, ...**: Virtual consoles.
- **/dev/pts/0, /dev/pts/1, ...**: Pseudo-terminals. These are the most commonly used; when you open a terminal emulator window, the system creates a pseudo-terminal and assigns it a device file like `/dev/pts/0`.

Each terminal session (e.g., a terminal window you open) is associated with a specific TTY device file. You can use the `tty` command to view the device file corresponding to the current terminal:

Bash

```
$ tty
/dev/pts/0
```

### 3. The Role of `/dev/tty`: A Dynamic Link to the "Current" Terminal

Now let's return to the main subject, `/dev/tty`.

Imagine you are writing a program that needs to interact directly with the user (read the user's input or display information on the user's screen), regardless of where the program ultimately runs.

- If the user runs your program on virtual console `tty2`, the program should read from and write to `/dev/tty2`.
- If the user runs it in a GNOME Terminal window, the program may need to read from and write to `/dev/pts/5`.
- If the user logs in remotely via `ssh`, the program needs to read from and write to another pseudo-terminal device.

Having the program determine which terminal it is running on would be very complex and unreliable.

`/dev/tty` exists to solve this problem. **No matter what the specific device (e.g., `/dev/tty2` or `/dev/pts/5`) of a process's "controlling terminal" is, `/dev/tty` is always a link pointing to this controlling terminal.**

When a program opens the `/dev/tty` file, the kernel automatically redirects this file descriptor to the current process's actual controlling terminal. Thus, program developers do not need to worry about the underlying specific TTY device; they only need to uniformly read from and write to `/dev/tty`, ensuring interaction with the current user.

**In short, `/dev/tty` tells the program: "Send the information to the user who started you, no matter where they are."**

### 4. The Difference Between `/dev/tty` and Standard Input/Output/Error (stdin, stdout, stderr)

You might ask: This sounds similar to standard input (stdin) and standard output (stdout). What's the difference?

In most cases, a process's standard input, output, and error streams are by default connected to its controlling terminal. For example, when you run the `ls` command, its `stdout` is your terminal by default, so you can see the file list on the screen.

However, **redirection (Redirection)** changes this default behavior.

- `ls > files.txt`: The `stdout` of the `ls` command is redirected to the `files.txt` file instead of the terminal.
- `cat my_script.sh | bash`: The `stdin` of the `bash` process is redirected to a pipe (`|`), reading content from the output of the `cat` command instead of the keyboard.

In such cases, if the program still wants to forcibly interact with the user (for example, a script that requires the user to input a password), it can no longer rely on `stdin` or `stdout`, because they may have been redirected to a file or pipe, no longer the user's screen and keyboard.

**This is where `/dev/tty` comes into play.**

Reading from and writing to `/dev/tty` bypasses the redirection of standard input/output and directly accesses the controlling terminal.

#### Example:

Let's look at a practical example. Suppose we have a script `ask_password.sh`:

Bash

```
#!/bin/bash

# 尝试从标准输入读取密码
echo "Enter password (from stdin):"
read password_stdin

# 现在，强制从控制终端读取密码
echo "Enter password again (from /dev/tty):"
read password_tty < /dev/tty

echo "Password from stdin: $password_stdin"
echo "Password from tty: $password_tty"
```

Now, we run it normally:

Bash

```
$ ./ask_password.sh
Enter password (from stdin):
my_secret_pass
Enter password again (from /dev/tty):
my_secret_pass
Password from stdin: my_secret_pass
Password from tty: my_secret_pass
```

There seems to be no difference. But now, let's try running it with redirection:

Bash

```
$ echo "password_from_file" | ./ask_password.sh
Enter password (from stdin):
Enter password again (from /dev/tty):
my_real_secret  <-- 这里光标会停住，等待你从键盘输入
Password from stdin: password_from_file
Password from tty: my_real_secret
```

**Analysis:**

1. The first `read` command reads from `stdin`. Since we redirected the output of `echo` to the script's `stdin` via a pipe, it reads "password_from_file".
2. The second `read` command is explicitly redirected to read from `/dev/tty` (`< /dev/tty`). This operation bypasses the `stdin` pipe and directly accesses your keyboard and screen. Therefore, it will stop and wait for you to manually input the password.

This is the core value of `/dev/tty`: **providing a reliable channel to interact with the user's terminal, regardless of how standard streams are redirected.** Programs like `ssh` and `sudo` that require secure password input use this mechanism internally.

### Summary

| Feature                      | Description                                                  |
| ---------------------------- | ------------------------------------------------------------ |
| **Definition**               | A special device file that serves as an alias or shortcut for the current process's controlling terminal. |
| **Function**                 | Provides a stable and reliable way for programs to interact with the user's terminal that launched them. |
| **Dynamism**                 | It is not a specific device itself but a dynamically managed link by the kernel, pointing to the specific TTY device. |
| **Difference from stdin/stdout** | When standard input/output/error streams are redirected to files, pipes, or other processes, `/dev/tty` can still be used to directly access the user's screen and keyboard. |
| **Typical Uses**             | - Programs requiring user input of passwords or confirmations (e.g., `sudo`, `ssh`).<br>- Scripts needing explicit user interaction, even when run via pipes or redirection. |

### How is /dev/tty Typically Used in User Programs?

Well, the core goal of using `/dev/tty` in user programs is: **to bypass potentially redirected standard input/output streams and forcibly interact directly with the user's controlling terminal.**

This is very common in the following scenarios:

1. **Requesting Sensitive Information**: Such as passwords, private key passwords, etc. Even if a script's output is redirected to a log file, you do not want the password prompt and input process to be recorded.
2. **Interactive Confirmation**: In a script that may be automatically called, before performing dangerous operations (e.g., `rm -rf /`), it is necessary to force the user to manually confirm.
3. **Diagnostics and Debugging**: Printing debugging information to the user's screen, even if the user has redirected the script's standard output elsewhere.
4. **Fullscreen or Cursor-Based Applications**: Programs like `vim` and `top` need to directly control the terminal's screen, colors, and cursor positions, and they interact directly with the TTY device.

#### Summary

The pattern of using `/dev/tty` in user programs is very consistent:

1. **Open the File**: Open `/dev/tty` like a regular file, typically requiring read/write permissions (`r+`).
2. **Error Handling**: Check if the open operation was successful. If a process does not have a controlling terminal (e.g., a background daemon process started by `systemd`), opening `/dev/tty` will fail. The program needs to handle this situation properly.
3. **Writing (Output)**: Use standard file writing functions (e.g., `fprintf`, `write`) to write data to the opened `/dev/tty` file descriptor, which will display prompt information on the user's screen.
4. **Reading (Input)**: Use standard file reading functions (e.g., `fgets`, `read`) to read data from `/dev/tty`, obtaining the user's keyboard input.
5. **Close the File**: After completing the interaction, close the file descriptor.

## What is the Role of /dev/ptmx and Files Under /dev/pts/?

Well, let's delve into the files under `/dev/ptmx` and `/dev/pts/`. These two components are the core of the **pseudo-terminal (PTY)** mechanism in modern Linux systems, crucial for terminal emulators and SSH remote login functions we use daily.

Simply put, they together create a "fake" terminal device, making programs (e.g., `bash`) think they are communicating with a physical terminal, when in fact they are interacting with software (e.g., GNOME Terminal or `sshd`).

This mechanism consists of two parts:

- **Master Device**: Represented by `/dev/ptmx`.
- **Slave Device**: Located in the `/dev/pts/` directory, such as `/dev/pts/0`, `/dev/pts/1`, etc.

Let's delve deeper into their respective roles and how they work together.

### 1. The Concept of Pseudo-Terminals (PTY)

First, understand why pseudo-terminals are needed. In early Unix systems, users interacted with computers through physical serial ports (e.g., `/dev/ttyS0`) connected to physical terminals. Later, with the development of graphical interfaces and networks, we needed a way to simulate this hardware terminal at the software level.

Pseudo-terminals are this software simulation of terminals. They act like a pipe, with a "device" at each end:

- **Master Side**: Held and controlled by terminal emulators (e.g., xterm, GNOME Terminal) or remote login services (e.g., `sshd`).
- **Slave Side**: Provided for applications (e.g., `shell`, `vim`, `top`) to use. This slave device looks and behaves exactly like a true physical terminal device.

When you type on the keyboard in a terminal emulator, the terminal emulator program writes data from the master side; the kernel forwards this data to the slave side, and the `shell` program can read your input from the slave side. Conversely, when the `shell` program produces output (e.g., the result of `ls`), it writes data to the slave side; the kernel forwards it to the master side, and the terminal emulator reads this data and displays it in the window.

### 2. `/dev/ptmx`: The Pseudo-Terminal Master Multiplexer

`/dev/ptmx` is a special character device file, whose name is an abbreviation for "pseudo-terminal multiplexer." You can think of it as a **factory for creating pseudo-terminal master/slave device pairs**.

Its core functions are:

1. **Creating New PTY Pairs**: When a program (e.g., a terminal emulator) needs a new pseudo-terminal, it opens the `/dev/ptmx` file.
2. **Returning the Master Device File Descriptor**: This `open` operation successfully returns a file descriptor. This file descriptor represents the **master side** of the newly created PTY pair.
3. **Dynamically Creating the Slave Device**: While opening `/dev/ptmx`, the kernel dynamically creates a corresponding **slave device (Slave)** file in the `/dev/pts/` directory, such as `/dev/pts/0`.
4. **Providing a Control Interface**: The program can perform `ioctl()` system calls on the file descriptor returned by `/dev/ptmx` to configure the PTY, such as obtaining the name of the slave device, unlocking the slave device, etc.

**Key Point**: You cannot directly perform extensive read/write operations on `/dev/ptmx`. Its primary purpose is to request and create a new PTY pair through `open()` calls. Subsequent read/write operations are performed through the file descriptor returned by `open()`. Each opening of `/dev/ptmx` creates a brand new, independent PTY master/slave device pair.

### 3. The `/dev/pts/` Directory and Its Files

`/dev/pts` is a special file system of type `devpts`. This directory is specifically used to store pseudo-terminal **slave device** files.

- **Dynamically Created**: The files in this directory (e.g., `/dev/pts/0`, `/dev/pts/1`, ...) are not permanently present. They are dynamically created by the kernel when the corresponding PTY master device is created (i.e., when `/dev/ptmx` is opened).
- **Role of Slave Devices**: Each `/dev/pts/N` file acts as the slave side of a PTY pair. It is a standard TTY device, and applications (e.g., `bash`) can open it, read user input, and write program output just like any other terminal device.
- **Assigned to Shell**: After creating a PTY pair, the terminal emulator will `fork` a child process and redirect the standard input, output, and error of the child process to this newly created slave device (e.g., `/dev/pts/0`), then execute `bash` or another shell. Thus, `bash` "owns" this pseudo-terminal as its controlling terminal.

You can use the `tty` command to view the slave device associated with the current shell:

Bash

```
$ tty
/dev/pts/0
```

If you open a new terminal window and run `tty` in the new window, you might see:

Bash

```
$ tty
/dev/pts/1
```

### 4. Complete Creation Process

Let's string the entire process together to see what happens in the background when you open a new terminal window:

1. **Open ptmx**: The GNOME Terminal program calls `open("/dev/ptmx", O_RDWR)`.
2. **Create PTY Pair**: The kernel receives the request and creates a new pseudo-terminal master/slave device pair.
3. **Return Master Device FD**: The `open` call returns a file descriptor (e.g., `fd=3`) to GNOME Terminal. This `fd` is the master side of the PTY.
4. **Create Slave Device File**: Simultaneously, the kernel creates a new slave device file in the `/dev/pts/` directory, such as `/dev/pts/5`.
5. **Unlock and Authorize**: The GNOME Terminal program performs a series of `ioctl` calls (e.g., `grantpt` and `unlockpt`) on the file descriptor of the master device to set the permissions and status of the slave device `/dev/pts/5`, making it usable.
6. **Get Slave Device Name**: The GNOME Terminal queries the name of the slave device associated with the master device of `fd=3` through an `ioctl` call to `ptsname`, obtaining the string "/dev/pts/5".
7. **Create Child Process**: The GNOME Terminal calls `fork()` to create a child process.
8. **Set Session and Redirect**: In the child process:
   - A new session is created (`setsid()`), and `/dev/pts/5` is set as the controlling terminal of the session.
   - The standard input, output, and error (file descriptors 0, 1, 2) are closed.
   - `/dev/pts/5` is opened and duplicated to file descriptors 0, 1, 2. Now, the child process's `stdin`, `stdout`, and `stderr` all point to this pseudo-terminal slave device.
9. **Execute Shell**: The child process calls `execve("/bin/bash", ...)` to start `bash`. `bash` inherits the already set file descriptors, naturally reading commands from `/dev/pts/5` and writing results to it.
10. **Data Forwarding**:
    - You type `ls` in the GNOME Terminal window.
    - The GNOME Terminal program reads the input from keyboard events and **writes** "ls\n" through the file descriptor of the master device `fd=3`.
    - The kernel forwards the data from the master side to the slave side `/dev/pts/5`.
    - `bash` reads "ls\n" from its standard input (`/dev/pts/5`) and executes the command.
    - The output of `ls` is **written** by `bash` to its standard output (`/dev/pts/5`).
    - The kernel forwards the data from the slave side to the master side.
    - The GNOME Terminal program **reads** the output result of `ls` through the file descriptor of the master device `fd=3` and renders it in the window.

### Summary

| Component                     | Role                            | Function                                                     |
| ---------------------------- | ------------------------------- | ------------------------------------------------------------ |
| **`/dev/ptmx`**          | **Master Device Factory**       | As a single entry point, when opened, it creates a new pseudo-terminal master/slave device pair and returns the file descriptor representing the **master device**. |
| **Files Under `/dev/pts/`** | **Slave Devices**               | These are dynamically created device files, each representing the **slave device** of a pseudo-terminal. They provide a standard TTY interface for applications (e.g., shells) to interact as if with a physical terminal. |

### A Simple User Program Demonstrating the Use of /dev/ptmx

Well, here is a simple C language user program that clearly demonstrates how to use `/dev/ptmx` to create a pseudo-terminal and start a shell (`/bin/bash`) within this pseudo-terminal.

This program will act as the most basic "terminal emulator." It handles the master side, while the `bash` shell runs on the slave side it creates.

#### Program Function Breakdown

1. **Open `/dev/ptmx`**: Obtain a file descriptor for the pseudo-terminal master device.
2. **Initialize the Slave Device**: Call `grantpt()` and `unlockpt()` to set the permissions and status of the slave device (slave PTY).
3. **Get the Slave Device Name**: Call `ptsname()` to obtain the path of the corresponding slave device (e.g., `/dev/pts/3`).
4. **Create a Child Process**: Use `fork()` to create a child process, which will run the shell.
5. **Child Process Setup**: In the child process, associate its session with the slave device, redirect standard input, output, and error to the slave device, and then execute `/bin/bash`.
6. **Parent Process Communication**: In the parent process, listen to input from the user's current terminal and output from `bash`, transferring data back and forth between them to achieve interaction.

#### Source Code (`ptmx_demo.c`)

C

```
#define _XOPEN_SOURCE 600 // Needed for grantpt, unlockpt, ptsname
#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <sys/wait.h>
#include <termios.h> // Not strictly needed for the demo, but good practice
#include <string.h>

int main() {
    int master_fd;
    char *slave_name;
    pid_t pid;

    // 1. 打开 /dev/ptmx 来获取一个主设备文件描述符
    master_fd = open("/dev/ptmx", O_RDWR | O_NOCTTY);
    if (master_fd < 0) {
        perror("Error opening /dev/ptmx");
        return 1;
    }
    printf("1. Master PTY opened with fd: %d\n", master_fd);

    // 2. 授权并解锁从设备
    if (grantpt(master_fd) != 0) {
        perror("Error calling grantpt");
        close(master_fd);
        return 1;
    }
    if (unlockpt(master_fd) != 0) {
        perror("Error calling unlockpt");
        close(master_fd);
        return 1;
    }
    printf("2. Slave PTY permissions granted and unlocked.\n");

    // 3. 获取从设备的名字
    slave_name = ptsname(master_fd);
    if (slave_name == NULL) {
        perror("Error calling ptsname");
        close(master_fd);
        return 1;
    }
    printf("3. Slave PTY name is: %s\n", slave_name);

    // 4. 创建子进程
    pid = fork();
    if (pid < 0) {
        perror("Error calling fork");
        close(master_fd);
        return 1;
    }

    // 5. 子进程的代码
    if (pid == 0) {
        int slave_fd;

        // 创建一个新的会话，使子进程成为会话领导者
        // 这是让从设备成为控制终端的关键步骤
        if (setsid() < 0) {
            perror("setsid failed");
            exit(1);
        }

        // 打开从设备
        slave_fd = open(slave_name, O_RDWR);
        if (slave_fd < 0) {
            perror("Error opening slave pty");
            exit(1);
        }

        // 将从设备设置为该进程的控制终端
        // TIOCSCTTY 是 "Set Controlling TTY" 的意思
        if (ioctl(slave_fd, TIOCSCTTY, NULL) < 0) {
            perror("ioctl TIOCSCTTY failed");
            exit(1);
        }
        
        // 将子进程的标准输入、输出、错误重定向到从设备
        dup2(slave_fd, STDIN_FILENO);   // fd 0
        dup2(slave_fd, STDOUT_FILENO);  // fd 1
        dup2(slave_fd, STDERR_FILENO);  // fd 2

        // 关闭不再需要的文件描述符
        close(master_fd); // 子进程不需要主设备
        close(slave_fd);  // 因为已经 dup2 了，这个原始的也可以关了

        // 执行一个新的 bash shell
        printf("--- Starting Bash Shell in Slave PTY ---\n\n");
        fflush(stdout);
        execlp("/bin/bash", "bash", NULL);

        // 如果 execlp 成功，下面的代码不会被执行
        perror("execlp failed");
        exit(1);
    }

    // 6. 父进程的代码
    printf("4. Forked child process with PID: %d\n", pid);
    printf("5. Parent process will now forward data between stdin and master PTY.\n");
    printf("--- You are now interacting with the new shell. Type 'exit' to quit. ---\n\n");
    
    // 父进程不需要从设备
    // close(slave_fd) in parent - it was never opened here

    char buffer[256];
    ssize_t nread;

    // 循环，直到子进程退出
    while (1) {
        fd_set read_fds;
        FD_ZERO(&read_fds);
        FD_SET(STDIN_FILENO, &read_fds); // 监听当前终端的输入
        FD_SET(master_fd, &read_fds);    // 监听主设备的输出 (来自子进程shell)

        // 使用 select 阻塞，直到有数据可读
        if (select(master_fd + 1, &read_fds, NULL, NULL, NULL) < 0) {
            perror("select failed");
            break;
        }

        // 检查是否是当前终端有输入
        if (FD_ISSET(STDIN_FILENO, &read_fds)) {
            nread = read(STDIN_FILENO, buffer, sizeof(buffer));
            if (nread > 0) {
                // 将用户的输入写入主设备，数据会流向子进程的shell
                write(master_fd, buffer, nread);
            } else {
                break; // 读错误或EOF
            }
        }

        // 检查是否是主设备有输出
        if (FD_ISSET(master_fd, &read_fds)) {
            nread = read(master_fd, buffer, sizeof(buffer));
            if (nread > 0) {
                // 将来自shell的输出写入当前终端的屏幕
                write(STDOUT_FILENO, buffer, nread);
            } else {
                // 读取到 0 或 -1，意味着子进程的另一端关闭了连接
                // 通常是 shell 执行了 exit
                break;
            }
        }
    }

    printf("\n--- Shell terminated. Parent process is shutting down. ---\n");
    close(master_fd);
    wait(NULL); // 等待子进程完全终止

    return 0;
}
```

#### How to Compile and Run

1. **Save the Code**: Save the above code as `ptmx_demo.c`.

2. **Compile**: Compile using gcc.

   Bash

   ```_translated_label__
   gcc -o ptmx_demo ptmx_demo.c
   _en```_translated_label__

3. **Run**: Execute the generated executable file.

   Bash

   _en```_translated_label__
   ./ptmx_demo
   _en```

#### Runtime Output and Interaction

When you run the program, you will see output similar to the following:

```
1. Master PTY opened with fd: 3
2. Slave PTY permissions granted and unlocked.
3. Slave PTY name is: /dev/pts/2
4. Forked child process with PID: 12345
5. Parent process will now forward data between stdin and master PTY.
--- You are now interacting with the new shell. Type 'exit' to quit. ---

bash-5.1$
```

**What Happened?**

- Your `./ptmx_demo` program created a new pseudo-terminal `/dev/pts/2`.
- It started a new `bash` process, whose "world" is `/dev/pts/2`.
- Your `./ptmx_demo` program acts as an intermediary:
  - It reads keyboard input from your current terminal (e.g., `/dev/pts/1`).
  - Writes your input (e.g., `ls -l`) to the master device (`master_fd`).
  - The kernel forwards the data to the slave device `/dev/pts/2`.
  - `bash` reads `ls -l` from its standard input (`/dev/pts/2`) and executes it.
  - `ls`'s output is **written** by `bash` to its standard output (`/dev/pts/2`).
  - The kernel forwards the data to the master device.
  - Your `./ptmx_demo` program reads the result from the master device (`master_fd`) and prints it to its standard output, so you see the result of `ls -l` on the screen.

You can execute commands as in a normal shell:

```
bash-5.1$ pwd
/home/user/test
bash-5.1$ whoami
user
bash-5.1$ ps -f
UID          PID    PPID  C STIME TTY          TIME CMD
user       12344   5678  0 10:30 pts/1    00:00:00 ./ptmx_demo
user       12345  12344  0 10:30 pts/2    00:00:00 bash
user       12350  12345  0 10:31 pts/2    00:00:00 ps -f
```

Note the output of the `ps` command! Our `ptmx_demo` program runs on `pts/1` (your original terminal), while the `bash` process it creates runs in a completely new terminal `pts/2`.

When you type `exit` and press Enter, the `bash` process terminates, causing the pseudo-terminal connection to close. The `read(master_fd, ...)` in the parent process returns 0, the loop breaks, and the program exits gracefully.

This example fully demonstrates the core use of `/dev/ptmx`: creating an isolated terminal session environment and allowing a program (parent process) to fully control the input and output of another program (child process). This is also the foundational working principle of all terminal emulators, `ssh` services, and `tmux`/`screen` tools.
