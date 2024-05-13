#include <stdio.h>

int main() {
  while(1){
    printf("\033[43;37mHello, World!\033[0m\n");
    sleep(1);
  }
  return 0;
}