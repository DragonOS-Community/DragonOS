#define main helper_main
#include "agent_memory_guest.c"
#undef main
#include <assert.h>
int main(int argc, char **argv) {
  assert(argc == 2);
  unsigned long long n = 0;
  assert(number("18446744073709551615\n", &n));
  assert(n == ~0ULL);
  assert(!number("-1", &n));
  assert(!number("18446744073709551616", &n));
  assert(!number("1 garbage", &n));
  char path[PATH_MAX];
  assert(join(path, argv[1], "values"));
  FILE *f = fopen(path, "w");
  assert(f);
  fputs("usage_usec 7\nuser_usec 3\nsystem_usec 4\n", f);
  fclose(f);
  struct table t = read_table(argv[1], "values", false);
  assert(complete(&t, cpu_keys, COUNT(cpu_keys)));
  assert(lookup(&t, "usage_usec", &n) && n == 7);
  f = fopen(path, "w");
  assert(f);
  fputs("usage_usec 11\n", f);
  fclose(f);
  t = read_table(argv[1], "values", false);
  assert(lookup(&t, "usage_usec", &n) && n == 11);
  assert(!complete(&t, cpu_keys, COUNT(cpu_keys)));
  f = fopen(path, "w");
  assert(f);
  fputs("x 1\nx 2\n", f);
  fclose(f);
  assert(!read_table(argv[1], "values", false).valid);
  f = fopen(path, "w");
  assert(f);
  fputs("MemTotal: 123 kB\nHugePages_Total: 0\n", f);
  fclose(f);
  t = read_table(argv[1], "values", true);
  assert(lookup(&t, "MemTotal", &n) && n == 123);
  f = fopen(path, "w");
  assert(f);
  fputs("x -1\n", f);
  fclose(f);
  assert(!read_table(argv[1], "values", false).valid);

  assert(join(path, argv[1], "cgroup.events"));
  f = fopen(path, "w");
  assert(f);
  fputs("populated 1\n", f);
  fclose(f);
  bool cleaned = false;
  unsigned long long empty_ns = 0;
  pid_t sibling = fork();
  assert(sibling >= 0);
  if (!sibling) {
    for (;;)
      pause();
  }
  assert(!drain(argv[1], 0, 1, &cleaned, &empty_ns));
  assert(!cleaned && kill(sibling, 0) == 0);
  int ready[2];
  assert(pipe(ready) == 0);
  pid_t owned = fork();
  assert(owned >= 0);
  if (!owned) {
    close(ready[0]);
    assert(setsid() == getpid());
    assert(write(ready[1], "R", 1) == 1);
    close(ready[1]);
    for (;;)
      pause();
  }
  close(ready[1]);
  char r;
  assert(read(ready[0], &r, 1) == 1);
  close(ready[0]);
  assert(!drain(argv[1], owned, 1, &cleaned, &empty_ns));
  assert(cleaned && kill(sibling, 0) == 0);
  assert(empty_ns == 0);
  int status;
  assert(waitpid(owned, &status, 0) == owned);
  assert(WIFSIGNALED(status));
  kill(sibling, SIGKILL);
  assert(waitpid(sibling, &status, 0) == sibling);
  f = fopen(path, "w");
  assert(f);
  fputs("populated 0\n", f);
  fclose(f);
  assert(drain(argv[1], 0, 1, &cleaned, &empty_ns));
  assert(empty_ns > 0);
  return 0;
}
