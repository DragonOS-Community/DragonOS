#include <stdio.h>

void overflow(int depth) {
  char buffer[1024 * 1024]; // 占用一些栈空间
  printf("Recursion depth: %d\n", depth);
  overflow(depth + 1); // 递归调用
}

int main() {
  overflow(1);
  printf("This line will not be printed due to stack overflow.\n");
  return 0;
}
