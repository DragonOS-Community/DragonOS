#include <stdlib.h>
#include <string.h>
#include <unistd.h> // for isatty()
#include <stdio.h>
int main() {
  char confirm[4];

  // 尝试打开控制终端
  FILE *tty = fopen("/dev/tty", "r+");
  if (tty == NULL) {
    perror("Error opening /dev/tty");
    // 如果没有控制终端，就不能继续，直接退出
    return EXIT_FAILURE;
  }

  // 检查标准输出是否是一个终端
  if (isatty(fileno(stdout))) {
    printf("This message goes to stdout.\n");
  } else {
    // 如果stdout被重定向了，我们仍然可以通过tty和用户交互
    fprintf(tty, "stdout has been redirected. This message is sent directly to "
                 "your terminal.\n");
  }

  // --- 关键部分 ---
  // 向 /dev/tty 写入提示信息
  fprintf(tty, "Do you want to proceed? (yes/no): ");

  // 从 /dev/tty 读取用户输入
  if (fgets(confirm, sizeof(confirm), tty) == NULL) {
    fprintf(stderr, "Failed to read from /dev/tty\n");
    fclose(tty);
    return EXIT_FAILURE;
  }

  // 清理换行符
  confirm[strcspn(confirm, "\n")] = 0;

  if (strcmp(confirm, "yes") == 0) {
    fprintf(tty, "Proceeding...\n");
    // 在这里执行实际操作...
  } else {
    fprintf(tty, "Operation cancelled.\n");
  }

  // 关闭文件
  fclose(tty);

  return EXIT_SUCCESS;
}