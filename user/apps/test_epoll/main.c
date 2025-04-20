#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <unistd.h>

#define MAX_EVENTS 10

static int efd;  // eventfd 描述符
static int efd2; // eventfd 描述符

// 工作线程：等待2秒后向 eventfd 写入事件通知
void *worker_thread(void *arg) {
  uint64_t u = 1;
  printf("工作线程：等待2秒后发送事件通知...\n");
  sleep(2); // 模拟耗时任务
  printf("工作线程：发送事件通知...\n");
  if (write(efd, &u, sizeof(u)) != sizeof(u)) {
    perror("工作线程写入 eventfd 出错");
    exit(EXIT_FAILURE);
  }
  printf("工作线程：事件通知已发送\n");
  return NULL;
}

int main() {
  int epoll_fd;
  struct epoll_event ev, events[MAX_EVENTS];
  int nfds;
  pthread_t tid;

  // 创建 eventfd，对象初始计数为 0
  efd = eventfd(0, 0);
  if (efd == -1) {
    perror("创建 eventfd 失败");
    exit(EXIT_FAILURE);
  } else {
    printf("创建 eventfd 成功，描述符 = %d\n", efd);
  }

  efd2 = dup(efd); // 复制 eventfd 描述符
  if (efd2 == -1) {
    perror("复制 eventfd 失败");
    close(efd);
    exit(EXIT_FAILURE);
  } else {
    printf("复制 eventfd 成功，描述符 = %d\n", efd2);
  }

  // 创建 epoll 实例
  epoll_fd = epoll_create1(0);
  if (epoll_fd == -1) {
    perror("创建 epoll 实例失败");
    close(efd);
    exit(EXIT_FAILURE);
  }

  // 将 eventfd 添加到 epoll 监听队列，关注可读事件
  ev.events = EPOLLIN;
  ev.data.fd = efd;
  if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, efd, &ev) == -1) {
    perror("epoll_ctl 添加 eventfd 失败");
    close(efd);
    close(epoll_fd);
    exit(EXIT_FAILURE);
  }

  // 将复制的 eventfd 添加到 epoll 监听队列，关注可读事件
  ev.data.fd = efd2;
  if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, efd2, &ev) == -1) {
    perror("epoll_ctl 添加复制的 eventfd 失败");
    close(efd);
    close(efd2);
    close(epoll_fd);
    exit(EXIT_FAILURE);
  }

  // 创建工作线程，模拟事件发生
  if (pthread_create(&tid, NULL, worker_thread, NULL) != 0) {
    perror("创建工作线程失败");
    close(efd);
    close(efd2);
    close(epoll_fd);
    exit(EXIT_FAILURE);
  }

  printf("主线程：使用 epoll_wait 等待事件...\n");

  // 阻塞等待事件发生
  nfds = epoll_wait(epoll_fd, events, MAX_EVENTS, -1);
  if (nfds == -1) {
    perror("epoll_wait 失败");
    exit(EXIT_FAILURE);
  } else {
    printf("主线程：epoll_wait 返回，事件数量 = %d\n", nfds);
  }

  // 处理就绪事件
  //   for (int i = 0; i < nfds; i++) {
  //     if (events[i].data.fd == efd || events[i].data.fd == efd2) {
  //       uint64_t count;
  //       int fd = events[i].data.fd;
  //       printf("主线程：事件发生在 fd = %d\n", fd);
  //       if (read(fd, &count, sizeof(count)) != sizeof(count)) {
  //         perror("从 eventfd 读取失败");
  //         exit(EXIT_FAILURE);
  //       }
  //       printf("主线程：接收到 eventfd 事件，计数值 = %lu\n", count);
  //     }
  //   }

  // 等待工作线程结束
  pthread_join(tid, NULL);

  int r = close(epoll_fd);
  if (r == -1) {
    perror("关闭 epoll 实例失败");
    exit(EXIT_FAILURE);
  } else {
    printf("关闭 epoll 实例成功\n");
  }
  close(efd);
  close(efd2); // 关闭复制的 eventfd 描述符
  printf("test_epoll ok\n");
  return 0;
}