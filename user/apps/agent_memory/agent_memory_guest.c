#define _GNU_SOURCE
#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* Only an exclusively created leaf is controlled; parent policy is never
 * changed. */
#define FIELD_COUNT 96
struct field {
  char key[64];
  unsigned long long value;
};
struct table {
  struct field fields[FIELD_COUNT];
  size_t count;
  bool valid;
};
struct snapshot {
  unsigned long long ns, current, peak;
  bool current_valid, peak_valid;
  struct table stat, events, cpu, meminfo;
};
static unsigned sample_errors;
static unsigned diagnostic_errors;
static volatile sig_atomic_t interrupted;
static const char *stat_keys[] = {
    "anon", "file", "shmem", "kernel_stack", "pagetables", "vmalloc", "kernel"};
static const char *event_keys[] = {"low", "high",     "max",
                                   "oom", "oom_kill", "oom_group_kill"};
static const char *cpu_keys[] = {"usage_usec", "user_usec", "system_usec"};
static const char *cpu_output_keys[] = {"usage_usec",   "user_usec",
                                        "system_usec",  "nr_periods",
                                        "nr_throttled", "throttled_usec"};
static const char *mem_keys[] = {"MemTotal", "MemFree",   "Cached", "Slab",
                                 "Dirty",    "Writeback", "Shmem"};
