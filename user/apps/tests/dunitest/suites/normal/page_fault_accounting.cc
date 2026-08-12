#include <gtest/gtest.h>

#include <fcntl.h>
#include <pthread.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cerrno>
#include <cstdint>
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

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
