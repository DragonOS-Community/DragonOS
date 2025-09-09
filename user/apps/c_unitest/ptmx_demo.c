#define _XOPEN_SOURCE 600 // Needed for grantpt, unlockpt, ptsname
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/select.h>
#include <sys/wait.h>
#include <termios.h> // Not strictly needed for the demo, but good practice
#include <unistd.h>

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
    dup2(slave_fd, STDIN_FILENO);  // fd 0
    dup2(slave_fd, STDOUT_FILENO); // fd 1
    dup2(slave_fd, STDERR_FILENO); // fd 2

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
  printf("5. Parent process will now forward data between stdin and master "
         "PTY.\n");
  printf("--- You are now interacting with the new shell. Type 'exit' to quit. "
         "---\n\n");

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