#define COUNT(a) (sizeof(a) / sizeof((a)[0]))
static unsigned long long now_ns(void) {
  struct timespec ts;
  if (clock_gettime(CLOCK_MONOTONIC, &ts)) {
    perror("clock_gettime");
    exit(2);
  }
  return (unsigned long long)ts.tv_sec * 1000000000ULL +
         (unsigned long long)ts.tv_nsec;
}
static void pause_ms(unsigned ms) {
  struct timespec ts = {.tv_sec = ms / 1000,
                        .tv_nsec = (long)(ms % 1000) * 1000000};
  while (nanosleep(&ts, &ts) && errno == EINTR && !interrupted) {
  }
}
static void on_signal(int sig) { interrupted = sig; }
static bool join(char *out, const char *dir, const char *name) {
  int n = snprintf(out, PATH_MAX, "%s/%s", dir, name);
  return n >= 0 && n < PATH_MAX;
}
static bool read_text(const char *path, char *buf, size_t cap) {
  int fd = open(path, O_RDONLY | O_CLOEXEC);
  if (fd < 0)
    return false;
  size_t used = 0;
  bool ok = true;
  while (used < cap - 1) {
    ssize_t n = read(fd, buf + used, cap - 1 - used);
    if (n < 0 && errno == EINTR)
      continue;
    if (n < 0) {
      ok = false;
      break;
    }
    if (!n)
      break;
    used += (size_t)n;
  }
  if (used == cap - 1) {
    char extra;
    if (read(fd, &extra, 1) != 0)
      ok = false;
  }
  buf[used] = 0;
  if (close(fd))
    ok = false;
  return ok;
}
static bool write_text(const char *path, const char *text) {
  int fd = open(path, O_WRONLY | O_CLOEXEC);
  if (fd < 0)
    return false;
  size_t len = strlen(text), used = 0;
  while (used < len) {
    ssize_t n = write(fd, text + used, len - used);
    if (n < 0 && errno == EINTR)
      continue;
    if (n <= 0) {
      close(fd);
      return false;
    }
    used += (size_t)n;
  }
  return close(fd) == 0;
}
static bool number(const char *text, unsigned long long *value) {
  if (!text || !isdigit((unsigned char)*text))
    return false;
  errno = 0;
  char *end;
  unsigned long long n = strtoull(text, &end, 10);
  while (isspace((unsigned char)*end))
    end++;
  if (errno || *end)
    return false;
  *value = n;
  return true;
}
static bool read_number(const char *dir, const char *name,
                        unsigned long long *n) {
  char path[PATH_MAX], buf[128];
  return join(path, dir, name) && read_text(path, buf, sizeof(buf)) &&
         number(buf, n);
}
static struct table read_table(const char *dir, const char *name, bool kib) {
  struct table t = {0};
  char path[PATH_MAX], buf[16384];
  if (!join(path, dir, name) || !read_text(path, buf, sizeof(buf)))
    return t;
  char *save;
  for (char *line = strtok_r(buf, "\n", &save); line;
       line = strtok_r(NULL, "\n", &save)) {
    char key[64], numeric[32], unit[16], extra[2];
    int n = sscanf(line, "%63s %31s %15s %1s", key, numeric, unit, extra);
    unsigned long long value;
    /* meminfo also has unrelated unitless fields; selected fields require kB.
     */
    if (kib && n == 2)
      continue;
    if ((kib ? n != 3 || strcmp(unit, "kB") : n != 2) ||
        !number(numeric, &value))
      return t;
    size_t len = strlen(key);
    if (kib && len && key[len - 1] == ':')
      key[--len] = 0;
    for (size_t i = 0; i < len; i++)
      if (!isalnum((unsigned char)key[i]) && key[i] != '_')
        return t;
    if (!len || t.count == FIELD_COUNT)
      return t;
    for (size_t i = 0; i < t.count; i++)
      if (!strcmp(t.fields[i].key, key))
        return t;
    strcpy(t.fields[t.count].key, key);
    t.fields[t.count++].value = value;
  }
  t.valid = true;
  return t;
}
static bool lookup(const struct table *t, const char *key,
                   unsigned long long *n) {
  if (!t->valid)
    return false;
  for (size_t i = 0; i < t->count; i++)
    if (!strcmp(t->fields[i].key, key)) {
      *n = t->fields[i].value;
      return true;
    }
  return false;
}
static bool complete(const struct table *t, const char **keys, size_t count) {
  unsigned long long n;
  for (size_t i = 0; i < count; i++)
    if (!lookup(t, keys[i], &n))
      return false;
  return true;
}
static struct snapshot sample(const char *leaf) {
  struct snapshot s = {.ns = now_ns()};
  s.current_valid = read_number(leaf, "memory.current", &s.current);
  s.peak_valid = read_number(leaf, "memory.peak", &s.peak);
  s.stat = read_table(leaf, "memory.stat", false);
  s.events = read_table(leaf, "memory.events", false);
  s.cpu = read_table(leaf, "cpu.stat", false);
  s.meminfo = read_table("/proc", "meminfo", true);
  if (!s.current_valid || !s.peak_valid ||
      !complete(&s.stat, stat_keys, COUNT(stat_keys)) ||
      !complete(&s.events, event_keys, COUNT(event_keys)) ||
      !complete(&s.cpu, cpu_keys, COUNT(cpu_keys)) ||
      !complete(&s.meminfo, mem_keys, COUNT(mem_keys)))
    sample_errors++;
  return s;
}
static void json_number(FILE *f, bool valid, unsigned long long n) {
  if (valid)
    fprintf(f, "%llu", n);
  else
    fputs("null", f);
}
static void json_table(FILE *f, const struct table *t, const char **keys,
                       size_t count) {
  fputc('{', f);
  for (size_t i = 0; i < count; i++) {
    unsigned long long n = 0;
    if (i)
      fputc(',', f);
    fprintf(f, "\"%s\":", keys[i]);
    bool valid = lookup(t, keys[i], &n);
    json_number(f, valid, n);
  }
  fputc('}', f);
}
static void json_snapshot(FILE *f, const struct snapshot *s,
                          const char *phase) {
  fputc('{', f);
  if (phase)
    fprintf(f, "\"phase\":\"%s\",", phase);
  fprintf(f, "\"monotonic_ns\":%llu,\"memory_current_bytes\":", s->ns);
  json_number(f, s->current_valid, s->current);
  fputs(",\"memory_peak_bytes\":", f);
  json_number(f, s->peak_valid, s->peak);
  fputs(",\"memory_stat\":", f);
  json_table(f, &s->stat, stat_keys, COUNT(stat_keys));
  fputs(",\"memory_events\":", f);
  json_table(f, &s->events, event_keys, COUNT(event_keys));
  fputs(",\"cpu_stat\":", f);
  json_table(f, &s->cpu, cpu_output_keys, COUNT(cpu_output_keys));
  fputs(",\"guest_meminfo\":", f);
  json_table(f, &s->meminfo, mem_keys, COUNT(mem_keys));
  fputc('}', f);
}
static void line(FILE *f, const struct snapshot *s, const char *phase) {
  json_snapshot(f, s, phase);
  fputc('\n', f);
  if (fflush(f) || ferror(f))
    sample_errors++;
}
static bool empty_leaf(const char *leaf) {
  struct table t = read_table(leaf, "cgroup.events", false);
  unsigned long long n;
  return lookup(&t, "populated", &n) && !n;
}
/* A reaped leader no longer protects its PID/PGID. Never signal it or enumerate
 * leaf PIDs afterwards; escaped/background descendants require host VM
 * teardown. */
