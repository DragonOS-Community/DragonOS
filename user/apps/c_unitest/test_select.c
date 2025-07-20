#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/select.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// 创建 eventfd 并返回 fd
int create_eventfd() {
  int fd = eventfd(0, EFD_NONBLOCK);
  if (fd == -1) {
    perror("eventfd");
    exit(EXIT_FAILURE);
  }
  return fd;
}

// 子线程或子进程模拟事件发生
void trigger_event(int efd, unsigned int delay_sec) {
  printf("[trigger] Writing eventfd after %u seconds...\n", delay_sec);
  sleep(delay_sec);
  uint64_t val = 1;
  if (write(efd, &val, sizeof(val)) != sizeof(val)) {
    perror("write eventfd");
    exit(EXIT_FAILURE);
  }
  printf("[trigger] Event written to eventfd.\n");
}

int main() {
  int efd = create_eventfd();

  pid_t pid = fork();
  if (pid < 0) {
    perror("fork");
    exit(1);
  }

  if (pid == 0) {
    // 子进程：触发事件
    trigger_event(efd, 3);
    close(efd);
    exit(0);
  }

  // 父进程：使用 select 等待事件发生
  printf("[select_test] Waiting for event...\n");

  fd_set rfds;
  FD_ZERO(&rfds);
  FD_SET(efd, &rfds);

  int maxfd = efd + 1;
  struct timeval timeout;
  timeout.tv_sec = 5;
  timeout.tv_usec = 0;

  int ret = select(maxfd, &rfds, NULL, NULL, &timeout);
  if (ret < 0) {
    perror("select");
    exit(1);
  }
  printf("[select_test] select returned: %d\n", ret);

  if (FD_ISSET(efd, &rfds)) {
    printf("[select_test] Event occurred on eventfd.\n");
    uint64_t val;
    if (read(efd, &val, sizeof(val)) != sizeof(val)) {
      perror("read");
      exit(1);
    }
    printf("[select_test] Received eventfd value: %lu\n", val);
  }

  // wait for child process to finish
  int status;
  waitpid(pid, &status, 0);
  printf("[parent] Child exited with status: %d\n", WEXITSTATUS(status));
  close(efd);

  return 0;
}
