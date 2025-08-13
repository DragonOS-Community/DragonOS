#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define BUFFER_SIZE 1024

#define MSG "Hello from parent!\n"

static int handled_signal = 0;
// 子进程的信号处理函数
void child_signal_handler(int sig) {
  printf("Child received signal %d\n", sig);
  handled_signal = 1;
}

// 父进程的信号处理函数
void parent_signal_handler(int sig) {
  printf("Parent received signal %d\n", sig);
}

int main() {
  int pipefd[2];
  pid_t pid;
  char buffer[BUFFER_SIZE];

  // 创建管道
  if (pipe(pipefd) == -1) {
    perror("pipe");
    exit(EXIT_FAILURE);
  }

  // 创建子进程
  pid = fork();
  if (pid == -1) {
    perror("fork");
    exit(EXIT_FAILURE);
  }

  if (pid == 0) {
    // 子进程
    close(pipefd[1]); // 关闭写端

    // 设置子进程的信号处理函数
    signal(SIGUSR1, child_signal_handler);

    printf("Child: Waiting for data...\n");

    // 尝试从管道中读取数据
    ssize_t bytes_read = read(pipefd[0], buffer, BUFFER_SIZE - 1);
    if (bytes_read == -1) {
      printf("[FAILED]: Child: read error, errno=%d\n", errno);
      exit(EXIT_FAILURE);
    } else if (bytes_read == 0) {
      printf("Child: End of file\n");
    }

    if (bytes_read != sizeof(MSG) - 1) {
      printf("[FAILED]: Child: read error: got %ld bytes, expected %ld\n",
             bytes_read, sizeof(MSG) - 1);
    } else {
      printf("[PASS]: Child: read success: got %ld bytes, expected %ld\n",
             bytes_read, sizeof(MSG) - 1);
    }

    buffer[bytes_read] = '\0';
    printf("Child: Received message: %s", buffer);

    close(pipefd[0]);

    if (!handled_signal)
      printf("[FAILED]: Parent: child did not handle signal\n");
    else
      printf("[PASS]: Parent: child handled signal\n");
    exit(EXIT_SUCCESS);
  } else {
    // 父进程
    close(pipefd[0]); // 关闭读端

    // 设置父进程的信号处理函数
    signal(SIGCHLD, parent_signal_handler);

    // 发送信号给子进程，中断它的读操作
    sleep(1); // 确保子进程已经开始读取
    // printf("Parent: Sending SIGCHLD to child...\n");
    // kill(pid, SIGCHLD);
    printf("Parent: Sending SIGUSR1 to child...\n");
    kill(pid, SIGUSR1);
    sleep(1); // 确保子进程已经处理了信号

    write(pipefd[1], MSG, strlen(MSG));

    printf("Parent: Sent message: %s", MSG);

    // 等待子进程结束
    waitpid(pid, NULL, 0);

    printf("Parent: Child process finished.\n");

    close(pipefd[1]);
    exit(EXIT_SUCCESS);
  }
}