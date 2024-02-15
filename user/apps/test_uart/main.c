#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main() {
  // 打开设备文件
  int fd = open("/dev/char/uart:1088", O_WRONLY | O_NONBLOCK);
  char buf[1] = {0};
  int n;
  memset(buf, 0, 1);
  while (1) {
    n = read(fd, buf, 1);
    close(fd);
    fd = open("/dev/char/uart:1088", O_WRONLY | O_NONBLOCK);
    if (n != 0) {               // 添加字符串结束符
      printf("Received: %s\n", buf); // 打印接收到的数据
      if (buf[0] == 'g') {
        break;
      }
    }
  }
  printf("fd: %d", fd);
  // 写入字符串
  char *str = "------fuck-----";
  int len = write(fd, str, strlen(str));
  printf("len: %d", len);
  // 关闭文件
  close(fd);
  return 0;
}