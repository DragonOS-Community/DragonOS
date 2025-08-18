#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/klog.h>
#include <unistd.h>
/**
 * @brief 识别dmesg程序的第一个选项参数
 *
 * @param arg dmesg命令第一个选项参数
 * @return int 有效时返回对应选项码，无效时返回 -1
 */
int getoption(char *arg) {
  if (!strcmp(arg, "-h") || !strcmp(arg, "--help"))
    return 0;
  else if (!strcmp(arg, "-c") || !strcmp(arg, "--read-clear"))
    return 4;
  else if (!strcmp(arg, "-C") || !strcmp(arg, "--clear"))
    return 5;
  else if (!strcmp(arg, "-l") || !strcmp(arg, "--level"))
    return 8;

  return -1;
}

/**
 * @brief 识别dmesg程序的第二个选项参数
 *
 * @param arg dmesg命令第一个选项参数
 * @return int 有效时返回设置的日志级别，无效时返回 -1
 */
int getlevel(char *arg) {
  if (!strcmp(arg, "EMERG") || !strcmp(arg, "emerg"))
    return 0;
  else if (!strcmp(arg, "ALERT") || !strcmp(arg, "alert"))
    return 1;
  else if (!strcmp(arg, "CRIT") || !strcmp(arg, "crit"))
    return 2;
  else if (!strcmp(arg, "ERR") || !strcmp(arg, "err"))
    return 3;
  else if (!strcmp(arg, "WARN") || !strcmp(arg, "warn"))
    return 4;
  else if (!strcmp(arg, "NOTICE") || !strcmp(arg, "notice"))
    return 5;
  else if (!strcmp(arg, "INFO") || !strcmp(arg, "info"))
    return 6;
  else if (!strcmp(arg, "DEBUG") || !strcmp(arg, "debug"))
    return 7;
  else {
    printf("dmesg: unknown level '%s'\n", arg);
  }
  return -2;
}

/**
 * @brief 打印dmesg手册
 */
void print_help_msg() {
  const char *help_msg =
      "Usage:\n"
      " dmesg [options]\n\n"
      "Display or control the kernel ring buffer.\n\n"
      "Options:\n"
      " -C, --clear                 clear the kernel ring buffer\n"
      " -c, --read-clear            read and clear all messages\n"
      " -l, --level <list>          restrict output to defined levels\n"
      " -h, --help                  display this help\n\n"
      "Supported log levels (priorities):\n"
      "   emerg - system is unusable\n"
      "   alert - action must be taken immediately\n"
      "    crit - critical conditions\n"
      "     err - error conditions\n"
      "    warn - warning conditions\n"
      "  notice - normal but significant condition\n"
      "    info - informational\n"
      "   debug - debug-level messages\n";
  printf("%s\n", help_msg);
}

/**
 * @brief 打印dmesg错误使用的信息
 */
void print_bad_usage_msg() {
  const char *bad_usage_msg =
      "dmesg: bad usage\nTry 'dmesg --help' for more information.";
  printf("%s\n", bad_usage_msg);
}
int main(int argc, char **argv) {
  unsigned int len = 1;
  char *buf = NULL;
  int opt;
  unsigned int color = 65280;

  // 获取内核缓冲区大小
  len = klogctl(10, buf, len);

  if (len < 16 * 1024)
    len = 16 * 1024;
  if (len > 16 * 1024 * 1024)
    len = 16 * 1024 * 1024;

  buf = malloc(len);
  if (buf == NULL) {
    perror("");
    return -1;
  }

  if (argc == 1) {
    // 无选项参数，默认打印所有日志消息
    len = klogctl(2, buf, len);
  } else {
    // 获取第一个选项参数
    opt = getoption(argv[1]);

    // 无效参数
    if (opt == -1) {
      print_bad_usage_msg();
      return -1;
    }
    // 打印帮助手册
    else if (opt == 0) {
      print_help_msg();
      return 0;
    }
    // 4 -> 读取内核缓冲区后，清空缓冲区
    // 5 -> 清空内核缓冲区
    else if (opt == 4 || opt == 5) {
      len = klogctl(opt, buf, len);
    }
    // 读取特定日志级别的消息
    else if (opt == 8) {
      // 无指定日志级别参数，打印错误使用信息
      if (argc < 3) {
        print_bad_usage_msg();
        return -1;
      }

      int level = -1;

      // 获取日志级别
      // 这里加1的原因是：如果klogctl的第三个参数是0，不会发生系统调用
      level = getlevel(argv[2]) + 1;

      if (level == -1)
        return -1;

      klogctl(8, buf, level);
      len = klogctl(2, buf, len);
    }
  }

  // 当前打印内容
  // 0: 日志级别
  // 1: 时间戳
  // 2: 代码行号
  // 3: 日志消息
  unsigned int content = 0;
  for (int i = 0; i < len; i++) {
    char c[2];
    c[0] = buf[i];
    c[1] = '\0';
    syscall(100000, &c[0], color, 0);
    if (content == 0 && buf[i] == '>') {
      content++;
    } else if (content == 1 && buf[i] == ']') {
      color = 16744448;
      content++;
    } else if (content == 2 && buf[i] == ')') {
      color = 16777215;
      content++;
    } else if (content == 3 && buf[i] == '\n') {
      color = 65280;
      content = 0;
    }
  }

  free(buf);

  return 0;
}