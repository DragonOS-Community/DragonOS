#include <errno.h>
#define _GNU_SOURCE

#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/poll.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define RED "\x1B[31m"
#define GREEN "\x1B[32m"
#define RESET "\x1B[0m"

// 测试用例1：基本功能测试（管道I/O）
void test_basic_functionality() {
  int pipefd[2];
  struct pollfd fds[1];
  struct timespec timeout = {5, 0}; // 5秒超时

  printf("=== Test 1: Basic functionality test ===\n");

  // 创建管道
  if (pipe(pipefd) == -1) {
    perror("pipe creation failed");
    exit(EXIT_FAILURE);
  }

  // 设置监听读端管道
  fds[0].fd = pipefd[0];
  fds[0].events = POLLIN;

  printf("Test scenario 1: Wait with no data (should timeout)\n");
  int ret = ppoll(fds, 1, &timeout, NULL);
  if (ret == 0) {
    printf(GREEN "Test passed: Correct timeout\n" RESET);
  } else {
    printf(RED "Test failed: Return value %d\n" RESET, ret);
  }

  // 向管道写入数据
  const char *msg = "test data";
  write(pipefd[1], msg, strlen(msg));

  printf(
      "\nTest scenario 2: Should return immediately when data is available\n");
  timeout.tv_sec = 5;
  ret = ppoll(fds, 1, &timeout, NULL);
  if (ret > 0 && (fds[0].revents & POLLIN)) {
    printf(GREEN "Test passed: Data detected\n" RESET);
  } else {
    printf(RED "Test failed: Return value %d, revents %d\n" RESET, ret,
           fds[0].revents);
  }

  close(pipefd[0]);
  close(pipefd[1]);
}

// 测试用例2：信号屏蔽测试
void test_signal_handling() {
  printf("\n=== Test 2: Signal handling test ===\n");
  sigset_t mask, orig_mask;
  struct timespec timeout = {5, 0};
  struct pollfd fds[1];

  fds[0].fd = -1;
  fds[0].events = 0;

  // 设置信号屏蔽
  sigemptyset(&mask);
  sigaddset(&mask, SIGUSR1);
  // 阻塞SIGUSR1，并保存原来的信号掩码
  if (sigprocmask(SIG_BLOCK, &mask, &orig_mask)) {
    perror("sigprocmask");
    exit(EXIT_FAILURE);
  }

  printf("Test scenario: Signal should not interrupt when masked\n");
  pid_t pid = fork();
  if (pid == 0) { // 子进程
    sleep(2);     // 等待父进程进入ppoll
    kill(getppid(), SIGUSR1);
    exit(0);
  }

  int ret = ppoll(fds, 1, &timeout, &mask);

  if (ret == 0) {
    printf(GREEN "Test passed: Completed full 5 second wait\n" RESET);
  } else {
    printf(RED "Test failed: Premature return %d\n" RESET, errno);
  }

  waitpid(pid, NULL, 0);

  // 检查并消费挂起的SIGUSR1信号
  sigset_t pending;
  sigpending(&pending);
  if (sigismember(&pending, SIGUSR1)) {
    int sig;
    sigwait(&mask, &sig); // 主动消费信号

    printf("Consumed pending SIGUSR1 signal\n");
  }
  // 恢复原来的信号掩码
  sigprocmask(SIG_SETMASK, &orig_mask, NULL);
}

// 测试用例3：精确超时测试
void test_timeout_accuracy() {
  printf("\n=== Test 3: Timeout accuracy test ===\n");
  struct timespec start, end, timeout = {0, 500000000};
  struct pollfd fds[1];
  fds[0].fd = -1;
  fds[0].events = 0;

  clock_gettime(CLOCK_MONOTONIC, &start);
  int ret = ppoll(fds, 1, &timeout, NULL);
  clock_gettime(CLOCK_MONOTONIC, &end);

  long elapsed = (end.tv_sec - start.tv_sec) * 1000000 +
                 (end.tv_nsec - start.tv_nsec) / 1000;

  printf("Expected timeout: 500ms, Actual elapsed: %.3fms\n", elapsed / 1000.0);
  if (labs(elapsed - 500000) < 50000) { // 允许±50ms误差
    printf(GREEN "Test passed: Timeout within acceptable range\n" RESET);
  } else {
    printf(RED "Test failed: Timeout deviation too large\n" RESET);
  }
}

int main() {
  // 设置非阻塞标准输入
  fcntl(STDIN_FILENO, F_SETFL, O_NONBLOCK);

  test_basic_functionality();
  test_signal_handling();
  test_timeout_accuracy();

  return 0;
}