static bool drain(const char *leaf, pid_t owned_child, unsigned seconds,
                  bool *cleanup_required, unsigned long long *empty_ns) {
  unsigned long long start = now_ns(),
                     duration = (unsigned long long)seconds * 1000000000ULL;
  do {
    if (empty_leaf(leaf)) {
      *empty_ns = now_ns();
      return true;
    }
    if (owned_child > 0) {
      int sig = now_ns() - start < duration / 2 ? SIGTERM : SIGKILL;
      *cleanup_required = true;
      kill(-owned_child, sig);
      /* The unreaped direct child also covers setsid/start-gate failure. */
      kill(owned_child, sig);
    }
    pause_ms(50);
  } while (now_ns() - start < duration);
  if (empty_leaf(leaf)) {
    *empty_ns = now_ns();
    return true;
  }
  return false;
}
static FILE *private_file(const char *dir, const char *name) {
  char path[PATH_MAX];
  if (!join(path, dir, name))
    return NULL;
  int fd = open(path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
  if (fd < 0)
    return NULL;
  FILE *f = fdopen(fd, "w");
  if (!f)
    close(fd);
  return f;
}
static void diagnostic(const char *dir, const char *name) {
  char buf[16384];
  if (!read_text("/sys/kernel/debug/mm/allocation_stats", buf, sizeof(buf))) {
    diagnostic_errors++;
    return;
  }
  FILE *f = private_file(dir, name);
  if (!f) {
    diagnostic_errors++;
    return;
  }
  bool ok = fputs(buf, f) >= 0;
  if (fclose(f))
    ok = false;
  if (!ok)
    diagnostic_errors++;
}
static void usage(void) {
  fputs("agent_memory_guest --output DIR [--cgroup-parent PATH] [--limit-bytes "
        "N] "
        "[--interval-ms N] [--deadline-seconds N] [--settle-seconds N] "
        "[--drain-seconds N] "
        "[--diagnostic] -- command [args...]\n",
        stderr);
}
int main(int argc, char **argv) {
  const char *output = NULL, *parent = "/sys/fs/cgroup";
  unsigned long long limit = 0, interval = 1000, deadline = 300, settle = 5,
                     drain_seconds = 5;
  bool has_limit = false, diag = false;
  int arg = 1;
  for (; arg < argc && strcmp(argv[arg], "--"); arg++) {
    const char *option = argv[arg];
    if (!strcmp(option, "--diagnostic")) {
      diag = true;
      continue;
    }
    if (++arg == argc) {
      usage();
      return 2;
    }
    if (!strcmp(option, "--output"))
      output = argv[arg];
    else if (!strcmp(option, "--cgroup-parent"))
      parent = argv[arg];
    else {
      unsigned long long n;
      if (!number(argv[arg], &n)) {
        usage();
        return 2;
      }
      if (!strcmp(option, "--limit-bytes")) {
        limit = n;
        has_limit = true;
      } else if (!strcmp(option, "--interval-ms"))
        interval = n;
      else if (!strcmp(option, "--deadline-seconds"))
        deadline = n;
      else if (!strcmp(option, "--settle-seconds"))
        settle = n;
      else if (!strcmp(option, "--drain-seconds"))
        drain_seconds = n;
      else {
        usage();
        return 2;
      }
    }
  }
  if (!output || arg + 1 >= argc || interval < 10 || interval > 60000 ||
      !deadline || deadline > 86400 || settle > 3600 || !drain_seconds ||
      drain_seconds > 300) {
    usage();
    return 2;
  }
  umask(077);
  if (mkdir(output, 0700)) {
    perror("exclusive output directory");
    return 2;
  }
  FILE *metrics = private_file(output, "metrics.jsonl");
  FILE *out = private_file(output, "stdout.log"),
       *err = private_file(output, "stderr.log");
  if (!metrics || !out || !err) {
    perror("output files");
    return 2;
  }
  struct sigaction sa = {.sa_handler = on_signal};
  sigemptyset(&sa.sa_mask);
  sigaction(SIGTERM, &sa, NULL);
  sigaction(SIGINT, &sa, NULL);
  /* A closed start gate is a setup failure, not permission to execute. */
  signal(SIGPIPE, SIG_IGN);
  char leaf[PATH_MAX], path[PATH_MAX], buf[128];
  unsigned long long begin = now_ns();
  unsigned long long workload_start_ns = 0, leader_exit_ns = 0,
                     descendants_empty_ns = 0;
  bool natural_completion = false;
  snprintf(buf, sizeof(buf), "agent-memory-%ld-%llu", (long)getpid(), begin);
  bool made_leaf = join(leaf, parent, buf) && !mkdir(leaf, 0700);
  const char *status = "sample_failed";
  pid_t child = -1;
  int child_status = 0;
  bool reaped = false, empty = false, child_owned = false,
       cleanup_required = false;
  struct snapshot baseline = {0}, final = {0}, settled = {0};
  if (!made_leaf) {
    sample_errors++;
    goto finish;
  }
  baseline = sample(leaf);
  line(metrics, &baseline, "baseline");
  if (sample_errors)
    goto cleanup;
  if (has_limit) {
    snprintf(buf, sizeof(buf), "%llu", limit);
    if (!join(path, leaf, "memory.max") || !write_text(path, buf))
      goto cleanup;
  }
  if (diag)
    diagnostic(output, "allocation_before.txt");
  int gate[2];
  if (pipe2(gate, O_CLOEXEC))
    goto cleanup;
  child = fork();
  if (!child) {
    if (setsid() < 0)
      _exit(126);
    close(gate[1]);
    char go;
    ssize_t n;
    do {
      n = read(gate[0], &go, 1);
    } while (n < 0 && errno == EINTR);
    if (n != 1 || go != 'G')
      _exit(125);
    close(gate[0]);
    if (dup2(fileno(out), STDOUT_FILENO) < 0 ||
        dup2(fileno(err), STDERR_FILENO) < 0)
      _exit(126);
    signal(SIGTERM, SIG_DFL);
    signal(SIGINT, SIG_DFL);
    signal(SIGPIPE, SIG_DFL);
    execvp(argv[arg + 1], &argv[arg + 1]);
    perror("execvp");
    _exit(127);
  }
  close(gate[0]);
  if (child < 0) {
    close(gate[1]);
    goto cleanup;
  }
  child_owned = true;
  snprintf(buf, sizeof(buf), "%ld", (long)child);
  if (!join(path, leaf, "cgroup.procs") || !write_text(path, buf)) {
    close(gate[1]);
    goto cleanup;
  }
  unsigned long long gate_release_ns = now_ns();
  if (write(gate[1], "G", 1) != 1) {
    close(gate[1]);
    goto cleanup;
  }
  workload_start_ns = gate_release_ns;
  close(gate[1]);
  status = "ok";
  unsigned long long next_sample = now_ns();
  while (!reaped) {
    pid_t waited = waitpid(child, &child_status, WNOHANG);
    if (waited == child) {
      leader_exit_ns = now_ns();
      reaped = true;
      child_owned = false;
      break;
    }
    if (waited < 0 && errno != EINTR) {
      child_owned = false;
      status = "cleanup_failed";
      break;
    }
    unsigned long long now = now_ns();
    if (now >= next_sample) {
      struct snapshot s = sample(leaf);
      line(metrics, &s, "running");
      next_sample = now + interval * 1000000ULL;
    }
    if (interrupted || now - begin >= deadline * 1000000000ULL) {
      status = "timeout";
      break;
    }
    pause_ms(20);
  }
  if (reaped && (!WIFEXITED(child_status) || WEXITSTATUS(child_status)))
    status = "workload_failed";
cleanup:
  final = sample(leaf);
  line(metrics, &final, "final");
  empty = drain(leaf, child_owned ? child : 0, (unsigned)drain_seconds,
                &cleanup_required, &descendants_empty_ns);
  natural_completion = workload_start_ns && leader_exit_ns && empty &&
                       !cleanup_required && strcmp(status, "timeout");
  if (child_owned) {
    kill(child, SIGKILL);
    unsigned long long until = now_ns() + drain_seconds * 1000000000ULL;
    do {
      if (waitpid(child, &child_status, WNOHANG) == child) {
        leader_exit_ns = now_ns();
        reaped = true;
        child_owned = false;
        break;
      }
      pause_ms(20);
    } while (now_ns() < until);
  }
  if (!empty || (child > 0 && !reaped))
    status = "cleanup_failed";
  if (empty) {
    unsigned long long until = now_ns() + settle * 1000000000ULL;
    do {
      settled = sample(leaf);
      line(metrics, &settled, "settled");
      unsigned long long now = now_ns();
      if (now >= until)
        break;
      unsigned long long remaining_ms = (until - now + 999999ULL) / 1000000ULL;
      pause_ms((unsigned)(interval < remaining_ms ? interval : remaining_ms));
    } while (true);
  }
  if (diag)
    diagnostic(output, "allocation_after.txt");
  if (rmdir(leaf))
    status = "cleanup_failed";
finish:
  if ((sample_errors || diagnostic_errors) && !strcmp(status, "ok"))
    status = "sample_failed";
  FILE *result = private_file(output, "result.json.tmp");
  if (!result)
    return 2;
  fprintf(result, "{\"schema\":1,\"status\":\"%s\",\"child_pid\":", status);
  if (child > 0)
    fprintf(result, "%ld", (long)child);
  else
    fputs("null", result);
  fputs(",\"exit_code\":", result);
  if (reaped && WIFEXITED(child_status))
    fprintf(result, "%d", WEXITSTATUS(child_status));
  else
    fputs("null", result);
  fputs(",\"term_signal\":", result);
  if (reaped && WIFSIGNALED(child_status))
    fprintf(result, "%d", WTERMSIG(child_status));
  else
    fputs("null", result);
  fprintf(
      result,
      ",\"elapsed_ns\":%llu,\"sample_errors\":%u,\"diagnostic_errors\":%u,"
      "\"cleanup_required\":%s,\"descendants_empty\":%s,\"memory_peak_bytes\":",
      now_ns() - begin, sample_errors, diagnostic_errors,
      cleanup_required ? "true" : "false", empty ? "true" : "false");
  json_number(result, settled.peak_valid || final.peak_valid,
              settled.peak_valid ? settled.peak : final.peak);
  fputs(",\"workload_start_ns\":", result);
  json_number(result, workload_start_ns != 0, workload_start_ns);
  fputs(",\"leader_exit_ns\":", result);
  json_number(result, leader_exit_ns != 0, leader_exit_ns);
  fputs(",\"descendants_empty_ns\":", result);
  json_number(result, descendants_empty_ns != 0, descendants_empty_ns);
  fputs(",\"task_elapsed_ns\":", result);
  json_number(result, natural_completion,
              natural_completion ? descendants_empty_ns - workload_start_ns
                                 : 0);
  fputs(",\"baseline\":", result);
  json_snapshot(result, &baseline, NULL);
  fputs(",\"final\":", result);
  json_snapshot(result, &final, NULL);
  fputs(",\"settled\":", result);
  json_snapshot(result, &settled, NULL);
  fputs("}\n", result);
  bool ok = !ferror(result) && !fflush(result);
  if (fclose(result))
    ok = false;
  if (fclose(metrics))
    ok = false;
  fclose(out);
  fclose(err);
  char completed[PATH_MAX], temporary[PATH_MAX];
  if (!join(completed, output, "result.json") ||
      !join(temporary, output, "result.json.tmp") ||
      rename(temporary, completed))
    ok = false;
  return ok && !strcmp(status, "ok") ? 0 : 1;
}
