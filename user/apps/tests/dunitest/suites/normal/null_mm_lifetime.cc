#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <gtest/gtest.h>

#include <dirent.h>
#include <fcntl.h>
#include <sched.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <map>
#include <sstream>
#include <string>

namespace {
constexpr const char* kControl = "/sys/kernel/debug/kthread/selftest";

bool PinToCpu(int cpu) {
  cpu_set_t mask;
  CPU_ZERO(&mask);
  if (cpu < 0 || cpu >= CPU_SETSIZE) return false;
  CPU_SET(cpu, &mask);
  return sched_setaffinity(0, sizeof(mask), &mask) == 0 && sched_getcpu() == cpu;
}

bool Command(const char* command) {
  int fd = open(kControl, O_WRONLY | O_CLOEXEC);
  if (fd < 0) return false;
  const size_t len = std::strlen(command);
  ssize_t n;
  do {
    n = write(fd, command, len);
  } while (n < 0 && errno == EINTR);
  int saved = errno;
  int closed = close(fd);
  errno = saved;
  return n == static_cast<ssize_t>(len) && closed == 0;
}

int BorrowChild(int source, int target) {
  if (!PinToCpu(source)) return 10;
  long page_size = sysconf(_SC_PAGESIZE);
  if (page_size <= 0) return 11;
  void* mapping = mmap(nullptr, static_cast<size_t>(page_size),
                       PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED) return 12;
  auto* sentinel = static_cast<volatile unsigned char*>(mapping);
  *sentinel = 0x5a;
  if (*sentinel != 0x5a) return 13;
  if (!Command("borrow_arm\n")) return 14;
  if (!PinToCpu(target)) return 15;
  if (!Command("borrow_start\n")) return 16;
  // Keep the mapping live until the real exit_mm/unmap_all path.
  return 0;
}

std::string ReadFile(const std::string& path) {
  std::ifstream input(path);
  std::ostringstream out;
  out << input.rdbuf();
  return out.str();
}

// The kernel helpers resolve these paths in the initial mount namespace.
// Own an exclusively created directory, and never overwrite existing files.
class ExecFixtures {
 public:
  ExecFixtures() = default;
  ExecFixtures(const ExecFixtures&) = delete;
  ExecFixtures& operator=(const ExecFixtures&) = delete;

  ~ExecFixtures() {
    for (size_t i = 0; i < 4; ++i) {
      if (created_[i] && unlinkat(dir_fd_, kFiles[i].name, 0) != 0)
        ADD_FAILURE() << "fixture unlink: " << kFiles[i].name << ": " << std::strerror(errno);
    }
    if (dir_fd_ >= 0 && close(dir_fd_) != 0)
      ADD_FAILURE() << "fixture directory close: " << std::strerror(errno);
    if (owned_ && rmdir(kDirectory) != 0)
      ADD_FAILURE() << "fixture directory removal: " << std::strerror(errno);
  }

  bool Create() {
    if (mkdir(kDirectory, 0700) != 0) return false;
    owned_ = true;
    dir_fd_ = open(kDirectory, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC);
    if (dir_fd_ < 0) return false;
    for (size_t i = 0; i < 4; ++i) {
      int fd = openat(dir_fd_, kFiles[i].name,
                      O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0755);
      if (fd < 0) return false;
      created_[i] = true;
      const char* data = kFiles[i].content;
      size_t remaining = std::strlen(data);
      bool ok = true;
      while (remaining != 0) {
        ssize_t n = write(fd, data, remaining);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) { ok = false; break; }
        data += n;
        remaining -= static_cast<size_t>(n);
      }
      if (fchmod(fd, 0755) != 0) ok = false;
      if (close(fd) != 0) ok = false;
      if (!ok) return false;
    }
    return true;
  }

 private:
  struct File { const char* name; const char* content; };
  static constexpr const char* kDirectory = "/tmp/null-mm-exec";
  static constexpr File kFiles[4] = {
      {"invalid", "not an ELF executable\n"},
      {"script-ok", "#!/bin/busybox sh\nexit 37\n"},
      {"script-missing", "#!/no/such/null-mm-interpreter\n"},
      {"script-invalid-interpreter", "#!/tmp/null-mm-exec/invalid\n"},
  };
  int dir_fd_ = -1;
  bool owned_ = false;
  bool created_[4] = {};
};

