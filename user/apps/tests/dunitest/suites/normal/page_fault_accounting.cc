#include <gtest/gtest.h>

#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <time.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <cstdint>
#include <cstdlib>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <sstream>
#include <string>
#include <vector>

namespace {

constexpr size_t kPageSize = 4096;
constexpr size_t kFaultPages = 128;

struct FaultStat {
  uint64_t minflt;
  uint64_t cminflt;
  uint64_t majflt;
  uint64_t cmajflt;
};

bool ReadProcStat(const std::string& path, FaultStat* out) {
  std::ifstream input(path);
  std::string line;
  if (!std::getline(input, line)) return false;

  const size_t comm_end = line.rfind(')');
  if (comm_end == std::string::npos || comm_end + 2 >= line.size()) return false;

  std::istringstream fields(line.substr(comm_end + 2));
  std::vector<std::string> values;
  for (std::string value; fields >> value;) values.push_back(value);
  if (values.size() <= 10) return false;

  try {
    out->minflt = std::stoull(values[7]);
    out->cminflt = std::stoull(values[8]);
    out->majflt = std::stoull(values[9]);
    out->cmajflt = std::stoull(values[10]);
  } catch (...) {
    return false;
  }
  return true;
}

bool ReadVmstat(const char* name, uint64_t* out) {
  std::ifstream input("/proc/vmstat");
  std::string key;
  uint64_t value;
  while (input >> key >> value) {
    if (key == name) {
      *out = value;
      return true;
    }
  }
  return false;
}

void* MapAndTouch(size_t pages) {
  const size_t length = pages * kPageSize;
  void* mapping = mmap(nullptr, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (mapping == MAP_FAILED) return MAP_FAILED;

  volatile uint8_t* bytes = static_cast<volatile uint8_t*>(mapping);
  for (size_t page = 0; page < pages; ++page) bytes[page * kPageSize] = 0x5a;
  return mapping;
}

bool WriteByte(int fd) {
  const char byte = 'x';
  return write(fd, &byte, 1) == 1;
}

bool ReadByte(int fd) {
  char byte;
  return read(fd, &byte, 1) == 1;
}

bool WriteExact(int fd, const void* data, size_t length) {
  const auto* bytes = static_cast<const uint8_t*>(data);
  while (length != 0) {
    const ssize_t written = write(fd, bytes, length);
    if (written <= 0) return false;
    bytes += written;
    length -= static_cast<size_t>(written);
  }
  return true;
}

bool ReadExact(int fd, void* data, size_t length) {
  auto* bytes = static_cast<uint8_t*>(data);
  while (length != 0) {
    const ssize_t nread = read(fd, bytes, length);
    if (nread <= 0) return false;
    bytes += nread;
    length -= static_cast<size_t>(nread);
  }
  return true;
}

struct WorkerContext {
  int ready_write;
  int start_read;
  int done_write;
  int finish_read;
  void* mapping;
};

void* FaultWorker(void* arg) {
  auto* context = static_cast<WorkerContext*>(arg);
  const pid_t tid = static_cast<pid_t>(syscall(SYS_gettid));
  if (!WriteExact(context->ready_write, &tid, sizeof(tid)) ||
      !ReadByte(context->start_read))
    return nullptr;
  context->mapping = MapAndTouch(kFaultPages);
  const char status = context->mapping == MAP_FAILED ? 'e' : 'x';
  if (!WriteExact(context->done_write, &status, sizeof(status))) return nullptr;
  (void)ReadByte(context->finish_read);
  return nullptr;
}

// Reuse this exact code and stack path before the measured first writes.
__attribute__((noinline)) void WriteFaultPages(volatile uint8_t* bytes,
                                             size_t pages, uint8_t value) {
  for (size_t page = 0; page < pages; ++page) bytes[page * kPageSize] = value;
}

TEST(PageFaultAccounting, AnonymousFirstWriteNeedsOneFaultPerPage) {
  constexpr size_t kPages = 64;
  constexpr size_t kLength = kPages * kPageSize;
  void* scratch = mmap(nullptr, kLength, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_NE(MAP_FAILED, scratch) << strerror(errno);
  void* target = mmap(nullptr, kLength, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (target == MAP_FAILED) {
    const int saved = errno;
    munmap(scratch, kLength);
    FAIL() << strerror(saved);
    return;
  }
  // Multi-size THP can be smaller than 2 MiB; page count alone is insufficient.
  const int advice = madvise(target, kLength, MADV_NOHUGEPAGE);
  const int advice_errno = errno;
  auto* bytes = static_cast<volatile uint8_t*>(target);
  struct rusage usage[5] {};
  long results[5] {};
  for (auto& u : usage) (void)syscall(SYS_getrusage, RUSAGE_THREAD, &u);
  WriteFaultPages(static_cast<volatile uint8_t*>(scratch), kPages, 0x5a);

  // No assertions, allocation, formatting or setup inside the counter window.
  results[0] = syscall(SYS_getrusage, RUSAGE_THREAD, &usage[0]);
  results[1] = syscall(SYS_getrusage, RUSAGE_THREAD, &usage[1]);
  results[2] = syscall(SYS_getrusage, RUSAGE_THREAD, &usage[2]);
  WriteFaultPages(bytes, kPages, 0x5a);
  results[3] = syscall(SYS_getrusage, RUSAGE_THREAD, &usage[3]);
  WriteFaultPages(bytes, kPages, 0xa5);
  results[4] = syscall(SYS_getrusage, RUSAGE_THREAD, &usage[4]);

  bool contents_ok = true;
  for (size_t page = 0; page < kPages; ++page)
    contents_ok = contents_ok && bytes[page * kPageSize] == 0xa5;
  const int target_unmap = munmap(target, kLength);
  const int scratch_unmap = munmap(scratch, kLength);
  ASSERT_EQ(0, advice) << strerror(advice_errno);
  for (long result : results) ASSERT_EQ(0, result);
  EXPECT_EQ(0, usage[1].ru_minflt - usage[0].ru_minflt);
  EXPECT_EQ(0, usage[2].ru_minflt - usage[1].ru_minflt);
  EXPECT_EQ(static_cast<long>(kPages), usage[3].ru_minflt - usage[2].ru_minflt);
  EXPECT_EQ(0, usage[4].ru_minflt - usage[3].ru_minflt);
  EXPECT_EQ(0, usage[4].ru_majflt - usage[0].ru_majflt);
  EXPECT_TRUE(contents_ok);
  EXPECT_EQ(0, target_unmap);
  EXPECT_EQ(0, scratch_unmap);
}

// Parent bounds the wait and always reaps its own child, including failures.
bool ReapProtectionChild(pid_t child, int* status) {
  struct timespec begin {};
  const bool have_clock = clock_gettime(CLOCK_MONOTONIC, &begin) == 0;
  for (;;) {
    const pid_t result = waitpid(child, status, WNOHANG);
    if (result == child) return true;
    if (result < 0) {
      if (errno == EINTR) continue;
      // ECHILD (or another wait error) does not certify a live owned PID.
      // Never signal a PID after ownership has been lost.
      return false;
    }
    struct timespec now {};
    if (!have_clock || clock_gettime(CLOCK_MONOTONIC, &now) != 0 ||
        now.tv_sec - begin.tv_sec >= 3) break;
    struct timespec delay {0, 1000000};
    nanosleep(&delay, nullptr);
  }
  // No other thread reaps this child; the last WNOHANG result was zero.
  kill(child, SIGKILL);
  while (waitpid(child, status, 0) < 0 && errno == EINTR) {}
  return false;
}

TEST(PageFaultAccounting, AnonymousFirstFaultPreservesProtection) {
  for (int protection : {PROT_READ, PROT_NONE}) {
    void* mapping = mmap(nullptr, kPageSize, protection,
                        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);
    const pid_t child = fork();
    if (child == 0) {
      struct rlimit limit {0, 0};
      (void)setrlimit(RLIMIT_CORE, &limit);
      struct sigaction action {};
      action.sa_handler = SIG_DFL;
      sigemptyset(&action.sa_mask);
      sigset_t unblocked;
      sigemptyset(&unblocked);
      sigaddset(&unblocked, SIGSEGV);
      if (sigaction(SIGSEGV, &action, nullptr) != 0 ||
          sigprocmask(SIG_UNBLOCK, &unblocked, nullptr) != 0) _exit(2);
      *static_cast<volatile uint8_t*>(mapping) = 0x5a;
      _exit(1);
    }
    int status = 0;
    const bool reaped = child > 0 && ReapProtectionChild(child, &status);
    const int writable = mprotect(mapping, kPageSize, PROT_READ | PROT_WRITE);
    bool zero_and_writable = false;
    if (writable == 0) {
      auto* byte = static_cast<volatile uint8_t*>(mapping);
      const bool zero = *byte == 0;
      *byte = 0xa5;
      zero_and_writable = zero && *byte == 0xa5;
    }
    const int unmapped = munmap(mapping, kPageSize);
    ASSERT_GT(child, 0);
    ASSERT_TRUE(reaped);
    ASSERT_TRUE(WIFSIGNALED(status));
    EXPECT_EQ(SIGSEGV, WTERMSIG(status));
    EXPECT_EQ(0, writable);
    EXPECT_TRUE(zero_and_writable);
    EXPECT_EQ(0, unmapped);
  }
}

// A cached private-file first write must install its new anonymous copy writable.
TEST(PageFaultAccounting, PrivateFileFirstWriteNeedsOneFaultPerPage) {
  constexpr size_t kPages = 64;
  constexpr size_t kLength = kPages * kPageSize;
  std::vector<uint8_t> file_bytes(kLength, 0x31);
  int fd = -1;
  void* target = MAP_FAILED;
  void* view = MAP_FAILED;
  void* scratch = MAP_FAILED;
  char path[] = "/tmp/dunitest-private-fault-XXXXXX";
  bool linked = false;

  // Fatal assertions leave this scope, then the common cleanup below runs.
  [&] {
    fd = mkstemp(path);
    ASSERT_GE(fd, 0) << strerror(errno);
    linked = true;
    ASSERT_EQ(0, unlink(path)) << strerror(errno);
    linked = false;
    size_t done = 0;
    while (done < kLength) {
      const ssize_t n = pwrite(fd, file_bytes.data() + done, kLength - done, done);
      if (n < 0 && errno == EINTR) continue;
      ASSERT_GT(n, 0) << strerror(errno);
      done += static_cast<size_t>(n);
    }
    view = mmap(nullptr, kLength, PROT_READ, MAP_SHARED, fd, 0);
    ASSERT_NE(MAP_FAILED, view) << strerror(errno);
    const auto* original = static_cast<volatile const uint8_t*>(view);
    // Populate the file cache and independent view before creating the target.
    for (size_t i = 0; i < kLength; ++i) ASSERT_EQ(0x31, original[i]);
    target = mmap(nullptr, kLength, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    ASSERT_NE(MAP_FAILED, target) << strerror(errno);
    ASSERT_EQ(0, madvise(target, kLength, MADV_NOHUGEPAGE)) << strerror(errno);
    scratch = mmap(nullptr, kLength, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, scratch) << strerror(errno);
    auto* bytes = static_cast<volatile uint8_t*>(target);
    struct rusage usage[5] {};
    long results[5] {};
    for (auto& u : usage) ASSERT_EQ(0, syscall(SYS_getrusage, RUSAGE_THREAD, &u));
    WriteFaultPages(static_cast<volatile uint8_t*>(scratch), kPages, 0x5a);

    // No target pre-read, assertions, allocation or file I/O in this window.
    results[0] = syscall(SYS_getrusage, RUSAGE_THREAD, &usage[0]);
    results[1] = syscall(SYS_getrusage, RUSAGE_THREAD, &usage[1]);
    results[2] = syscall(SYS_getrusage, RUSAGE_THREAD, &usage[2]);
    WriteFaultPages(bytes, kPages, 0x5a);
    results[3] = syscall(SYS_getrusage, RUSAGE_THREAD, &usage[3]);
    WriteFaultPages(bytes, kPages, 0xa5);
    results[4] = syscall(SYS_getrusage, RUSAGE_THREAD, &usage[4]);

    for (long result : results) ASSERT_EQ(0, result);
    EXPECT_EQ(0, usage[1].ru_minflt - usage[0].ru_minflt);
    EXPECT_EQ(0, usage[2].ru_minflt - usage[1].ru_minflt);
    EXPECT_EQ(static_cast<long>(kPages), usage[3].ru_minflt - usage[2].ru_minflt);
    EXPECT_EQ(0, usage[4].ru_minflt - usage[3].ru_minflt);
    EXPECT_EQ(0, usage[4].ru_majflt - usage[0].ru_majflt);
    done = 0;
    while (done < kLength) {
      const ssize_t n = pread(fd, file_bytes.data() + done, kLength - done, done);
      if (n < 0 && errno == EINTR) continue;
      ASSERT_GT(n, 0) << strerror(errno);
      done += static_cast<size_t>(n);
    }
    bool isolated = true;
    bool private_contents = true;
    for (size_t i = 0; i < kLength; ++i) {
      isolated = isolated && file_bytes[i] == 0x31 && original[i] == 0x31;
      private_contents = private_contents &&
          bytes[i] == (i % kPageSize == 0 ? 0xa5 : 0x31);
    }
    EXPECT_TRUE(isolated);
    EXPECT_TRUE(private_contents);
    // Downgrading the newly writable private page must revoke write access.
    ASSERT_EQ(0, mprotect(target, kLength, PROT_READ));
    const pid_t child = fork();
    if (child == 0) {
      struct rlimit limit {0, 0};
      (void)setrlimit(RLIMIT_CORE, &limit);
      struct sigaction action {};
      action.sa_handler = SIG_DFL;
      sigemptyset(&action.sa_mask);
      sigset_t unblocked;
      sigemptyset(&unblocked);
      sigaddset(&unblocked, SIGSEGV);
      if (sigaction(SIGSEGV, &action, nullptr) != 0 ||
          sigprocmask(SIG_UNBLOCK, &unblocked, nullptr) != 0) _exit(2);
      bytes[0] = 0x77;
      _exit(1);
    }
    int status = 0;
    const bool reaped = child > 0 && ReapProtectionChild(child, &status);
    ASSERT_GT(child, 0);
    ASSERT_TRUE(reaped);
    ASSERT_TRUE(WIFSIGNALED(status));
    EXPECT_EQ(SIGSEGV, WTERMSIG(status));
    ASSERT_EQ(0, mprotect(target, kLength, PROT_READ | PROT_WRITE));
    bytes[0] = 0x77;
    EXPECT_EQ(0x77, bytes[0]);
    EXPECT_EQ(0x31, original[0]);
  }();
  if (scratch != MAP_FAILED) EXPECT_EQ(0, munmap(scratch, kLength));
  if (target != MAP_FAILED) EXPECT_EQ(0, munmap(target, kLength));
  if (view != MAP_FAILED) EXPECT_EQ(0, munmap(view, kLength));
  if (fd >= 0) EXPECT_EQ(0, close(fd));
  if (linked) EXPECT_EQ(0, unlink(path));
}

TEST(PageFaultAccounting, AnonymousWriteUpdatesTaskAndVmstat) {
  struct rusage before_usage {};
  struct rusage after_usage {};
  FaultStat before_stat {};
  FaultStat after_stat {};
  uint64_t before_vmstat = 0;
  uint64_t after_vmstat = 0;

  ASSERT_EQ(0, getrusage(RUSAGE_THREAD, &before_usage)) << strerror(errno);
  ASSERT_TRUE(ReadProcStat("/proc/self/task/" + std::to_string(getpid()) + "/stat",
                           &before_stat));
  ASSERT_TRUE(ReadVmstat("pgfault", &before_vmstat));

  void* mapping = MapAndTouch(kFaultPages);
  ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);

  ASSERT_EQ(0, getrusage(RUSAGE_THREAD, &after_usage)) << strerror(errno);
  ASSERT_TRUE(ReadProcStat("/proc/self/task/" + std::to_string(getpid()) + "/stat",
                           &after_stat));
  ASSERT_TRUE(ReadVmstat("pgfault", &after_vmstat));

  ASSERT_GE(after_usage.ru_minflt, before_usage.ru_minflt);
  ASSERT_GE(after_stat.minflt, before_stat.minflt);
  ASSERT_GE(after_vmstat, before_vmstat);
  EXPECT_GE(static_cast<uint64_t>(after_usage.ru_minflt - before_usage.ru_minflt), kFaultPages);
  EXPECT_GE(after_stat.minflt - before_stat.minflt, kFaultPages);
  EXPECT_GE(after_vmstat - before_vmstat, kFaultPages);
  EXPECT_EQ(0, munmap(mapping, kFaultPages * kPageSize));
}

TEST(PageFaultAccounting, ProcStatDistinguishesThreadAndThreadGroup) {
  int ready[2], start[2], done[2], finish[2];
  ASSERT_EQ(0, pipe(ready));
  ASSERT_EQ(0, pipe(start));
  ASSERT_EQ(0, pipe(done));
  ASSERT_EQ(0, pipe(finish));

  WorkerContext context{ready[1], start[0], done[1], finish[0], MAP_FAILED};
  pthread_t worker;
  ASSERT_EQ(0, pthread_create(&worker, nullptr, FaultWorker, &context));
  pid_t worker_tid = 0;
  const bool got_tid = ReadExact(ready[0], &worker_tid, sizeof(worker_tid));

  const std::string worker_path =
      "/proc/self/task/" + std::to_string(worker_tid) + "/stat";
  const std::string main_path = "/proc/self/task/" +
                                std::to_string(static_cast<pid_t>(syscall(SYS_gettid))) +
                                "/stat";
  FaultStat worker_before {}, worker_after {}, main_before {}, main_after {}, group_before {},
      group_after {};
  const bool baseline_ok = got_tid && ReadProcStat(worker_path, &worker_before) &&
                           ReadProcStat(main_path, &main_before) &&
                           ReadProcStat("/proc/self/stat", &group_before);

  const bool started = WriteByte(start[1]);
  char worker_status = 'e';
  const bool completed = started && ReadExact(done[0], &worker_status, sizeof(worker_status));
  const bool after_ok = completed && ReadProcStat(worker_path, &worker_after) &&
                        ReadProcStat(main_path, &main_after) &&
                        ReadProcStat("/proc/self/stat", &group_after);

  const bool finish_sent = WriteByte(finish[1]);
  const int join_result = pthread_join(worker, nullptr);
  for (int fd : {ready[0], ready[1], start[0], start[1], done[0], done[1],
                 finish[0], finish[1]})
    close(fd);

  ASSERT_TRUE(got_tid);
  ASSERT_TRUE(baseline_ok);
  ASSERT_TRUE(completed);
  ASSERT_EQ('x', worker_status);
  ASSERT_TRUE(after_ok);
  ASSERT_TRUE(finish_sent);
  ASSERT_EQ(0, join_result);
  ASSERT_NE(MAP_FAILED, context.mapping);
  ASSERT_GE(worker_after.minflt, worker_before.minflt);
  ASSERT_GE(main_after.minflt, main_before.minflt);
  ASSERT_GE(group_after.minflt, group_before.minflt);
  const uint64_t worker_delta = worker_after.minflt - worker_before.minflt;
  const uint64_t main_delta = main_after.minflt - main_before.minflt;
  const uint64_t group_delta = group_after.minflt - group_before.minflt;
  EXPECT_GE(worker_delta, kFaultPages);
  EXPECT_GE(group_delta, worker_delta);
  EXPECT_LT(main_delta, worker_delta);

  EXPECT_EQ(0, munmap(context.mapping, kFaultPages * kPageSize));
}

TEST(PageFaultAccounting, ReapedChildUpdatesChildrenUsage) {
  struct rusage before_children {};
  struct rusage after_children {};
  struct rusage waited {};
  FaultStat before_stat {};
  FaultStat after_stat {};
  ASSERT_EQ(0, getrusage(RUSAGE_CHILDREN, &before_children)) << strerror(errno);
  ASSERT_TRUE(ReadProcStat("/proc/self/stat", &before_stat));

  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    void* mapping = MapAndTouch(kFaultPages);
    _exit(mapping == MAP_FAILED ? 2 : 0);
  }

  int status = 0;
  ASSERT_EQ(child, wait4(child, &status, 0, &waited)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  ASSERT_EQ(0, WEXITSTATUS(status));
  ASSERT_EQ(0, getrusage(RUSAGE_CHILDREN, &after_children)) << strerror(errno);
  ASSERT_TRUE(ReadProcStat("/proc/self/stat", &after_stat));

  ASSERT_GE(after_children.ru_minflt, before_children.ru_minflt);
  ASSERT_GE(after_stat.cminflt, before_stat.cminflt);
  EXPECT_GE(static_cast<uint64_t>(waited.ru_minflt), kFaultPages);
  EXPECT_GE(static_cast<uint64_t>(after_children.ru_minflt - before_children.ru_minflt),
            static_cast<uint64_t>(waited.ru_minflt));
  EXPECT_GE(after_stat.cminflt - before_stat.cminflt,
            static_cast<uint64_t>(waited.ru_minflt));
}

TEST(PageFaultAccounting, SharedAnonymousDelayedFaultsRemainVisibleAcrossFork) {
  constexpr size_t kLength = 2 * kPageSize;
  void* raw_mapping =
      mmap(nullptr, kLength, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  ASSERT_NE(MAP_FAILED, raw_mapping) << strerror(errno);
  auto* mapping = static_cast<volatile uint8_t*>(raw_mapping);

  FaultStat before {};
  FaultStat after {};
  const std::string stat_path = "/proc/self/task/" + std::to_string(getpid()) + "/stat";
  if (!ReadProcStat(stat_path, &before)) {
    munmap(const_cast<uint8_t*>(mapping), kLength);
    FAIL() << "failed to read parent task stat";
    return;
  }

  int child_ready[2];
  int parent_ready[2];
  if (pipe(child_ready) != 0) {
    const int saved_errno = errno;
    munmap(const_cast<uint8_t*>(mapping), kLength);
    FAIL() << strerror(saved_errno);
    return;
  }
  if (pipe(parent_ready) != 0) {
    const int saved_errno = errno;
    close(child_ready[0]);
    close(child_ready[1]);
    munmap(const_cast<uint8_t*>(mapping), kLength);
    FAIL() << strerror(saved_errno);
    return;
  }

  pid_t child = fork();
  if (child < 0) {
    const int saved_errno = errno;
    close(child_ready[0]);
    close(child_ready[1]);
    close(parent_ready[0]);
    close(parent_ready[1]);
    munmap(const_cast<uint8_t*>(mapping), kLength);
    FAIL() << strerror(saved_errno);
    return;
  }
  if (child == 0) {
    close(child_ready[0]);
    close(parent_ready[1]);
    mapping[kPageSize] = 0x22;
    if (!WriteByte(child_ready[1]) || !ReadByte(parent_ready[0])) _exit(2);
    _exit(mapping[0] == 0x11 ? 0 : 3);
  }

  close(child_ready[1]);
  close(parent_ready[0]);
  const bool child_faulted = ReadByte(child_ready[0]);
  const bool child_value_visible = child_faulted && mapping[kPageSize] == 0x22;
  bool parent_faulted = false;
  if (child_faulted) {
    mapping[0] = 0x11;
    parent_faulted = WriteByte(parent_ready[1]);
  }
  close(parent_ready[1]);

  int status = 0;
  const pid_t waited = waitpid(child, &status, 0);
  close(child_ready[0]);
  const bool stat_ok = ReadProcStat(stat_path, &after);
  const int unmap_result = munmap(const_cast<uint8_t*>(mapping), kLength);

  ASSERT_TRUE(child_faulted);
  ASSERT_TRUE(child_value_visible);
  ASSERT_TRUE(parent_faulted);
  ASSERT_EQ(child, waited) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  ASSERT_EQ(0, WEXITSTATUS(status));
  ASSERT_TRUE(stat_ok);
  ASSERT_GE(after.minflt, before.minflt);
  EXPECT_GE(after.minflt - before.minflt, 2u);
  EXPECT_EQ(0, unmap_result);
}

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
