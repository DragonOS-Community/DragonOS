#pragma once
#include "glib.h"

// 计算字符串的长度（经过测试，该版本比采用repne/scasb汇编的运行速度快16.8%左右）
static inline int strlen(const char *s) {
  if (s == NULL)
    return 0;
  register int __res = 0;
  while (s[__res] != '\0') {
    ++__res;
  }
  return __res;
}