bool ReadLifecycleReport(std::string* report) {
  int fd = open("/sys/kernel/debug/kthread/usermode_helper_selftest",
                O_RDONLY | O_NOFOLLOW | O_CLOEXEC);
  if (fd < 0) return false;
  char buf[4096];
  bool ok = true;
  for (;;) {
    ssize_t n = read(fd, buf, sizeof(buf));
    if (n < 0 && errno == EINTR) continue;
    if (n < 0) { ok = false; break; }
    if (n == 0) break;
    report->append(buf, static_cast<size_t>(n));
    if (report->size() > 65536) { ok = false; break; }
  }
  if (close(fd) != 0) ok = false;
  return ok;
}

using ReportFields = std::map<std::string, std::string>;

bool ParseReport(const std::string& report, ReportFields* fields) {
  if (report.empty() || report.back() != '\n') return false;
  std::istringstream input(report);
  std::string line;
  while (std::getline(input, line)) {
    size_t equal = line.find('=');
    if (equal == std::string::npos || equal == 0 || equal + 1 == line.size()) return false;
    if (!fields->emplace(line.substr(0, equal), line.substr(equal + 1)).second) return false;
  }
  return true;
}

#if defined(__x86_64__)
bool ParseU64(const std::string& text, unsigned long long* value) {
  if (text.empty()) return false;
  for (char c : text) if (c < '0' || c > '9') return false;
  errno = 0;
  char* end = nullptr;
  *value = std::strtoull(text.c_str(), &end, 10);
  return errno != ERANGE && end && *end == '\0';
}

void ExpectBorrowPair(const ReportFields& report, const std::string& key) {
  auto found = report.find(key);
  ASSERT_NE(found, report.end()) << "missing " << key;
  std::istringstream input(found->second);
  std::string status;
  ASSERT_TRUE(static_cast<bool>(input >> status));
  ASSERT_EQ(status, "ok");
  ReportFields fields;
  std::string token;
  while (input >> token) {
    size_t equal = token.find('=');
    ASSERT_NE(equal, std::string::npos);
    ASSERT_GT(equal, 0U);
    ASSERT_TRUE(fields.emplace(token.substr(0, equal), token.substr(equal + 1)).second)
        << "duplicate field " << token;
  }
  ASSERT_EQ(fields.size(), 11U);
  for (const char* name : {"admitted", "helper_reaped", "worker_joined", "inactive",
                           "retired", "no_users", "torn_down", "root_held"}) {
    auto field = fields.find(name);
    ASSERT_NE(field, fields.end()) << "missing " << name;
    EXPECT_EQ(field->second, "true") << name;
  }
  unsigned long long initial = 0, final = 0, loaded = 0;
  for (const char* name : {"initial_gen", "final_gen", "loaded_gen"}) {
    ASSERT_NE(fields.find(name), fields.end());
  }
  ASSERT_TRUE(ParseU64(fields.at("initial_gen"), &initial));
  ASSERT_TRUE(ParseU64(fields.at("final_gen"), &final));
  ASSERT_TRUE(ParseU64(fields.at("loaded_gen"), &loaded));
  EXPECT_GT(final, initial);
  EXPECT_GE(loaded, final);
}

#endif

