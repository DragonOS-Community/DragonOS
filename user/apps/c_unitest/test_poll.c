#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/time.h>
#include <unistd.h>

int pipe_fd[2];         // 管道文件描述符数组
int child_can_exit = 0; // 子进程是否可以退出的标志
int signal_pid = 0;
int poll_errno; // poll错误码

#define WRITE_WAIT_SEC 3
#define POLL_TIMEOUT_SEC 5
#define EXPECTED_MESSAGE "Data is ready!\n"
#define POLL_DELTA_MS 1000
#define min(a, b) ((a) < (b) ? (a) : (b))

// 信号处理函数
void signal_handler(int signo) {
  printf("[PID: %d, TID: %lu] Signal %d received.\n", getpid(), pthread_self(),
         signo);
}

// 线程函数，用于在n秒后向管道写入数据
void *writer_thread(void *arg) {
  int seconds = WRITE_WAIT_SEC;
  for (int i = 0; i < seconds; i++) {
    printf("[PID: %d, TID: %lu] Waiting for %d seconds...\n", getpid(),
           pthread_self(), seconds - i);
    sleep(1);
    kill(signal_pid, SIGUSR1); // 发送信号
  }
  const char *message = EXPECTED_MESSAGE;
  write(pipe_fd[1], message, strlen(message)); // 写入管道
  printf("[PID: %d, TID: %lu] Data written to pipe.\n", getpid(),
         pthread_self());
  close(pipe_fd[1]); // 关闭写端
  printf("[PID: %d, TID: %lu] Pipe write end closed.\n", getpid(),
         pthread_self());

  while (child_can_exit == 0) {
    printf("[PID: %d, TID: %lu] Waiting for main to finish...\n", getpid(),
           pthread_self());
    sleep(1);
  }
  return NULL;
}

int main() {
  pthread_t tid;
  struct pollfd fds[1];
  int ret;
  int test_passed = 1; // 假设测试通过

  // 创建管道
  if (pipe(pipe_fd) == -1) {
    perror("pipe");
    exit(EXIT_FAILURE);
  }

  // 设置信号处理函数
  struct sigaction sa;
  sa.sa_handler = signal_handler;
  sigemptyset(&sa.sa_mask);
  sa.sa_flags = SA_RESTART;
  if (sigaction(SIGUSR1, &sa, NULL) == -1) {
    perror("sigaction");
    exit(EXIT_FAILURE);
  }

  signal_pid = getpid(); // 设置信号接收进程ID

  // 创建写线程
  if (pthread_create(&tid, NULL, writer_thread, NULL) != 0) {
    perror("pthread_create");
    exit(EXIT_FAILURE);
  }

  // 设置poll监视的文件描述符
  fds[0].fd = pipe_fd[0]; // 监视管道的读端
  fds[0].events = POLLIN; // 监视是否有数据可读

  printf("[PID: %d, TID: %lu] Waiting for data...\n", getpid(), pthread_self());

  // 在 poll 调用前后添加时间统计
  struct timeval start_time, end_time;
  gettimeofday(&start_time, NULL); // 记录 poll 开始时间

  ret = poll(fds, 1, POLL_TIMEOUT_SEC * 1000); // 调用 poll
  poll_errno = errno;
  gettimeofday(&end_time, NULL); // 记录 poll 结束时间

  // 计算 poll 的总耗时（单位：毫秒）
  long poll_duration_ms = (end_time.tv_sec - start_time.tv_sec) * 1000 +
                          (end_time.tv_usec - start_time.tv_usec) / 1000;

  if (abs((int)poll_duration_ms -
          min(POLL_TIMEOUT_SEC, WRITE_WAIT_SEC) * 1000) >= POLL_DELTA_MS) {
    printf("Poll duration: %ld ms, expected: %d ms, errno: %s\n",
           poll_duration_ms, POLL_TIMEOUT_SEC * 1000, strerror(poll_errno));
    test_passed = 0; // 测试失败（如果 poll 耗时与预期相差较大，认为测试未通过）
  }

  if (test_passed == 0) {

  } else if (ret == -1) {
    printf("poll errno: %s\n", strerror(poll_errno));
    test_passed = 0; // 测试失败
  } else if (ret == 0) {
    printf("Timeout! No data available.\n");
    test_passed = 0; // 测试失败
  } else {
    if (fds[0].revents & POLLIN) {
      char buffer[1024];
      ssize_t count = read(pipe_fd[0], buffer, sizeof(buffer)); // 读取数据
      if (count > 0) {
        printf("Data received: %s", buffer);
        // 检查读取的数据是否与预期一致
        if (strcmp(buffer, EXPECTED_MESSAGE) != 0) {
          printf("Unexpected data received.\n");
          test_passed = 0; // 测试失败
        }
      } else {
        printf("No data read from pipe.\n");
        test_passed = 0; // 测试失败
      }
    } else {
      printf("Unexpected event on pipe.\n");
      test_passed = 0; // 测试失败
    }
  }

  child_can_exit = 1; // 允许子进程退出
  // 等待写线程结束
  pthread_join(tid, NULL);
  close(pipe_fd[0]); // 关闭读端

  if (test_passed) {
    printf("Test passed!\n");
  } else {
    printf("Test failed!\n");
  }

  printf("Program finished.\n");

  return test_passed ? 0 : 1; // 返回0表示测试通过，返回1表示测试失败
}