TEST(NullMmLifetime, KernelHelperLifecycle) {
  ExecFixtures fixtures;
  ASSERT_TRUE(fixtures.Create()) << "exclusive exec fixtures: " << std::strerror(errno);
  std::string report;
  ASSERT_TRUE(ReadLifecycleReport(&report)) << "selftest open/read/close failed: " << std::strerror(errno);
  SCOPED_TRACE(report);
  constexpr const char* ordinary = "exec=ok\nmissing_exec=enoent\nwait_reap=ok\n";
  if (report == ordinary) {
    GTEST_SKIP() << "mm_selftest is disabled; ordinary exec/error/reap checks passed";
  }
  ReportFields fields;
  ASSERT_TRUE(ParseReport(report, &fields));
  const ReportFields expected = {
      {"exec", "ok"}, {"missing_exec", "enoent"}, {"wait_reap", "ok"},
      {"null_mm_elf", "ok rounds=1 reaped=1"},
      {"null_mm_missing_path", "ok rounds=0 reaped=1"},
      {"null_mm_invalid_elf", "ok rounds=1 reaped=1"},
      {"null_mm_shebang", "ok rounds=2 reaped=1"},
      {"null_mm_missing_interpreter", "ok rounds=1 reaped=1"},
      {"null_mm_invalid_interpreter", "ok rounds=2 reaped=1"},
      {"null_mm_natural_oom", "not_tested"},
  };
  for (const auto& entry : expected) {
    auto field = fields.find(entry.first);
    ASSERT_NE(field, fields.end()) << "missing " << entry.first;
    EXPECT_EQ(field->second, entry.second) << entry.first;
  }
  size_t expected_size = expected.size();
#if defined(__x86_64__)
  long online = sysconf(_SC_NPROCESSORS_ONLN);
  ASSERT_GT(online, 0);
  if (online == 1) {
    ASSERT_NE(fields.find("borrow_lifetime"), fields.end());
    EXPECT_EQ(fields.at("borrow_lifetime"), "not_applicable requires_two_cpus");
    ++expected_size;
  } else {
    ExpectBorrowPair(fields, "borrow_0_1");
    ++expected_size;
    if (online >= 4) {
      ExpectBorrowPair(fields, "borrow_2_3");
      ++expected_size;
    }
  }
#endif
  EXPECT_EQ(fields.size(), expected_size) << "unexpected report rows";
}

TEST(NullMmLifetime, KernelManagerHasNoUserMappings) {
  DIR* proc = opendir("/proc");
  ASSERT_NE(proc, nullptr);
  std::string manager;
  while (dirent* entry = readdir(proc)) {
    if (entry->d_name[0] < '0' || entry->d_name[0] > '9') continue;
    std::string base = std::string("/proc/") + entry->d_name;
    std::string stat = ReadFile(base + "/stat");
    if (stat.find("(kthreadd)") != std::string::npos) {
      manager = base;
      break;
    }
  }
  ASSERT_EQ(closedir(proc), 0);
  ASSERT_FALSE(manager.empty()) << "kthreadd must exist";
  int maps = open((manager + "/maps").c_str(), O_RDONLY | O_CLOEXEC);
  ASSERT_GE(maps, 0);
  char byte;
  ssize_t n = read(maps, &byte, 1);
  EXPECT_EQ(n, 0) << "kernel managers must expose no user mappings";
  EXPECT_EQ(close(maps), 0);
  std::istringstream statm(ReadFile(manager + "/statm"));
  unsigned long size = 1, resident = 1;
  ASSERT_TRUE(static_cast<bool>(statm >> size >> resident));
  EXPECT_EQ(size, 0UL);
  EXPECT_EQ(resident, 0UL);
}

TEST(NullMmLifetime, UserForkCowAndExecRemainUsable) {
  const long page_size = sysconf(_SC_PAGESIZE);
  ASSERT_GT(page_size, 0);
  auto* page = static_cast<volatile int*>(mmap(nullptr, static_cast<size_t>(page_size),
      PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
  ASSERT_NE(const_cast<int*>(page), MAP_FAILED);
  *page = 17;
  pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    if (*page != 17) _exit(20);
    *page = 29;
    execl("/bin/busybox", "busybox", "true", static_cast<char*>(nullptr));
    _exit(21);
  }
  int status = 0;
  pid_t waited;
  do {
    waited = waitpid(child, &status, 0);
  } while (waited < 0 && errno == EINTR);
  EXPECT_EQ(waited, child);
  EXPECT_TRUE(WIFEXITED(status));
  EXPECT_EQ(WEXITSTATUS(status), 0);
  EXPECT_EQ(*page, 17);
  EXPECT_EQ(munmap(const_cast<int*>(page), static_cast<size_t>(page_size)), 0);
}
}  // namespace

int main(int argc, char** argv) {
  if (argc == 4 && std::strcmp(argv[1], "--borrow-child") == 0) {
    char* source_end = nullptr;
    char* target_end = nullptr;
    errno = 0;
    long source = std::strtol(argv[2], &source_end, 10);
    long target = std::strtol(argv[3], &target_end, 10);
    if (errno == ERANGE || source_end == argv[2] || *source_end ||
        target_end == argv[3] || *target_end ||
        source < 0 || source >= CPU_SETSIZE || target < 0 || target >= CPU_SETSIZE ||
        source == target) _exit(9);
    _exit(BorrowChild(static_cast<int>(source), static_cast<int>(target)));
  }
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
