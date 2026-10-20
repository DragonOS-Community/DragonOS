#include <gtest/gtest.h>

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/utsname.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <sstream>
#include <string>
#include <vector>

namespace {
constexpr uint64_t MiB = 1024 * 1024;
constexpr uint64_t kWorkerBytes = 64 * MiB;
constexpr uint64_t kCaseBytes = 128 * MiB;
constexpr uint64_t kSecond = 1000000000;
namespace worker_op {
constexpr char InstallHighSignal = 's';
constexpr char ReadHighSignal = 'g';
constexpr char CloneSharedMm = 'Y';
constexpr char CloneVforkMm = 'v';
constexpr char EpollSetup = 'a';
constexpr char EpollAdd = 'b';
constexpr char EpollDuplicate = 'c';
constexpr char EpollModify = 'd';
constexpr char EpollDelete = 'f';
constexpr char EpollClose = 'l';
constexpr char CpuFastClock = 'e';
constexpr char CpuSetup = 'h';
constexpr char CpuStart = 'i';
constexpr char CpuStatus = 't';
constexpr char CpuInterrupt = 'n';
constexpr char CpuDestroy = 'o';
constexpr char Protect = 'p';
// Register every dispatched command here: duplicate wire values must fail
// compilation rather than silently making a later if-branch unreachable.
constexpr char All[] = {'G', InstallHighSignal, ReadHighSignal, 'M', 'S', 'D',
                       'T', 'V', 'Q', 'U', 'L', 'H', 'P', 'N', 'R', 'j', 'k',
                       'K', 'O', 'F', 'C', 'I', 'J', 'B', 'E', CloneSharedMm,
                       CloneVforkMm, 'Z', 'X', EpollSetup, EpollAdd,
                       EpollDuplicate, EpollModify, EpollDelete, EpollClose,
                       CpuFastClock, CpuSetup, CpuStart, CpuStatus, CpuInterrupt,
                       CpuDestroy, Protect};
constexpr bool Unique() {
  for (size_t i = 0; i < sizeof(All); ++i)
    for (size_t j = i + 1; j < sizeof(All); ++j)
      if (All[i] == All[j]) return false;
  return true;
}
static_assert(Unique(), "MemoryWorker command values must be unique");
}  // namespace worker_op
volatile sig_atomic_t high_signal_seen = 0;
void RecordHighSignal(int) { high_signal_seen = 1; }

uint64_t Now() {
  timespec t{};
  if (clock_gettime(CLOCK_MONOTONIC, &t)) return 0;
  return uint64_t(t.tv_sec) * kSecond + t.tv_nsec;
}

// Every retry consumes the same absolute deadline, including EINTR retries.
bool Transfer(int fd, void* data, size_t size, bool sending, uint64_t deadline) {
  auto* bytes = static_cast<char*>(data);
  while (size) {
    const uint64_t now = Now();
    if (!now || now >= deadline) return false;
    pollfd p{fd, short(sending ? POLLOUT : POLLIN), 0};
    int ms = int((deadline - now + 999999) / 1000000);
    int ready = poll(&p, 1, ms);
    if (ready < 0 && errno == EINTR) continue;
    if (ready <= 0 || !(p.revents & (sending ? POLLOUT : POLLIN))) return false;
    ssize_t n = sending ? write(fd, bytes, size) : read(fd, bytes, size);
    if (n < 0 && (errno == EINTR || errno == EAGAIN)) continue;
    if (n <= 0) return false;
    bytes += n;
    size -= n;
  }
  return true;
}

bool Reap(pid_t pid, int* status, uint64_t deadline) {
  for (;;) {
    uint64_t now = Now();
    if (!now || now >= deadline) return false;
    pid_t result = waitpid(pid, status, WNOHANG);
    if (result == pid) return true;
    if (result < 0 && errno != EINTR) return false;
    timespec pause{0, 1000000};
    nanosleep(&pause, nullptr);
  }
}

int Write(const std::string& path, const std::string& text) {
  int fd = open(path.c_str(), O_WRONLY | O_CLOEXEC);
  if (fd < 0) return errno;
  ssize_t n;
  do { n = write(fd, text.data(), text.size()); } while (n < 0 && errno == EINTR);
  int error = n < 0 ? errno : (n == ssize_t(text.size()) ? 0 : EIO);
  close(fd);
  return error;
}

std::string Read(const std::string& path) {
  int fd = open(path.c_str(), O_RDONLY | O_CLOEXEC);
  if (fd < 0) return {};
  std::string text;
  char data[4096];
  for (;;) {
    ssize_t n = read(fd, data, sizeof(data));
    if (n < 0 && errno == EINTR) continue;
    if (n <= 0) break;
    text.append(data, n);
    if (text.size() > 65536) break;
  }
  close(fd);
  return text;
}

uint64_t Number(const std::string& text) {
  std::istringstream input(text);
  input >> std::ws;
  if (input.peek() == '-') return UINT64_MAX;
  uint64_t n;
  if (!(input >> n)) return UINT64_MAX;
  input >> std::ws;
  return input.eof() ? n : UINT64_MAX;
}

uint64_t Field(const std::string& text, const std::string& name) {
  std::istringstream input(text);
  std::string key, value;
  while (input >> key >> value) if (key == name) return Number(value);
  return UINT64_MAX;
}

bool Has(const std::string& text, const std::string& token) {
  std::istringstream input(text);
  for (std::string item; input >> item;) if (item == token) return true;
  return false;
}

class InotifySource {
 public:
  InotifySource() : fd_(inotify_init1(IN_NONBLOCK | IN_CLOEXEC)) {}
  ~InotifySource() { if (fd_ >= 0) close(fd_); }
  int Watch(const std::string& path) { return inotify_add_watch(fd_, path.c_str(), IN_MODIFY); }
  bool valid() const { return fd_ >= 0; }
  // The event file itself is never open across a transition. Watch records
  // may coalesce, so require identities/masks, not an exact event count.
  bool Modified(const std::vector<int>& expected, uint64_t duration, bool positive = true,
                const std::string& required_name = "") {
    std::vector<int> seen;
    const uint64_t start = Now();
    if (!start) return false;
    const uint64_t deadline = start + duration;
    while (Now() && Now() < deadline) {
      pollfd p{fd_, POLLIN, 0};
      const uint64_t now = Now();
      if (!now || now >= deadline) break;
      int result = poll(&p, 1, int((deadline - now + 999999) / 1000000));
      if (result < 0 && errno == EINTR) continue;
      if (result < 0 || (p.revents & (POLLERR | POLLHUP | POLLNVAL))) return false;
      if (!result) continue;
      alignas(inotify_event) char data[4096];
      ssize_t bytes = read(fd_, data, sizeof(data));
      if (bytes < 0 && (errno == EAGAIN || errno == EINTR)) continue;
      if (bytes <= 0) return false;
      for (size_t offset = 0; offset < size_t(bytes);) {
        if (offset % alignof(inotify_event) || size_t(bytes) - offset < sizeof(inotify_event)) return false;
        auto* event = reinterpret_cast<const inotify_event*>(data + offset);
        const size_t length = sizeof(*event) + event->len;
        if (length > size_t(bytes) - offset || (event->mask & (IN_Q_OVERFLOW | IN_IGNORED))) return false;
        if (event->mask & IN_MODIFY) {
          if (!positive || std::find(expected.begin(), expected.end(), event->wd) == expected.end()) return false;
          // Directory watches can also see userspace writes to other files.
          // Never mistake those for a kernel notification of the event file.
          if (required_name.empty() || (event->len &&
              strnlen(event->name, event->len) < event->len &&
              required_name == event->name)) seen.push_back(event->wd);
        }
        offset += length;
      }
      if (positive && std::all_of(expected.begin(), expected.end(), [&](int wd) {
            return std::find(seen.begin(), seen.end(), wd) != seen.end();
          })) return true;
    }
    return !positive && Now() != 0;
  }
 private:
  int fd_;
};

// Policy writes can reclaim, throttle or OOM. The root runner must not be
// trapped inside that write and must never be migrated into a limited group.
int WriteBounded(const std::string& path, const std::string& text,
                 bool unprivileged = false) {
  pid_t child = fork();
  if (child < 0) return errno;
  if (!child) {
    if (unprivileged && (setgid(65534) || setuid(65534))) _exit(errno);
    _exit(Write(path, text));
  }
  int status = 0;
  if (!Reap(child, &status, Now() + 10 * kSecond)) {
    kill(child, SIGKILL);
    EXPECT_TRUE(Reap(child, &status, Now() + 2 * kSecond));
    return ETIMEDOUT;
  }
  return WIFEXITED(status) ? WEXITSTATUS(status) : ECHILD;
}

struct Command { char operation; uint64_t bytes; };
struct Reply { int error; pid_t child; };

void Touch(void* mapping, uint64_t bytes, bool write_pages) {
  volatile unsigned char* p = static_cast<volatile unsigned char*>(mapping);
  const size_t page = size_t(sysconf(_SC_PAGESIZE));
  for (uint64_t i = 0; i < bytes; i += page) {
    if (write_pages) p[i] = 37;
    else { volatile unsigned char value = p[i]; (void)value; }
  }
}

void* HoldThread(void* gate) {
  char token;
  Transfer(*static_cast<int*>(gate), &token, 1, false, Now() + 30 * kSecond);
  return nullptr;
}

constexpr unsigned kCpuSleepers = 32;
struct CpuSleepContext {
  int gate[2] = {-1, -1};
  std::atomic<unsigned> warm{0}, entered{0}, finished{0};
  std::atomic<int> error{0};
  std::atomic<bool> stop{false};
  std::atomic<uint64_t> target{0};
};

bool CpuGateRead(int fd) {
  char token;
  ssize_t bytes;
  do { bytes = read(fd, &token, 1); } while (bytes < 0 && errno == EINTR);
  return bytes == 1;
}

void* CpuSleeper(void* argument) {
  auto* context = static_cast<CpuSleepContext*>(argument);
  // Resolve libc/TLS/request and the actual signal-frame path before the
  // observer's baseline. A private alternate stack keeps later SIGUSR1 from
  // first-touching a previously unused ordinary-stack page.
  alignas(16) unsigned char alternate[16384];
  volatile unsigned char ordinary[32768];
  for (size_t i = 0; i < sizeof(ordinary); i += 4096) ordinary[i] = 1;
  memset(alternate, 0, sizeof(alternate));
  stack_t stack{}; stack.ss_sp = alternate; stack.ss_size = sizeof(alternate);
  sigset_t signals; sigemptyset(&signals); sigaddset(&signals, SIGUSR1);
  int error = pthread_sigmask(SIG_UNBLOCK, &signals, nullptr);
  if (!error && sigaltstack(&stack, nullptr)) error = errno;
  timespec request{};
  if (!error) error = clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, 0, &request, nullptr);
  if (!error) error = pthread_kill(pthread_self(), SIGUSR1);
  if (error) { context->error.store(error); return nullptr; }
  context->warm.fetch_add(1, std::memory_order_release);
  if (!CpuGateRead(context->gate[0])) { context->error.store(EIO); return nullptr; }
  const uint64_t target = context->target.load(std::memory_order_acquire);
  request = {static_cast<time_t>(target / kSecond), static_cast<long>(target % kSecond)};
  context->entered.fetch_add(1, std::memory_order_release);
  error = clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, TIMER_ABSTIME, &request, nullptr);
  if (error != EINTR) context->error.store(error ? error : EPROTO);
  context->finished.fetch_add(1, std::memory_order_release);
  // Keep every task/kernel stack alive while registration refunds are sampled.
  if (!CpuGateRead(context->gate[0]) || !context->stop.load()) context->error.store(EIO);
  stack.ss_flags = SS_DISABLE;
  if (sigaltstack(&stack, nullptr)) context->error.store(errno);
  return nullptr;
}

int CpuTokens(int fd) {
  char tokens[kCpuSleepers]{};
  size_t offset = 0;
  const uint64_t end = Now() + 10 * kSecond;
  while (offset < sizeof(tokens)) {
    if (!Now() || Now() >= end) return ETIMEDOUT;
    ssize_t bytes = write(fd, tokens + offset, sizeof(tokens) - offset);
    if (bytes < 0 && errno == EINTR) continue;
    if (bytes <= 0) return bytes < 0 ? errno : EIO;
    offset += size_t(bytes);
  }
  return 0;
}

struct SharedMmGate { int command; int reply; };

int HoldSharedMm(void* argument) {
  auto* gate = static_cast<SharedMmGate*>(argument);
  char token;
  if (!Transfer(gate->command, &token, 1, false, Now() + 30 * kSecond)) return 95;
  // Ordinary CLONE_VM groups synchronize oom_score_adj through procfs.
  // Do not accidentally protect the victim while testing shared-mm cleanup.
  token = 'I';
  if (!Transfer(gate->reply, &token, 1, true, Now() + 10 * kSecond)) return 97;
  Transfer(gate->command, &token, 1, false, Now() + 30 * kSecond);
  return 0;
}

struct VforkMmGate { int reply; int hold; };

int HoldVforkMm(void* argument) {
  auto* gate = static_cast<VforkMmGate*>(argument);
  // The parent is suspended in clone. Report from the child, without waiting
  // for clone to return or touching the root runner's unrelated address space.
  Reply ready{0, getpid()};
  if (!Transfer(gate->reply, &ready, sizeof(ready), true, Now() + 10 * kSecond)) return 96;
  char token;
  Transfer(gate->hold, &token, 1, false, Now() + 30 * kSecond);
  return 0;
}

[[noreturn]] void RunWorker(int requests, int replies, const std::string& path,
                           int cpu_read = -1, int cpu_write = -1) {
  void* mapping = MAP_FAILED;
  uint64_t length = 0;
  int file = -1;
  pid_t shared_child = -1;
  int cow_gate = -1, cow_reply = -1;
  pthread_t threads[4];
  unsigned thread_count = 0;
  int thread_gate[2] = {-1, -1};
  int sockets[2] = {-1, -1};
  int object_fds[128];
  unsigned object_count = 0;
  SharedMmGate shared_mm{};
  CpuSleepContext cpu;
  cpu.gate[0] = cpu_read; cpu.gate[1] = cpu_write;
  pthread_t cpu_threads[kCpuSleepers];
  unsigned cpu_count = 0;
  for (;;) {
    Command command{};
    if (!Transfer(requests, &command, sizeof(command), false,
                  Now() + 30 * kSecond)) _exit(90);
    Reply reply{};
    if (command.bytes > kWorkerBytes) reply.error = E2BIG;
    else if (command.operation == 'G') {
      // The runner has already moved this process before opening the gate.
    } else if (command.operation == worker_op::CpuFastClock) {
      const timespec zero{};
      for (unsigned i = 0; !reply.error && i < 128; ++i) {
        reply.error = clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, TIMER_ABSTIME, &zero, nullptr);
        if (!reply.error) reply.error = clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, 0, &zero, nullptr);
      }
    } else if (command.operation == worker_op::CpuSetup) {
      if (cpu_count || cpu.gate[0] < 0) reply.error = EINVAL;
      else {
        struct sigaction action{}; action.sa_handler = RecordHighSignal;
        action.sa_flags = SA_ONSTACK; sigemptyset(&action.sa_mask);
        if (sigaction(SIGUSR1, &action, nullptr)) _exit(110);
        pthread_attr_t attributes;
        if (pthread_attr_init(&attributes) ||
            pthread_attr_setstacksize(&attributes, 128 * 1024)) _exit(111);
        while (cpu_count < kCpuSleepers) {
          if (pthread_create(&cpu_threads[cpu_count], &attributes, CpuSleeper, &cpu)) _exit(112);
          ++cpu_count;
        }
        pthread_attr_destroy(&attributes);
        const uint64_t end = Now() + 10 * kSecond;
        while (cpu.warm.load(std::memory_order_acquire) != kCpuSleepers && !cpu.error.load()) {
          if (!Now() || Now() >= end) _exit(113);
          usleep(1000);
        }
        reply.error = cpu.error.load(); reply.child = cpu.warm.load();
      }
    } else if (command.operation == worker_op::CpuStart) {
      timespec now{};
      if (cpu_count != kCpuSleepers || clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &now))
        reply.error = EINVAL;
      else {
        cpu.target.store(uint64_t(now.tv_sec) * kSecond + now.tv_nsec + 10 * kSecond,
                         std::memory_order_release);
        reply.error = CpuTokens(cpu.gate[1]);
      }
    } else if (command.operation == worker_op::CpuStatus) {
      reply.error = cpu.error.load();
      reply.child = command.bytes == 0 ? cpu.warm.load() :
                    command.bytes == 1 ? cpu.entered.load() : cpu.finished.load();
    } else if (command.operation == worker_op::CpuInterrupt) {
      if (cpu_count != kCpuSleepers) reply.error = EINVAL;
      else for (unsigned i = 0; i < cpu_count; ++i) {
        int error = pthread_kill(cpu_threads[i], SIGUSR1);
        if (error) { reply.error = error; break; }
      }
    } else if (command.operation == worker_op::CpuDestroy) {
      if (cpu_count != kCpuSleepers || cpu.finished.load() != kCpuSleepers) reply.error = EINVAL;
      else {
        cpu.stop.store(true);
        if (CpuTokens(cpu.gate[1])) _exit(114);
        for (unsigned i = 0; i < cpu_count; ++i)
          if (pthread_join(cpu_threads[i], nullptr)) _exit(115);
        cpu_count = 0; reply.error = cpu.error.load();
      }
    } else if (command.operation == worker_op::InstallHighSignal) {
      struct sigaction action{};
      action.sa_handler = RecordHighSignal;
      sigemptyset(&action.sa_mask);
      high_signal_seen = 0;
      if (sigaction(SIGUSR1, &action, nullptr)) reply.error = errno;
    } else if (command.operation == worker_op::ReadHighSignal) {
      reply.child = high_signal_seen;
    } else if (command.operation == 'M' || command.operation == 'S' ||
               command.operation == 'D') {
      if (mapping != MAP_FAILED) reply.error = EBUSY;
      else {
        int flags = MAP_ANONYMOUS |
                    (command.operation == 'M' ? MAP_PRIVATE : MAP_SHARED);
        if (command.operation == 'D') {
          file = int(syscall(SYS_memfd_create, "memcg-test", 0));
          if (file < 0 || ftruncate(file, command.bytes)) reply.error = errno;
          flags = MAP_SHARED;
        }
        if (!reply.error) {
          mapping = mmap(nullptr, command.bytes, PROT_READ | PROT_WRITE,
                         flags, file, 0);
          if (mapping == MAP_FAILED) reply.error = errno;
          else length = command.bytes;
        }
      }
    } else if (command.operation == 'T' || command.operation == 'V') {
      if (mapping == MAP_FAILED) reply.error = EINVAL;
      else if (command.bytes > length) reply.error = EINVAL;
      else Touch(mapping, command.bytes ? command.bytes : length,
                 command.operation == 'T');
    } else if (command.operation == worker_op::Protect) {
      if (mapping == MAP_FAILED ||
          (command.bytes & ~uint64_t(PROT_READ | PROT_WRITE | PROT_EXEC)))
        reply.error = EINVAL;
      else if (mprotect(mapping, length, int(command.bytes))) reply.error = errno;
    } else if (command.operation == 'Q') {
      if (mapping != MAP_FAILED) reply.error = EBUSY;
      else {
        length = 64 * MiB;
        mapping = mmap(nullptr, length, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (mapping == MAP_FAILED) reply.error = errno;
        else {
          volatile unsigned char* bytes = static_cast<volatile unsigned char*>(mapping);
          for (uint64_t off = 0; off < length; off += 2 * MiB) bytes[off] = 7;
        }
      }
    } else if (command.operation == 'U') {
      if (mapping != MAP_FAILED && munmap(mapping, length)) reply.error = errno;
      else { mapping = MAP_FAILED; length = 0; }
    } else if (command.operation == 'L') {
      if (file >= 0) close(file);
      file = -1;
    } else if (command.operation == 'H') {
      if (file < 0 || ftruncate(file, 0)) reply.error = errno;
    } else if (command.operation == 'P') {
      file = open(path.c_str(), O_RDWR | O_CREAT | O_EXCL, 0600);
      if (file < 0) reply.error = errno;
      struct statfs fs{};
      if (!reply.error && fstatfs(file, &fs)) reply.error = errno;
      // Protection tests require reclaimable ordinary file cache, not the
      // unswappable shmem created by a tmpfs /tmp. Configure a disk-backed
      // DUNITEST_MEMORY_FILE_DIR when testing such a Linux installation.
      if (!reply.error && (fs.f_type == 0x01021994 || fs.f_type == 0x858458f6))
        reply.error = EOPNOTSUPP;
      char page[4096];
      memset(page, 0x5a, sizeof(page));
      for (uint64_t off = 0; !reply.error && off < command.bytes; off += sizeof(page)) {
        ssize_t n = pwrite(file, page, sizeof(page), off);
        if (n != ssize_t(sizeof(page))) reply.error = n < 0 ? errno : EIO;
      }
      if (!reply.error && fsync(file)) reply.error = errno;
    } else if (command.operation == 'N') {
      char page[4096];
      if (file < 0) reply.error = EBADF;
      for (uint64_t off = 0; !reply.error && off < command.bytes; off += sizeof(page)) {
        ssize_t n = pread(file, page, sizeof(page), off);
        if (n != ssize_t(sizeof(page))) reply.error = n < 0 ? errno : EIO;
        else if (page[0] != 0x5a) reply.error = EIO;
      }
      memset(page, 0, sizeof(page));
    } else if (command.operation == 'R') {
      if (file < 0) file = open(path.c_str(), O_RDONLY);
      if (file < 0) reply.error = errno;
      else {
        mapping = mmap(nullptr, command.bytes, PROT_READ, MAP_SHARED, file, 0);
        if (mapping == MAP_FAILED) reply.error = errno;
        else length = command.bytes;
      }
    } else if (command.operation == worker_op::EpollSetup) {
      if (object_count) reply.error = EBUSY;
      else {
        int epoll = epoll_create1(EPOLL_CLOEXEC);
        if (epoll < 0) reply.error = errno;
        else {
          object_fds[object_count++] = epoll;
          for (unsigned i = 0; i < 64 && !reply.error; ++i) {
            int event = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
            if (event < 0) reply.error = errno;
            else object_fds[object_count++] = event;
          }
        }
      }
    } else if (command.operation == worker_op::EpollAdd ||
               command.operation == worker_op::EpollDuplicate ||
               command.operation == worker_op::EpollModify ||
               command.operation == worker_op::EpollDelete) {
      if (object_count != 65 || object_fds[0] < 0) reply.error = EINVAL;
      else {
        int operation = command.operation == worker_op::EpollDelete ? EPOLL_CTL_DEL :
                        command.operation == worker_op::EpollModify ? EPOLL_CTL_MOD : EPOLL_CTL_ADD;
        for (unsigned i = 1; i < object_count && !reply.error; ++i) {
          epoll_event event{};
          event.events = EPOLLIN;
          event.data.u64 = i;
          int result = epoll_ctl(object_fds[0], operation, object_fds[i], &event);
          if (command.operation == worker_op::EpollDuplicate) {
            if (result != -1 || errno != EEXIST) reply.error = EIO;
          } else if (result) reply.error = errno;
        }
      }
    } else if (command.operation == worker_op::EpollClose) {
      if (object_count != 65 || object_fds[0] < 0) reply.error = EINVAL;
      else { close(object_fds[0]); object_fds[0] = -1; }
    } else if (command.operation == 'j') {
      if (object_count) reply.error = EBUSY;
      else {
        // Ordinary syscall-created objects, not a kernel accounting hook.
        // Fixed storage bounds both userspace memory and fd pressure.
        for (unsigned i = 0; i < 64 && !reply.error; ++i) {
          int event = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
          if (event < 0) { reply.error = errno; break; }
          object_fds[object_count++] = event;
          int epoll = epoll_create1(EPOLL_CLOEXEC);
          if (epoll < 0) { reply.error = errno; break; }
          object_fds[object_count++] = epoll;
        }
      }
    } else if (command.operation == 'k') {
      while (object_count) {
        if (close(object_fds[--object_count])) reply.error = errno;
      }
    } else if (command.operation == 'K') {
      if (sockets[0] >= 0 || sockets[1] >= 0) reply.error = EBUSY;
      else {
        int listener = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
        sockaddr_in address{};
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        socklen_t address_size = sizeof(address);
        if (listener < 0 || bind(listener, reinterpret_cast<sockaddr*>(&address), sizeof(address)) ||
            getsockname(listener, reinterpret_cast<sockaddr*>(&address), &address_size) ||
            listen(listener, 1)) reply.error = errno;
        if (!reply.error) {
          sockets[0] = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
          if (sockets[0] < 0) reply.error = errno;
        }
        const uint64_t deadline = Now() + 8 * kSecond;
        if (!reply.error && connect(sockets[0], reinterpret_cast<sockaddr*>(&address), sizeof(address))) {
          if (errno != EINPROGRESS) reply.error = errno;
          else {
            pollfd connected{sockets[0], POLLOUT, 0};
            for (;;) {
              const uint64_t now = Now();
              if (!now || now >= deadline) { reply.error = ETIMEDOUT; break; }
              int ready = poll(&connected, 1, int((deadline - now + 999999) / 1000000));
              if (ready < 0 && errno == EINTR) continue;
              if (ready <= 0) { reply.error = ready < 0 ? errno : ETIMEDOUT; break; }
              socklen_t size = sizeof(reply.error);
              if (getsockopt(sockets[0], SOL_SOCKET, SO_ERROR, &reply.error, &size)) reply.error = errno;
              break;
            }
          }
        }
        while (!reply.error) {
          sockets[1] = accept4(listener, nullptr, nullptr, SOCK_NONBLOCK | SOCK_CLOEXEC);
          if (sockets[1] >= 0) break;
          if (errno != EINTR && errno != EAGAIN) { reply.error = errno; break; }
          const uint64_t now = Now();
          if (!now || now >= deadline) { reply.error = ETIMEDOUT; break; }
          pollfd pending{listener, POLLIN, 0};
          int ready = poll(&pending, 1, int((deadline - now + 999999) / 1000000));
          if (ready < 0 && errno == EINTR) continue;
          if (ready <= 0) { reply.error = ready < 0 ? errno : ETIMEDOUT; break; }
        }
        if (listener >= 0) close(listener);
        if (!reply.error) {
          // Keep actual queued TCP data, not AF_UNIX or merely fd metadata.
          // Linux charges network buffers lazily; DragonOS may reserve its
          // actual backing at birth. Do not equate payload and charged bytes.
          char data[4096];
          memset(data, 0x6b, sizeof(data));
          size_t sent = 0;
          while (sent < MiB) {
            const uint64_t now = Now();
            if (!now || now >= deadline) { reply.error = ETIMEDOUT; break; }
            ssize_t n = send(sockets[0], data, sizeof(data), MSG_DONTWAIT | MSG_NOSIGNAL);
            if (n > 0) { sent += size_t(n); continue; }
            if (n < 0 && errno == EINTR) continue;
            if (n < 0 && errno == EAGAIN && sent) break;
            reply.error = n < 0 ? errno : EIO;
            break;
          }
        }
        if (reply.error) {
          for (int& fd : sockets) { if (fd >= 0) close(fd); fd = -1; }
        }
      }
    } else if (command.operation == 'O') {
      for (int& fd : sockets) {
        if (fd >= 0) {
          // Abort close avoids making this accounting test depend on a
          // transport FIN/TIME_WAIT timeout; real backing still must release.
          linger abort_close{1, 0};
          if (setsockopt(fd, SOL_SOCKET, SO_LINGER, &abort_close, sizeof(abort_close))) reply.error = errno;
          if (close(fd)) reply.error = errno;
          fd = -1;
        }
      }
    } else if (command.operation == 'F') {
      if (mapping == MAP_FAILED || shared_child > 0) reply.error = EINVAL;
      else {
        int gate[2], done[2];
        if (pipe(gate)) reply.error = errno;
        else if (pipe(done)) { reply.error = errno; close(gate[0]); close(gate[1]); }
        else {
          shared_child = fork();
          if (shared_child == 0) {
            close(gate[1]); close(done[0]);
            close(requests); close(replies);
            char token;
            if (!Transfer(gate[0], &token, 1, false, Now() + 30 * kSecond)) _exit(91);
            Touch(mapping, length, true);
            token = 'D';
            if (!Transfer(done[1], &token, 1, true, Now() + 10 * kSecond)) _exit(92);
            // Retain COW pages until the root runner cleans the process group.
            while (Transfer(gate[0], &token, 1, false, Now() + 30 * kSecond)) {}
            _exit(0);
          }
          close(gate[0]); close(done[1]);
          cow_gate = gate[1]; cow_reply = done[0];
          if (shared_child < 0) reply.error = errno;
          reply.child = shared_child;
        }
      }
    } else if (command.operation == 'C') {
      char token = 'C';
      if (shared_child <= 0 ||
          !Transfer(cow_gate, &token, 1, true, Now() + 10 * kSecond) ||
          !Transfer(cow_reply, &token, 1, false, Now() + 10 * kSecond))
        reply.error = ETIMEDOUT;
    } else if (command.operation == 'I') {
      reply.error = Write("/proc/self/oom_score_adj", "-1000");
    } else if (command.operation == 'J') {
      reply.error = Write("/proc/self/oom_score_adj", "1000");
    } else if (command.operation == 'B') {
      if (thread_count) reply.error = EBUSY;
      else if (pipe(thread_gate)) reply.error = errno;
      else {
        pthread_attr_t attributes;
        reply.error = pthread_attr_init(&attributes);
        if (!reply.error) {
          reply.error = pthread_attr_setstacksize(&attributes, 256 * 1024);
          while (!reply.error && thread_count < 4) {
            reply.error = pthread_create(&threads[thread_count], &attributes,
                                         HoldThread, &thread_gate[0]);
            if (!reply.error) ++thread_count;
          }
          pthread_attr_destroy(&attributes);
        }
      }
    } else if (command.operation == 'E') {
      char tokens[4] = {'E', 'E', 'E', 'E'};
      if (!thread_count || !Transfer(thread_gate[1], tokens, thread_count, true,
                                    Now() + 10 * kSecond)) reply.error = EINVAL;
      else {
        for (unsigned i = 0; i < thread_count; ++i) {
          int error = pthread_join(threads[i], nullptr);
          if (error) reply.error = error;
        }
        thread_count = 0;
        close(thread_gate[0]); close(thread_gate[1]);
      }
    } else if (command.operation == worker_op::CloneVforkMm) {
      int gate[2];
      if (pipe(gate)) reply.error = errno;
      else {
        void* stack = mmap(nullptr, 128 * 1024, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (stack == MAP_FAILED) reply.error = errno;
        else {
          VforkMmGate child_gate{replies, gate[0]};
          pid_t child = clone(HoldVforkMm, static_cast<char*>(stack) + 128 * 1024,
                              CLONE_VM | CLONE_VFORK | SIGCHLD, &child_gate);
          if (child < 0) reply.error = errno;
          else reply.error = ECHILD;  // Returning before root OOM is not readiness.
          munmap(stack, 128 * 1024);
        }
        close(gate[0]); close(gate[1]);
      }
    } else if (command.operation == worker_op::CloneSharedMm) {
      if (shared_child > 0) reply.error = EBUSY;
      else {
        int gate[2], done[2];
        if (pipe(gate)) reply.error = errno;
        else if (pipe(done)) { reply.error = errno; close(gate[0]); close(gate[1]); }
        else {
          void* stack = mmap(nullptr, 128 * 1024, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
          if (stack == MAP_FAILED) {
            reply.error = errno;
            close(gate[0]); close(gate[1]); close(done[0]); close(done[1]);
          } else {
            shared_mm = {gate[0], done[1]};
            shared_child = clone(HoldSharedMm, static_cast<char*>(stack) + 128 * 1024,
                                 CLONE_VM | SIGCHLD, &shared_mm);
            if (shared_child < 0) reply.error = errno;
            // Separate fd tables: the child's ends remain valid after these closes.
            close(gate[0]); close(done[1]);
            cow_gate = gate[1]; cow_reply = done[0];
            reply.child = shared_child;
          }
        }
      }
    } else if (command.operation == 'Z') {
      char token = 'I';
      if (shared_child <= 0 ||
          !Transfer(cow_gate, &token, 1, true, Now() + 10 * kSecond) ||
          !Transfer(cow_reply, &token, 1, false, Now() + 10 * kSecond))
        reply.error = ETIMEDOUT;
    } else if (command.operation == 'X') {
      _exit(0);
    } else reply.error = EINVAL;
    if (!Transfer(replies, &reply, sizeof(reply), true, Now() + 10 * kSecond)) _exit(93);
  }
}

class MemoryWorker {
 public:
  MemoryWorker(const std::string& group, const std::string& file, bool cpu_mode = false) {
    int request[2], reply[2];
    if (pipe(request)) { ADD_FAILURE() << strerror(errno); return; }
    if (pipe(reply)) {
      ADD_FAILURE() << strerror(errno); close(request[0]); close(request[1]); return;
    }
    int cpu_gate[2] = {-1, -1};
    if (cpu_mode) {
      char token = 'W';
      // Ring and descriptor ownership belongs to the root constructor. No
      // fresh pipe stock is attributed to a sleeper after the baseline.
      if (pipe(cpu_gate) || write(cpu_gate[1], &token, 1) != 1 ||
          read(cpu_gate[0], &token, 1) != 1) {
        ADD_FAILURE() << "CPU gate setup: " << strerror(errno);
        for (int fd : {request[0], request[1], reply[0], reply[1], cpu_gate[0], cpu_gate[1]})
          if (fd >= 0) close(fd);
        return;
      }
    }
    pid_ = fork();
    if (pid_ == 0) {
      close(request[1]); close(reply[0]);
      // Do not retain another worker's control pipe: its OOM death must
      // become EOF immediately, not only after all sibling workers exit.
      for (int fd : controls_) close(fd);
      if (setpgid(0, 0)) _exit(94);
      RunWorker(request[0], reply[1], file, cpu_gate[0], cpu_gate[1]);
    }
    close(request[0]); close(reply[1]);
    for (int fd : cpu_gate) if (fd >= 0) close(fd);
    request_ = request[1]; reply_ = reply[0];
    controls_.push_back(request_);
    controls_.push_back(reply_);
    if (pid_ < 0) { ADD_FAILURE() << strerror(errno); Stop(); return; }
    // Both sides establish the process group before a gate can be opened.
    EXPECT_TRUE(setpgid(pid_, pid_) == 0 || errno == EACCES);
    int error = Write(group + "/cgroup.procs", std::to_string(pid_));
    EXPECT_EQ(0, error);
    if (!error) { EXPECT_EQ(0, Ask('G').error); }
    else Stop();  // Failed admission must never run pressure in root.
  }
  ~MemoryWorker() { Stop(); }
  MemoryWorker(const MemoryWorker&) = delete;
  MemoryWorker& operator=(const MemoryWorker&) = delete;
  pid_t pid() const { return pid_; }

  Reply Ask(char op, uint64_t bytes = 0) {
    if (!Begin(op, bytes)) return {ETIMEDOUT, -1};
    return Finish();
  }
  Reply AskUntil(char op, uint64_t bytes, uint64_t deadline) {
    Command command{op, bytes};
    Reply result{ETIMEDOUT, -1};
    if (!Transfer(request_, &command, sizeof(command), true, deadline) ||
        !Transfer(reply_, &result, sizeof(result), false, deadline)) return {ETIMEDOUT, -1};
    return result;
  }
  bool Begin(char op, uint64_t bytes = 0) {
    Command command{op, bytes};
    return Transfer(request_, &command, sizeof(command), true, Now() + 10 * kSecond);
  }
  Reply Finish() {
    Reply result{ETIMEDOUT, -1};
    if (!Transfer(reply_, &result, sizeof(result), false, Now() + 10 * kSecond))
      return {ETIMEDOUT, -1};
    return result;
  }
  bool ReplyReadyFor(uint64_t duration) {
    const uint64_t start = Now();
    if (!start) { ADD_FAILURE() << "CLOCK_MONOTONIC failed"; return true; }
    const uint64_t end = start + duration;
    for (;;) {
      const uint64_t now = Now();
      if (!now) { ADD_FAILURE() << "CLOCK_MONOTONIC failed"; return true; }
      if (now >= end) return false;
      pollfd p{reply_, POLLIN, 0};
      int result = poll(&p, 1, int((end - now + 999999) / 1000000));
      if (result < 0 && errno == EINTR) continue;
      if (result < 0) { ADD_FAILURE() << strerror(errno); return true; }
      if (result > 0) return true;  // EOF/errors are not a successful delay.
    }
  }
  bool Wait(int* status) {
    if (pid_ <= 0) return false;
    const uint64_t end = Now() + 10 * kSecond;
    for (;;) {
      uint64_t now = Now();
      if (!now || now >= end) return false;
      siginfo_t info{};
      int result = waitid(P_PID, pid_, &info, WEXITED | WNOHANG | WNOWAIT);
      if (result < 0 && errno != EINTR) return false;
      if (!result && info.si_pid == pid_) break;
      usleep(1000);
    }
    // WNOWAIT pins our leader PID while we kill any COW descendants. Once
    // the leader is reaped, its numeric pgid is not safe to use again.
    kill(-pid_, SIGKILL);
    if (!Reap(pid_, status, Now() + 2 * kSecond)) return false;
    pid_ = -1;
    return true;
  }
  bool Alive() const { return pid_ > 0 && kill(pid_, 0) == 0; }
  void Stop() {
    if (pid_ > 0) {
      kill(-pid_, SIGKILL);
      kill(pid_, SIGKILL);
      int status = 0;
      EXPECT_TRUE(Reap(pid_, &status, Now() + 2 * kSecond));
      pid_ = -1;
    }
    for (int fd : {request_, reply_}) {
      if (fd >= 0) {
        close(fd);
        controls_.erase(std::remove(controls_.begin(), controls_.end(), fd), controls_.end());
      }
    }
    request_ = reply_ = -1;
  }
 private:
  pid_t pid_ = -1;
  int request_ = -1, reply_ = -1;
  inline static std::vector<int> controls_;
};

class CgroupMemory : public ::testing::Test {
 protected:
  void SetUp() override {
    ASSERT_EQ(0u, geteuid()) << "memory controller tests require root";
    const char* supplied = getenv("DUNITEST_MEMORY_PARENT");
    parent_ = supplied ? supplied : "/sys/fs/cgroup";
    ASSERT_TRUE(Has(Read(parent_ + "/cgroup.controllers"), "memory"));
    if (!Has(Read(parent_ + "/cgroup.subtree_control"), "memory")) {
      ASSERT_EQ(0, Write(parent_ + "/cgroup.subtree_control", "+memory"));
      enabled_ = true;
    }
    static unsigned sequence = 0;
    group_ = Make(parent_, "dunitest-memory-" + std::to_string(getpid()) + "-" +
                           std::to_string(sequence++));
    ASSERT_FALSE(group_.empty());
    ASSERT_EQ(0, Write(group_ + "/cgroup.subtree_control", "+memory"));
    // The tested DragonOS platform has no swap. Match that boundary when
    // this same suite is run on a Linux host which otherwise has swap.
    if (!access((group_ + "/memory.swap.max").c_str(), F_OK)) {
      ASSERT_EQ(0, Write(group_ + "/memory.swap.max", "0"));
    }
  }
  void TearDown() override {
    workers_.clear();
    for (int fd : fds_) close(fd);
    for (auto& file : files_) EXPECT_TRUE(unlink(file.c_str()) == 0 || errno == ENOENT);
    if (!scratch_dir_.empty()) { EXPECT_EQ(0, rmdir(scratch_dir_.c_str())); }
    for (auto it = dirs_.rbegin(); it != dirs_.rend(); ++it) {
      const uint64_t end = Now() + 2 * kSecond;
      for (;;) {
        if (!rmdir(it->c_str()) || errno == ENOENT) break;
        uint64_t now = Now();
        if (errno != EBUSY || !now || now >= end) {
          ADD_FAILURE() << "rmdir " << *it << ": " << strerror(errno); break;
        }
        usleep(1000);
      }
    }
    if (enabled_) { EXPECT_EQ(0, Write(parent_ + "/cgroup.subtree_control", "-memory")); }
  }
  std::string Make(const std::string& parent, const std::string& name) {
    std::string path = parent + "/" + name;
    if (mkdir(path.c_str(), 0755)) { ADD_FAILURE() << strerror(errno); return {}; }
    dirs_.push_back(path);
    return path;
  }
  std::string Leaf(const char* name = "leaf") { return Make(group_, name); }
  MemoryWorker* Worker(const std::string& group, const std::string& file = "", bool cpu_mode = false) {
    if (workers_.size() >= 4) { ADD_FAILURE() << "worker bound exceeded"; return nullptr; }
    workers_.push_back(std::make_unique<MemoryWorker>(group, file, cpu_mode));
    return workers_.back().get();
  }
  bool Allocate(MemoryWorker* worker, uint64_t bytes, char type = 'M') {
    if (bytes > kWorkerBytes || reserved_ + bytes > kCaseBytes) return false;
    reserved_ += bytes;
    return worker && worker->Ask(type, bytes).error == 0 && worker->Ask('T').error == 0;
  }
  uint64_t Current(const std::string& group) {
    return Scalar(group + "/memory.current");
  }
  uint64_t Scalar(const std::string& path) {
    uint64_t value = Number(Read(path));
    if (value == UINT64_MAX) {
      ADD_FAILURE() << path << ": missing or invalid value";
      return 0;
    }
    return value;
  }
  uint64_t Stat(const std::string& group, const char* name) {
    uint64_t value = Field(Read(group + "/memory.stat"), name);
    if (value == UINT64_MAX) {
      ADD_FAILURE() << group << "/memory.stat: missing or invalid " << name;
      return 0;
    }
    return value;
  }
  uint64_t Event(const std::string& group, const char* name, bool local = false) {
    uint64_t value = Field(Read(group + (local ? "/memory.events.local" : "/memory.events")), name);
    if (value == UINT64_MAX) {
      ADD_FAILURE() << group << "/memory.events: missing or invalid " << name;
      return 0;
    }
    return value;
  }
  template<class Predicate> bool Eventually(Predicate predicate) {
    uint64_t end = Now() + 5 * kSecond;
    for (;;) {
      if (predicate()) return true;
      uint64_t now = Now();
      if (!now || now >= end) break;
      usleep(1000);
    }
    return predicate();
  }
  std::string Scratch() {
    if (scratch_dir_.empty()) {
      const char* supplied = getenv("DUNITEST_MEMORY_FILE_DIR");
      // File-cache protection must use ordinary reclaimable pages, not
      // unswappable shmem from a tmpfs /tmp. /var/tmp is the persistent
      // temporary directory; an explicit override must satisfy the same rule.
      const char* directory = supplied ? supplied : "/var/tmp";
      struct statfs fs{};
      if (statfs(directory, &fs)) {
        ADD_FAILURE() << directory << ": " << strerror(errno);
        return {};
      }
      if (fs.f_type == 0x01021994 || fs.f_type == 0x858458f6) {
        ADD_FAILURE() << directory
                      << ": set DUNITEST_MEMORY_FILE_DIR to a disk-backed filesystem";
        return {};
      }
      std::string pattern = std::string(directory) + "/dunitest-memory-XXXXXX";
      std::vector<char> path(pattern.begin(), pattern.end());
      path.push_back('\0');
      char* created = mkdtemp(path.data());
      if (!created) { ADD_FAILURE() << strerror(errno); return {}; }
      scratch_dir_ = created;
    }
    std::string path = scratch_dir_ + "/cache-" + std::to_string(files_.size());
    files_.push_back(path);
    return path;
  }
  int OwnFd(int fd) { if (fd >= 0) fds_.push_back(fd); return fd; }
  bool GoneOrZombie(pid_t pid) {
    std::string path = "/proc/" + std::to_string(pid) + "/stat";
    int fd = open(path.c_str(), O_RDONLY);
    if (fd < 0) {
      if (errno == ENOENT || errno == ESRCH) return true;
      ADD_FAILURE() << path << ": " << strerror(errno);
      return false;
    }
    char data[4096];
    ssize_t length = read(fd, data, sizeof(data));
    close(fd);
    if (length <= 0) { ADD_FAILURE() << "invalid " << path; return false; }
    std::string text(data, length);
    size_t end = text.rfind(')');
    if (end == std::string::npos || end + 2 >= text.size()) {
      ADD_FAILURE() << "invalid " << path; return false;
    }
    return text[end + 2] == 'Z';
  }
  bool Sleeping(pid_t pid) {
    const std::string text = Read("/proc/" + std::to_string(pid) + "/stat");
    size_t end = text.rfind(')');
    return end != std::string::npos && end + 2 < text.size() && text[end + 2] == 'S';
  }
  std::vector<pid_t> Threads(const std::string& group) {
    std::istringstream input(Read(group + "/cgroup.threads"));
    std::vector<pid_t> ids;
    long id;
    while (input >> id) {
      if (id <= 0 || id > INT32_MAX) { ADD_FAILURE() << "invalid thread id"; return {}; }
      ids.push_back(static_cast<pid_t>(id));
    }
    if (!input.eof()) { ADD_FAILURE() << "invalid cgroup.threads"; return {}; }
    std::sort(ids.begin(), ids.end());
    return ids;
  }
  bool AllSleeping(const std::vector<pid_t>& ids) {
    return !ids.empty() && std::all_of(ids.begin(), ids.end(), [&](pid_t id) { return Sleeping(id); });
  }
  bool CpuStage(MemoryWorker* worker, uint64_t phase, const std::vector<pid_t>& ids) {
    const uint64_t end = Now() + 10 * kSecond;
    for (;;) {
      if (!Now() || Now() >= end) return false;
      Reply state = worker->AskUntil(worker_op::CpuStatus, phase, end);
      if (state.error) { ADD_FAILURE() << "CPU stage " << phase << ": " << strerror(state.error); return false; }
      if (state.child == pid_t(kCpuSleepers) && AllSleeping(ids)) return true;
      usleep(1000);
    }
  }
  bool DragonCpuStorageOracle() {
    utsname identity{};
    if (uname(&identity)) { ADD_FAILURE() << "uname: " << strerror(errno); return false; }
    const bool dragon = strstr(identity.release, "-dragonos") || strstr(identity.version, "-dragonos");
    if (!dragon) return false; // Linux uses a stack k_itimer, not these heap registrations.
    const std::string report = Read("/sys/kernel/debug/page_cache/accounting_selftest");
    EXPECT_NE(std::string::npos, report.find("status=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("memcg_page_state=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("memcg_task_resources=ok\n")) << report;
    return true; // Missing/failing Dragon marker is a failure, never a Linux oracle fallback.
  }
  void Protection(bool hard, bool swap_roles = false);
  std::string parent_, group_, scratch_dir_;
  std::vector<std::string> dirs_, files_;
  std::vector<std::unique_ptr<MemoryWorker>> workers_;
  std::vector<int> fds_;
  uint64_t reserved_ = 0;
  bool enabled_ = false;
};

TEST_F(CgroupMemory, ZombiePidsChargeSurvivesRemovedAncestorsUntilReap) {
  ASSERT_TRUE(Has(Read(parent_ + "/cgroup.controllers"), "pids"));
  const bool enable_parent = !Has(Read(parent_ + "/cgroup.subtree_control"), "pids");
  struct PidsControlCleanup {
    std::string parent, group;
    bool enabled;
    ~PidsControlCleanup() {
      EXPECT_EQ(0, Write(group + "/cgroup.subtree_control", "-pids"));
      if (enabled) { EXPECT_EQ(0, Write(parent + "/cgroup.subtree_control", "-pids")); }
    }
  } cleanup{parent_, group_, enable_parent};
  if (enable_parent) { ASSERT_EQ(0, Write(parent_ + "/cgroup.subtree_control", "+pids")); }
  ASSERT_EQ(0, Write(group_ + "/cgroup.subtree_control", "+pids"));
  const uint64_t baseline = Scalar(group_ + "/pids.current");
  const std::string branch = Make(group_, "zombie-parent");
  ASSERT_FALSE(branch.empty());
  ASSERT_EQ(0, Write(branch + "/cgroup.subtree_control", "+pids"));
  const std::string leaf = Make(branch, "leaf");
  const std::string sibling = Make(branch, "sibling");
  ASSERT_FALSE(leaf.empty()); ASSERT_FALSE(sibling.empty());
  ASSERT_EQ(0, Write(leaf + "/pids.max", "1"));
  int gate[2], reply[2];
  ASSERT_EQ(0, pipe(gate));
  const int reply_result = pipe(reply);
  if (reply_result) { close(gate[0]); close(gate[1]); }
  ASSERT_EQ(0, reply_result);
  struct ChildCleanup {
    pid_t pid = -1;
    int gate = -1, reply = -1;
    ~ChildCleanup() {
      if (gate >= 0) close(gate);
      if (reply >= 0) close(reply);
      if (pid > 0) {
        // The leader has not been reaped, so its private PGID cannot be reused.
        kill(-pid, SIGKILL);
        kill(pid, SIGKILL);
        int status = 0;
        EXPECT_TRUE(Reap(pid, &status, Now() + 2 * kSecond));
      }
    }
  } child;
  child.pid = fork();
  if (child.pid < 0) {
    close(gate[0]); close(gate[1]); close(reply[0]); close(reply[1]);
  }
  ASSERT_GE(child.pid, 0);
  if (!child.pid) {
    close(gate[1]); close(reply[0]);
    if (setpgid(0, 0)) _exit(4);
    char command;
    if (read(gate[0], &command, 1) != 1) _exit(1);
    errno = 0;
    pid_t unexpected = fork();
    if (!unexpected) _exit(0);
    int result = unexpected < 0 ? errno : 0;
    if (unexpected > 0) {
      int status = 0;
      if (!Reap(unexpected, &status, Now() + 2 * kSecond)) {
        kill(unexpected, SIGKILL);
        if (!Reap(unexpected, &status, Now() + 2 * kSecond)) _exit(5);
      }
    }
    if (write(reply[1], &result, sizeof(result)) != sizeof(result)) _exit(2);
    if (read(gate[0], &command, 1) != 1) _exit(3);
    _exit(0);
  }
  close(gate[0]); close(reply[1]);
  child.gate = gate[1]; child.reply = reply[0];
  ASSERT_EQ(0, setpgid(child.pid, child.pid));
  ASSERT_EQ(0, Write(leaf + "/cgroup.procs", std::to_string(child.pid)));
  EXPECT_EQ(baseline + 1, Scalar(group_ + "/pids.current"));
  errno = 0;
  EXPECT_EQ(-1, rmdir(leaf.c_str())); EXPECT_EQ(EBUSY, errno);
  char command = 'F';
  ASSERT_TRUE(Transfer(child.gate, &command, 1, true, Now() + 2 * kSecond));
  int result = 0;
  ASSERT_TRUE(Transfer(child.reply, &result, sizeof(result), false, Now() + 2 * kSecond));
  EXPECT_EQ(EAGAIN, result);
  EXPECT_EQ(1u, Scalar(leaf + "/pids.current"));
  // Organizational migration is unlimited and preserves the common ancestor.
  ASSERT_EQ(0, Write(sibling + "/cgroup.procs", std::to_string(child.pid)));
  EXPECT_EQ(0u, Scalar(leaf + "/pids.current"));
  EXPECT_EQ(1u, Scalar(branch + "/pids.current"));
  ASSERT_EQ(0, Write(leaf + "/cgroup.procs", std::to_string(child.pid)));
  command = 'E';
  ASSERT_TRUE(Transfer(child.gate, &command, 1, true, Now() + 2 * kSecond));
  ASSERT_TRUE(Eventually([&] { return GoneOrZombie(child.pid); }));
  EXPECT_TRUE(Read(leaf + "/cgroup.procs").empty());
  EXPECT_EQ(1u, Scalar(leaf + "/pids.current"));
  ASSERT_EQ(0, rmdir(leaf.c_str())) << strerror(errno);
  ASSERT_EQ(0, rmdir(sibling.c_str())) << strerror(errno);
  ASSERT_EQ(0, rmdir(branch.c_str())) << strerror(errno);
  EXPECT_EQ(baseline + 1, Scalar(group_ + "/pids.current"));
  ASSERT_EQ(0, mkdir(branch.c_str(), 0755));
  EXPECT_EQ(0u, Scalar(branch + "/pids.current"));
  int status = 0;
  ASSERT_TRUE(Reap(child.pid, &status, Now() + 2 * kSecond));
  child.pid = -1;
  EXPECT_TRUE(WIFEXITED(status)); EXPECT_EQ(0, WEXITSTATUS(status));
  EXPECT_EQ(baseline, Scalar(group_ + "/pids.current"));
  EXPECT_EQ(0u, Scalar(branch + "/pids.current"));
  EXPECT_EQ(0, rmdir(branch.c_str()));
}

TEST_F(CgroupMemory, RootVisibilityAndDefaults) {
  for (const char* name : {"current", "peak", "min", "low", "high", "max",
                           "events", "events.local", "oom.group"}) {
    errno = 0;
    EXPECT_EQ(-1, access(("/sys/fs/cgroup/memory." + std::string(name)).c_str(), F_OK));
    EXPECT_EQ(ENOENT, errno) << name;
  }
  EXPECT_EQ(0, access("/sys/fs/cgroup/memory.stat", R_OK));
  EXPECT_EQ(0, access("/sys/fs/cgroup/memory.reclaim", W_OK));
  std::string leaf = Leaf();
  EXPECT_EQ("max\n", Read(leaf + "/memory.max"));
  EXPECT_EQ("max\n", Read(leaf + "/memory.high"));
  EXPECT_EQ("0\n", Read(leaf + "/memory.min"));
  EXPECT_EQ("0\n", Read(leaf + "/memory.low"));
  EXPECT_EQ("0\n", Read(leaf + "/memory.oom.group"));
  EXPECT_NE(0, Write(leaf + "/memory.peak", "0"));
}

TEST_F(CgroupMemory, ProgressExecutorCannotMoveIntoConstrainedDomain) {
  DIR* processes = opendir("/proc");
  ASSERT_NE(nullptr, processes);
  pid_t executor = -1;
  while (dirent* entry = readdir(processes)) {
    char* end = nullptr;
    long candidate = std::strtol(entry->d_name, &end, 10);
    if (candidate <= 0 || !end || *end) continue;
    const std::string status = Read("/proc/" + std::to_string(candidate) + "/status");
    if (status.compare(0, strlen("Name:\tmemory_notify\n"), "Name:\tmemory_notify\n") == 0) {
      executor = static_cast<pid_t>(candidate);
      break;
    }
  }
  EXPECT_EQ(0, closedir(processes));
  ASSERT_GT(executor, 0) << "memory progress executor is missing";
  const std::string membership = "/proc/" + std::to_string(executor) + "/cgroup";
  ASSERT_EQ("0::/\n", Read(membership));
  cpu_set_t before;
  CPU_ZERO(&before);
  ASSERT_EQ(0, sched_getaffinity(executor, sizeof(before), &before));
  EXPECT_EQ(1, CPU_COUNT(&before));

  // Internal domain nodes reject all tasks (EBUSY) before the per-CPU check.
  // Use an eligible leaf so the test exercises the executor's own contract.
  const std::string destination = Leaf();
  ASSERT_FALSE(destination.empty());
  const int migration = Write(destination + "/cgroup.procs", std::to_string(executor));
  if (migration == 0) {
    // Preserve subsequent tests even when the regression lets the permanent
    // executor move. Do not constrain this group before checking rejection.
    EXPECT_EQ(0, Write("/sys/fs/cgroup/cgroup.procs", std::to_string(executor)));
  }
  EXPECT_EQ(EINVAL, migration);
  EXPECT_EQ("0::/\n", Read(membership));
  errno = 0;
  EXPECT_EQ(-1, sched_setaffinity(executor, sizeof(before), &before));
  EXPECT_EQ(EINVAL, errno);
  cpu_set_t after;
  CPU_ZERO(&after);
  ASSERT_EQ(0, sched_getaffinity(executor, sizeof(after), &after));
  EXPECT_TRUE(CPU_EQUAL(&before, &after));
}

TEST_F(CgroupMemory, ReadOnlyAccountingFilesKeepControllerGeneration) {
  std::string leaf = Leaf();
  ASSERT_NE(UINT64_MAX, Scalar(leaf + "/memory.current"));
  ASSERT_GE(Scalar(leaf + "/memory.peak"), Scalar(leaf + "/memory.current"));
  for (const char* name : {"anon", "file", "kernel", "pagetables", "slab"}) {
    EXPECT_NE(UINT64_MAX, Stat(leaf, name)) << name;
    EXPECT_NE(UINT64_MAX, Field(Read("/sys/fs/cgroup/memory.stat"), name)) << name;
  }
  int old_hierarchy = OwnFd(open((leaf + "/memory.events").c_str(), O_RDONLY));
  int old_local = OwnFd(open((leaf + "/memory.events.local").c_str(), O_RDONLY));
  ASSERT_GE(old_hierarchy, 0);
  ASSERT_GE(old_local, 0);
  for (const char* name : {"low", "high", "max", "oom", "oom_kill", "oom_group_kill"}) {
    EXPECT_NE(UINT64_MAX, Field(Read(leaf + "/memory.events"), name));
    EXPECT_NE(UINT64_MAX, Field(Read(leaf + "/memory.events.local"), name));
  }
  ASSERT_EQ(0, Write(group_ + "/cgroup.subtree_control", "-memory"));
  ASSERT_EQ(0, Write(group_ + "/cgroup.subtree_control", "+memory"));
  char bytes[256];
  for (int fd : {old_hierarchy, old_local}) {
    errno = 0;
    EXPECT_EQ(-1, read(fd, bytes, sizeof(bytes)));
    EXPECT_EQ(ENODEV, errno);
  }
  EXPECT_NE(UINT64_MAX, Scalar(leaf + "/memory.current"));
  EXPECT_NE(UINT64_MAX, Field(Read(leaf + "/memory.events.local"), "oom"));
}

TEST_F(CgroupMemory, ParseLimitsAndRejectTrailingGarbage) {
  std::string leaf = Leaf();
  struct Case { const char* text; const char* expected; };
  const Case valid[] = {{"8193", "8192\n"}, {"0x2000", "8192\n"},
                        {"020000", "8192\n"}, {"8K", "8192\n"},
                        {"2m", "2097152\n"}, {" \tmax\n", "max\n"}};
  for (const char* name : {"min", "low", "high", "max"}) {
    std::string path = leaf + "/memory." + name;
    for (const auto& c : valid) {
      ASSERT_EQ(0, WriteBounded(path, c.text)) << name << ": " << c.text;
      EXPECT_EQ(c.expected, Read(path));
    }
    for (const char* bad : {"1Kjunk", "maxjunk", "08", "0x", "2 M"}) {
      EXPECT_EQ(EINVAL, WriteBounded(path, bad)) << bad;
      EXPECT_EQ("max\n", Read(path));
    }
  }
  EXPECT_EQ(0, Write(leaf + "/memory.oom.group", "0x1"));
  EXPECT_EQ("1\n", Read(leaf + "/memory.oom.group"));
  EXPECT_EQ(EINVAL, Write(leaf + "/memory.oom.group", "2"));
  EXPECT_EQ("1\n", Read(leaf + "/memory.oom.group"));
  EXPECT_EQ(EINVAL, WriteBounded(leaf + "/memory.reclaim", "1M swappiness=0"));
}

TEST_F(CgroupMemory, UnprivilegedPolicyWriteDenied) {
  std::string leaf = Leaf();
  EXPECT_EQ(EACCES, WriteBounded(leaf + "/memory.max", "16M", true));
  EXPECT_EQ("max\n", Read(leaf + "/memory.max"));
}

TEST_F(CgroupMemory, AnonymousFirstTouchUnmapAndPeak) {
  std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_NE(nullptr, worker);
  const uint64_t anon = Stat(leaf, "anon");
  ASSERT_NE(UINT64_MAX, anon);
  ASSERT_EQ(0, worker->Ask('M', 8 * MiB).error);
  EXPECT_LT(Stat(leaf, "anon"), anon + MiB);
  ASSERT_EQ(0, worker->Ask('T').error);
  ASSERT_TRUE(Eventually([&] { return Stat(leaf, "anon") >= anon + 8 * MiB; }));
  EXPECT_GE(Current(leaf), Stat(leaf, "anon"));
  uint64_t peak = Scalar(leaf + "/memory.peak");
  EXPECT_GE(peak, Current(leaf));
  ASSERT_EQ(0, worker->Ask('U').error);
  EXPECT_TRUE(Eventually([&] { return Stat(leaf, "anon") < anon + MiB; }));
  EXPECT_GE(Scalar(leaf + "/memory.peak"), peak);
}

TEST_F(CgroupMemory, ProtectedAnonymousChargePersistsUntilUnmapOrExit) {
  std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_NE(nullptr, worker);
  const uint64_t baseline = Stat(leaf, "anon");
  ASSERT_NE(UINT64_MAX, baseline);
  ASSERT_TRUE(Allocate(worker, 8 * MiB));
  ASSERT_TRUE(Eventually([&] { return Stat(leaf, "anon") >= baseline + 8 * MiB; }));
  ASSERT_EQ(0, worker->Ask(worker_op::Protect, PROT_NONE).error);
  // Permissions do not change physical ownership or release resident pages.
  EXPECT_GE(Stat(leaf, "anon"), baseline + 8 * MiB);
  ASSERT_EQ(0, worker->Ask('U').error);
  ASSERT_TRUE(Eventually([&] { return Stat(leaf, "anon") < baseline + MiB; }));

  const uint64_t before_exit = Stat(leaf, "anon");
  ASSERT_NE(UINT64_MAX, before_exit);
  ASSERT_TRUE(Allocate(worker, 8 * MiB));
  ASSERT_TRUE(Eventually([&] { return Stat(leaf, "anon") >= before_exit + 8 * MiB; }));
  ASSERT_EQ(0, worker->Ask(worker_op::Protect, PROT_NONE).error);
  EXPECT_GE(Stat(leaf, "anon"), before_exit + 8 * MiB);
  worker->Stop();
  EXPECT_TRUE(Eventually([&] { return Stat(leaf, "anon") < baseline + MiB; }));
}

TEST_F(CgroupMemory, ForkSharesPagesAndCowChargesNewOwner) {
  std::string a = Leaf("a"), b = Leaf("b");
  auto* worker = Worker(a);
  ASSERT_TRUE(Allocate(worker, 8 * MiB));
  Reply forked = worker->Ask('F');
  ASSERT_EQ(0, forked.error);
  ASSERT_GT(forked.child, 0);
  ASSERT_EQ(0, Write(b + "/cgroup.procs", std::to_string(forked.child)));
  const uint64_t before = Stat(b, "anon");
  ASSERT_NE(UINT64_MAX, before);
  EXPECT_LT(before, MiB);  // Inherited physical pages still belong to a.
  ASSERT_EQ(0, worker->Ask('C').error);
  EXPECT_TRUE(Eventually([&] { return Stat(b, "anon") >= before + 8 * MiB; }));
  EXPECT_GE(Stat(a, "anon"), 8 * MiB);
}

TEST_F(CgroupMemory, MigrationKeepsOldPageOwnerAndNewChargesFollowTask) {
  std::string a = Leaf("a"), b = Leaf("b");
  auto* worker = Worker(a);
  ASSERT_TRUE(Allocate(worker, 8 * MiB));
  ASSERT_TRUE(Eventually([&] { return Stat(a, "anon") >= 8 * MiB; }));
  ASSERT_EQ(0, Write(b + "/cgroup.procs", std::to_string(worker->pid())));
  EXPECT_GE(Stat(a, "anon"), 8 * MiB);
  EXPECT_LT(Stat(b, "anon"), MiB);
  ASSERT_EQ(0, worker->Ask('U').error);
  EXPECT_TRUE(Eventually([&] { return Stat(a, "anon") < MiB; }));
  ASSERT_TRUE(Allocate(worker, 8 * MiB));
  EXPECT_TRUE(Eventually([&] { return Stat(b, "anon") >= 8 * MiB; }));
}

TEST_F(CgroupMemory, OfflineOldOwnerRetainsAncestorChargeUntilUnmap) {
  std::string a = Leaf("a"), b = Leaf("b");
  auto* worker = Worker(a);
  ASSERT_TRUE(Allocate(worker, 8 * MiB));
  ASSERT_TRUE(Eventually([&] { return Stat(group_, "anon") >= 8 * MiB; }));
  ASSERT_EQ(0, Write(b + "/cgroup.procs", std::to_string(worker->pid())));
  ASSERT_EQ(0, rmdir(a.c_str()));
  EXPECT_GE(Stat(group_, "anon"), 8 * MiB);
  ASSERT_EQ(0, worker->Ask('U').error);
  EXPECT_TRUE(Eventually([&] { return Stat(group_, "anon") < MiB; }));
}

TEST_F(CgroupMemory, MemfdFdCloseDoesNotReleaseLiveMapping) {
  std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_TRUE(Allocate(worker, 8 * MiB, 'D'));
  ASSERT_TRUE(Eventually([&] { return Stat(leaf, "shmem") >= 8 * MiB; }));
  EXPECT_GE(Stat(leaf, "file"), Stat(leaf, "shmem"));
  ASSERT_EQ(0, worker->Ask('L').error);
  EXPECT_GE(Stat(leaf, "shmem"), 8 * MiB);
  ASSERT_EQ(0, worker->Ask('U').error);
  EXPECT_TRUE(Eventually([&] { return Stat(leaf, "shmem") == 0; }));
}

TEST_F(CgroupMemory, MemfdTruncateReleasesBackingPages) {
  std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_TRUE(Allocate(worker, 8 * MiB, 'D'));
  ASSERT_TRUE(Eventually([&] { return Stat(leaf, "shmem") >= 8 * MiB; }));
  ASSERT_EQ(0, worker->Ask('H').error);
  EXPECT_TRUE(Eventually([&] { return Stat(leaf, "shmem") == 0; }));
}

TEST_F(CgroupMemory, SharedAnonymousIsShmemNotDuplicatedAnonymousCharge) {
  std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_TRUE(Allocate(worker, 8 * MiB, 'S'));
  EXPECT_TRUE(Eventually([&] { return Stat(leaf, "shmem") >= 8 * MiB; }));
  EXPECT_GE(Stat(leaf, "file"), Stat(leaf, "shmem"));
  EXPECT_LT(Stat(leaf, "anon"), MiB);
  ASSERT_EQ(0, worker->Ask('U').error);
  EXPECT_TRUE(Eventually([&] { return Stat(leaf, "shmem") == 0; }));
}

TEST_F(CgroupMemory, PagecacheHitDoesNotChangeOwnerAndCleanReclaimFreesPages) {
  std::string a = Leaf("a"), b = Leaf("b"), file = Scratch();
  auto* producer = Worker(a, file);
  ASSERT_EQ(0, producer->Ask('P', 8 * MiB).error);
  ASSERT_TRUE(Eventually([&] { return Stat(a, "file") >= 8 * MiB; }));
  auto* reader = Worker(b, file);
  ASSERT_EQ(0, reader->Ask('R', 8 * MiB).error);
  ASSERT_EQ(0, reader->Ask('V').error);
  EXPECT_LT(Stat(b, "file"), MiB);
  EXPECT_GE(Stat(a, "file"), 8 * MiB);
  ASSERT_EQ(0, reader->Ask('U').error);
  ASSERT_EQ(0, producer->Ask('L').error);
  int result = WriteBounded(a + "/memory.reclaim", "4M");
  EXPECT_EQ(0, result);
  EXPECT_TRUE(Eventually([&] { return Stat(a, "file") <= 4 * MiB; }));
}

TEST_F(CgroupMemory, MaxOomKillsOnlyLimitedIndependentMm) {
  std::string limited = Leaf("limited"), sibling = Leaf("sibling");
  ASSERT_EQ(0, WriteBounded(limited + "/memory.max", "16M"));
  auto* witness = Worker(sibling);
  ASSERT_NE(nullptr, witness);
  ASSERT_EQ(0, witness->Ask('J').error);
  ASSERT_TRUE(Allocate(witness, 32 * MiB));
  // A global-victim scan would prefer this larger +1000 sibling. Keeping it
  // alive therefore discriminates real memcg scoping from a global OOM
  // which happens to select the limited task in an otherwise quiet guest.
  auto* victim = Worker(limited);
  ASSERT_EQ(0, victim->Ask('M', 32 * MiB).error);
  victim->Ask('T');  // A fatal OOM signal closes the reply channel.
  int status = 0;
  ASSERT_TRUE(victim->Wait(&status));
  EXPECT_TRUE(WIFSIGNALED(status));
  if (WIFSIGNALED(status)) { EXPECT_EQ(SIGKILL, WTERMSIG(status)); }
  EXPECT_TRUE(witness->Alive());
  EXPECT_EQ(0, witness->Ask('G').error);
  EXPECT_GT(Event(limited, "max"), 0u);
  EXPECT_GT(Event(limited, "oom"), 0u);
  EXPECT_GT(Event(limited, "oom_kill"), 0u);
  EXPECT_EQ(0u, Event(sibling, "oom_kill"));
}

TEST_F(CgroupMemory, AncestorLimitRecordsHierarchicalNotSiblingEvents) {
  std::string limited = Leaf("limited"), sibling = Leaf("sibling");
  ASSERT_EQ(0, WriteBounded(group_ + "/memory.max", "16M"));
  auto* victim = Worker(limited);
  ASSERT_EQ(0, victim->Ask('M', 32 * MiB).error);
  victim->Ask('T');
  int status = 0;
  ASSERT_TRUE(victim->Wait(&status));
  ASSERT_TRUE(WIFSIGNALED(status));
  EXPECT_EQ(SIGKILL, WTERMSIG(status));
  EXPECT_GT(Event(group_, "max", true), 0u);
  EXPECT_EQ(0u, Event(limited, "max", true));
  EXPECT_GT(Event(group_, "oom_kill"), 0u);
  EXPECT_GT(Event(limited, "oom_kill", true), 0u);
  EXPECT_EQ(0u, Event(group_, "oom_kill", true));
  EXPECT_EQ(0u, Event(sibling, "oom_kill"));
}

TEST_F(CgroupMemory, OomGroupKillsAllKillableMembersButProtectsMinus1000) {
  std::string leaf = Leaf();
  ASSERT_EQ(0, Write(leaf + "/memory.oom.group", "1"));
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "16M"));
  auto* immune = Worker(leaf);
  ASSERT_EQ(0, immune->Ask('I').error);
  auto* member = Worker(leaf);
  ASSERT_TRUE(Allocate(member, 2 * MiB));
  auto* victim = Worker(leaf);
  ASSERT_EQ(0, victim->Ask('M', 32 * MiB).error);
  victim->Ask('T');
  int status = 0;
  ASSERT_TRUE(victim->Wait(&status));
  ASSERT_TRUE(WIFSIGNALED(status));
  EXPECT_EQ(SIGKILL, WTERMSIG(status));
  ASSERT_TRUE(member->Wait(&status));
  ASSERT_TRUE(WIFSIGNALED(status));
  EXPECT_EQ(SIGKILL, WTERMSIG(status));
  EXPECT_EQ(0, immune->Ask('G').error);
  EXPECT_GT(Event(leaf, "oom_group_kill"), 0u);
}

TEST_F(CgroupMemory, HighIsSoftAndDoesNotInvokeOom) {
  std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.high", "4M"));
  ASSERT_EQ(0, worker->Ask('M', 8 * MiB).error);
  ASSERT_TRUE(worker->Begin('T'));
  EXPECT_TRUE(Eventually([&] { return Event(leaf, "high") > 0; }));
  // Unswappable anonymous pages may remain above high. No kill is allowed.
  EXPECT_TRUE(worker->Alive());
  EXPECT_EQ(0u, Event(leaf, "oom_kill"));
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.high", "max"));
  ASSERT_EQ(0, worker->Finish().error);
  EXPECT_EQ(0, worker->Ask('G').error);
}

TEST_F(CgroupMemory, HighPenaltyActuallyDelaysAndNonfatalSignalDoesNotShortenIt) {
  const std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_NE(nullptr, worker);
  ASSERT_EQ(0, worker->Ask(worker_op::InstallHighSignal).error);
  ASSERT_EQ(0, worker->Ask('M', 8192).error);
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.high", "0"));
  const uint64_t start = Now();
  ASSERT_NE(0u, start);
  ASSERT_TRUE(worker->Begin('T'));
  ASSERT_TRUE(Eventually([&] { return Event(leaf, "high") > 0; }));
  // Two unswappable pages exceed max(high, one page). Linux's quadratic
  // penalty reaches its two-second cap; merely recording high is not enough.
  EXPECT_FALSE(worker->ReplyReadyFor(kSecond / 10));
  ASSERT_EQ(0, kill(worker->pid(), SIGUSR1));
  EXPECT_FALSE(worker->ReplyReadyFor(kSecond / 10));
  ASSERT_EQ(0, worker->Finish().error);
  const uint64_t finish = Now();
  ASSERT_GE(finish, start);
  EXPECT_GE(finish - start, kSecond);
  EXPECT_EQ(1, worker->Ask(worker_op::ReadHighSignal).child);
  EXPECT_EQ(0u, Event(leaf, "oom_kill"));
  EXPECT_TRUE(worker->Alive());
}

TEST_F(CgroupMemory, FatalSignalTerminatesRealHighPenaltyWithoutOomKill) {
  const std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_NE(nullptr, worker);
  ASSERT_EQ(0, worker->Ask('M', 8192).error);
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.high", "0"));
  ASSERT_TRUE(worker->Begin('T'));
  ASSERT_TRUE(Eventually([&] { return Event(leaf, "high") > 0; }));
  EXPECT_FALSE(worker->ReplyReadyFor(kSecond / 10));
  ASSERT_EQ(0, kill(worker->pid(), SIGKILL));
  int status = 0;
  ASSERT_TRUE(worker->Wait(&status));
  ASSERT_TRUE(WIFSIGNALED(status));
  EXPECT_EQ(SIGKILL, WTERMSIG(status));
  EXPECT_EQ(0u, Event(leaf, "oom_kill"));
}

TEST_F(CgroupMemory, ReclaimInsufficientAnonymousReturnsEagain) {
  std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_TRUE(Allocate(worker, 8 * MiB));
  // DragonOS has no swap; no assertion depends on a global cache flush.
  EXPECT_EQ(EAGAIN, WriteBounded(leaf + "/memory.reclaim", "32M"));
  EXPECT_TRUE(worker->Alive());
  EXPECT_GE(Stat(leaf, "anon"), 8 * MiB);
}

// MM149: mutate the live source between reads, without allocator pressure or
// timing races. Each open description must still expose one coherent record.
TEST_F(CgroupMemory, TextSnapshotPartialReadKeepsScalarRecord) {
  const std::string leaf = Leaf();
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "4096"));
  int fd = OwnFd(open((leaf + "/memory.max").c_str(), O_RDONLY | O_CLOEXEC));
  ASSERT_GE(fd, 0);
  char data[64];
  ASSERT_EQ(1, read(fd, data, 1));
  ASSERT_EQ('4', data[0]);
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "1048576"));
  ssize_t n = read(fd, data + 1, sizeof(data) - 1);
  ASSERT_GE(n, 0);
  EXPECT_EQ("4096\n", std::string(data, size_t(n) + 1));
  EXPECT_EQ(0, read(fd, data, sizeof(data)));
}

TEST_F(CgroupMemory, TextSnapshotEofDoesNotAppendNewTail) {
  const std::string leaf = Leaf();
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "4096"));
  int fd = OwnFd(open((leaf + "/memory.max").c_str(), O_RDONLY | O_CLOEXEC));
  ASSERT_GE(fd, 0);
  char data[64];
  ASSERT_EQ(5, read(fd, data, sizeof(data)));
  ASSERT_EQ("4096\n", std::string(data, 5));
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "1048576"));
  EXPECT_EQ(0, read(fd, data, sizeof(data)));
  ASSERT_EQ(0, lseek(fd, 0, SEEK_SET));
  ASSERT_EQ(8, read(fd, data, sizeof(data)));
  EXPECT_EQ("1048576\n", std::string(data, 8));
}

TEST_F(CgroupMemory, TextSnapshotDupSharesRecordAndPosition) {
  const std::string leaf = Leaf();
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "4096"));
  int first = OwnFd(open((leaf + "/memory.max").c_str(), O_RDONLY | O_CLOEXEC));
  ASSERT_GE(first, 0);
  int duplicate = OwnFd(dup(first));
  ASSERT_GE(duplicate, 0);
  char data[64];
  ASSERT_EQ(1, read(first, data, 1));
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "1048576"));
  EXPECT_EQ("1048576\n", Read(leaf + "/memory.max"));
  ASSERT_EQ(4, read(duplicate, data, sizeof(data)));
  EXPECT_EQ("096\n", std::string(data, 4));
  EXPECT_EQ(5, lseek(first, 0, SEEK_CUR));
  ASSERT_EQ(0, lseek(duplicate, 0, SEEK_SET));
  ASSERT_EQ(8, read(first, data, sizeof(data)));
  EXPECT_EQ("1048576\n", std::string(data, 8));
  EXPECT_EQ(8, lseek(duplicate, 0, SEEK_CUR));
}

TEST_F(CgroupMemory, TextSnapshotPreadAndZeroLengthRespectSharedCursor) {
  const std::string leaf = Leaf();
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "4096"));
  int fd = OwnFd(open((leaf + "/memory.max").c_str(), O_RDONLY | O_CLOEXEC));
  ASSERT_GE(fd, 0);
  char data[64];
  ASSERT_EQ(1, read(fd, data, 1));
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "1048576"));
  EXPECT_EQ(0, read(fd, nullptr, 0));
  EXPECT_EQ(0, pread(fd, nullptr, 0, 0));
  EXPECT_EQ(1, lseek(fd, 0, SEEK_CUR));
  ASSERT_EQ(4, read(fd, data, sizeof(data)));
  EXPECT_EQ("096\n", std::string(data, 4));
  ASSERT_EQ(8, pread(fd, data, sizeof(data), 0));
  EXPECT_EQ("1048576\n", std::string(data, 8));
  EXPECT_EQ(5, lseek(fd, 0, SEEK_CUR));
  // pread changed seq state, but not f_pos: normal read regenerates at byte 5.
  ASSERT_EQ(3, read(fd, data, sizeof(data)));
  EXPECT_EQ("76\n", std::string(data, 3));
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "4096"));
  ASSERT_EQ(4, pread(fd, data, sizeof(data), 1));
  EXPECT_EQ("096\n", std::string(data, 4));
  EXPECT_EQ(8, lseek(fd, 0, SEEK_CUR));
}

TEST_F(CgroupMemory, TextSnapshotSameDescriptionWriteDoesNotReplaceReadBuffer) {
  const std::string leaf = Leaf();
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "4096"));
  int fd = OwnFd(open((leaf + "/memory.max").c_str(), O_RDWR | O_CLOEXEC));
  ASSERT_GE(fd, 0);
  char data[64];
  ASSERT_EQ(1, read(fd, data, 1));
  const std::string longer = "1048576";
  ASSERT_EQ(ssize_t(longer.size()), pwrite(fd, longer.data(), longer.size(), 0));
  EXPECT_EQ(1, lseek(fd, 0, SEEK_CUR));
  ASSERT_EQ(4, read(fd, data, sizeof(data)));
  EXPECT_EQ("096\n", std::string(data, 4));
  ASSERT_EQ(0, lseek(fd, 0, SEEK_SET));
  ASSERT_EQ(8, read(fd, data, sizeof(data)));
  EXPECT_EQ("1048576\n", std::string(data, 8));
}

TEST_F(CgroupMemory, TextSnapshotMemoryStatFreezesBeforeRealPageCharge) {
  const std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_NE(nullptr, worker);
  ASSERT_EQ(0, worker->Ask('M', 4 * MiB).error);
  int fd = OwnFd(open((leaf + "/memory.stat").c_str(), O_RDONLY | O_CLOEXEC));
  ASSERT_GE(fd, 0);
  char data[4096];
  ASSERT_EQ(1, read(fd, data, 1));
  std::string frozen(data, 1);
  ASSERT_EQ(0, worker->Ask('T').error);
  for (;;) {
    ssize_t n = read(fd, data, sizeof(data));
    if (n < 0 && errno == EINTR) continue;
    ASSERT_GE(n, 0);
    if (!n) break;
    frozen.append(data, size_t(n));
    ASSERT_LT(frozen.size(), 65536u);
  }
  uint64_t before = Field(frozen, "anon");
  ASSERT_NE(UINT64_MAX, before) << frozen;
  EXPECT_GE(Stat(leaf, "anon"), before + 4 * MiB);
}

TEST_F(CgroupMemory, TextSnapshotRemovedDescriptionCannotReadRecreatedEpoch) {
  const std::string leaf = Leaf("snapshot-epoch");
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "4096"));
  int old = OwnFd(open((leaf + "/memory.max").c_str(), O_RDONLY | O_CLOEXEC));
  ASSERT_GE(old, 0);
  char data[64];
  ASSERT_EQ(1, read(old, data, 1));
  ASSERT_EQ('4', data[0]);

  ASSERT_EQ(0, rmdir(leaf.c_str()));
  ASSERT_EQ(leaf, Make(group_, "snapshot-epoch"));
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "1048576"));
  int current = OwnFd(open((leaf + "/memory.max").c_str(), O_RDONLY | O_CLOEXEC));
  ASSERT_GE(current, 0);
  // The old record's cached bytes do not require a new kernfs source entry.
  ASSERT_EQ(4, read(old, data, sizeof(data)));
  EXPECT_EQ("096\n", std::string(data, 4));
  // EOF and rewind must validate the original node, not follow its pathname.
  errno = 0;
  EXPECT_EQ(-1, read(old, data, sizeof(data)));
  EXPECT_EQ(ENODEV, errno);
  ASSERT_EQ(0, lseek(old, 0, SEEK_SET));
  errno = 0;
  EXPECT_EQ(-1, read(old, data, sizeof(data)));
  EXPECT_EQ(ENODEV, errno);
  ASSERT_EQ(8, read(current, data, sizeof(data)));
  EXPECT_EQ("1048576\n", std::string(data, 8));
}

TEST_F(CgroupMemory, TextSnapshotEventTailAndZeroReadDoNotAcknowledgeUpdate) {
  const std::string leaf = Leaf();
  int fd = OwnFd(open((leaf + "/cgroup.events").c_str(), O_RDONLY | O_CLOEXEC));
  ASSERT_GE(fd, 0);
  char data[128];
  ASSERT_EQ(1, read(fd, data, 1));
  std::string frozen(data, 1);
  auto* worker = Worker(leaf);
  ASSERT_NE(nullptr, worker);
  ASSERT_TRUE(worker->Alive());
  pollfd changed{fd, POLLPRI, 0};
  ASSERT_EQ(1, poll(&changed, 1, 0));
  EXPECT_EQ(0, read(fd, nullptr, 0));
  EXPECT_EQ(0, pread(fd, nullptr, 0, 0));
  EXPECT_EQ(1, poll(&changed, 1, 0));
  ssize_t n = read(fd, data, sizeof(data));
  ASSERT_GE(n, 0);
  frozen.append(data, size_t(n));
  EXPECT_EQ(0u, Field(frozen, "populated"));
  EXPECT_EQ(0, read(fd, data, sizeof(data)));
  EXPECT_EQ(1, poll(&changed, 1, 0));
  ASSERT_EQ(0, lseek(fd, 0, SEEK_SET));
  ASSERT_GT(read(fd, data, sizeof(data)), 0);
  EXPECT_EQ(0, poll(&changed, 1, 0));
}

TEST_F(CgroupMemory, EventsPollUsesPerOpenAcknowledgement) {
  std::string leaf = Leaf();
  int first = OwnFd(open((leaf + "/memory.events").c_str(), O_RDONLY | O_CLOEXEC));
  int second = OwnFd(open((leaf + "/memory.events").c_str(), O_RDONLY | O_CLOEXEC));
  ASSERT_GE(first, 0);
  ASSERT_GE(second, 0);
  int duplicate = OwnFd(dup(first));
  ASSERT_GE(duplicate, 0);
  char data[1024];
  EXPECT_GT(read(first, data, sizeof(data)), 0);
  EXPECT_GT(read(second, data, sizeof(data)), 0);
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "16M"));
  auto* worker = Worker(leaf);
  ASSERT_EQ(0, worker->Ask('M', 32 * MiB).error);
  worker->Ask('T');
  int status = 0;
  EXPECT_TRUE(worker->Wait(&status));
  pollfd fds[] = {{first, POLLPRI, 0}, {second, POLLPRI, 0}};
  EXPECT_EQ(2, poll(fds, 2, 0));
  EXPECT_NE(0, fds[0].revents & (POLLPRI | POLLERR));
  EXPECT_NE(0, fds[1].revents & (POLLPRI | POLLERR));
  EXPECT_EQ(0, lseek(first, 0, SEEK_SET));
  EXPECT_GT(read(first, data, sizeof(data)), 0);
  pollfd shared{duplicate, POLLPRI, 0};
  EXPECT_EQ(0, poll(&shared, 1, 0));
  pollfd independent{second, POLLPRI, 0};
  EXPECT_EQ(1, poll(&independent, 1, 0));
}

// Scale the Linux selftest's protection model to <128 MiB. A is the reclaim
// root, B its protected child, and G an independently owned pressure sibling.
// We never invoke B.memory.reclaim to claim that B protects itself.
void CgroupMemory::Protection(bool hard, bool swap_roles) {
  static_assert(3 * 16 * MiB + 52 * MiB <= kCaseBytes);
  const char* attribute = hard ? "memory.min" : "memory.low";
  std::string b = Leaf("protected"), g = Leaf("pressure");
  ASSERT_EQ(0, Write(b + "/cgroup.subtree_control", "+memory"));
  std::string c = Make(b, "c"), d = Make(b, "d"), e = Make(b, "e"), empty = Make(b, "empty");
  const std::string high = swap_roles ? d : c, low = swap_roles ? c : d;
  ASSERT_EQ(0, Write(b + "/" + attribute, "16M"));
  ASSERT_EQ(0, Write(high + "/" + attribute, "24M"));
  ASSERT_EQ(0, Write(low + "/" + attribute, "8M"));
  ASSERT_EQ(0, Write(empty + "/" + attribute, "500M"));
  for (const auto& target : {c, d, e}) {
    auto* cache = Worker(target, Scratch());
    ASSERT_NE(nullptr, cache);
    ASSERT_EQ(0, cache->Ask('I').error);
    ASSERT_EQ(0, cache->Ask('P', 16 * MiB).error);
    ASSERT_EQ(0, cache->Ask('N', 16 * MiB).error);
    ASSERT_TRUE(Eventually([&] { return Stat(target, "file") >= 16 * MiB; }));
    EXPECT_EQ(0u, Stat(target, "shmem"));
  }
  ASSERT_EQ(0u, Current(empty));
  ASSERT_EQ(0, WriteBounded(group_ + "/memory.max", "64M"));
  auto* pressure = Worker(g);
  ASSERT_NE(nullptr, pressure);
  ASSERT_EQ(0, pressure->Ask('J').error);
  ASSERT_TRUE(Allocate(pressure, 44 * MiB));
  ASSERT_TRUE(Eventually([&] { return Stat(g, "anon") >= 44 * MiB; }));
  // Linux protection scales against total usage, including real accounted
  // kernel objects, not file alone. Verify conservation without assuming a
  // sub-1MiB metadata footprint that actual cache entries do not satisfy.
  ASSERT_TRUE(Eventually([&] {
    uint64_t total = Current(b), file = Stat(b, "file"), kernel = Stat(b, "kernel");
    return total >= file && kernel > 0 && total - file >= kernel;
  })) << "current=" << Current(b) << " file=" << Stat(b, "file")
      << "\nB memory.stat:\n" << Read(b + "/memory.stat")
      << "\nC memory.stat:\n" << Read(c + "/memory.stat")
      << "\nD memory.stat:\n" << Read(d + "/memory.stat")
      << "\nE memory.stat:\n" << Read(e + "/memory.stat");
  EXPECT_GE(Current(b), 16 * MiB);
  EXPECT_GT(Stat(high, "file"), Stat(low, "file"))
      << "high:\n" << Read(high + "/memory.stat") << "low:\n" << Read(low + "/memory.stat");
  EXPECT_GT(Current(high), Current(low))
      << "high total=" << Current(high) << " low total=" << Current(low)
      << " high:\n" << Read(high + "/memory.stat") << "low:\n" << Read(low + "/memory.stat");
  EXPECT_LT(Stat(e, "file"), Stat(low, "file"));
  EXPECT_EQ(0u, Current(empty));
  ASSERT_EQ(0, pressure->Ask('U').error);
  ASSERT_EQ(0, pressure->Ask('M', 52 * MiB).error);
  Reply result = pressure->Ask('T');
  if (hard) {
    int status = 0;
    ASSERT_TRUE(pressure->Wait(&status));
    ASSERT_TRUE(WIFSIGNALED(status));
    EXPECT_EQ(SIGKILL, WTERMSIG(status));
    EXPECT_GT(Event(group_, "oom_kill"), 0u);
    EXPECT_GE(Current(b), 16 * MiB);
  } else {
    ASSERT_EQ(0, result.error);
    ASSERT_TRUE(Eventually([&] { return Stat(g, "anon") >= 52 * MiB; }));
    EXPECT_LT(Current(b), 16 * MiB);
    EXPECT_GT(Event(b, "low"), 0u);
    EXPECT_GT(Event(c, "low") + Event(d, "low"), 0u);
    EXPECT_EQ(0u, Event(group_, "oom_kill"));
  }
}

TEST_F(CgroupMemory, MinAncestorBudgetAndSiblingOvercommit) { Protection(true); }
TEST_F(CgroupMemory, LowAncestorBudgetCanBeBreachedWithEvents) { Protection(false); }
TEST_F(CgroupMemory, MinProtectionFollowsPolicyAfterSwappingSiblingRoles) { Protection(true, true); }
TEST_F(CgroupMemory, LowProtectionFollowsPolicyAfterSwappingSiblingRoles) { Protection(false, true); }

TEST_F(CgroupMemory, ProactiveReclaimIgnoresTargetOwnMinAndLow) {
  std::string leaf = Leaf();
  ASSERT_EQ(0, Write(leaf + "/memory.min", "16M"));
  ASSERT_EQ(0, Write(leaf + "/memory.low", "16M"));
  auto* cache = Worker(leaf, Scratch());
  ASSERT_NE(nullptr, cache);
  ASSERT_EQ(0, cache->Ask('P', 8 * MiB).error);
  ASSERT_EQ(0, cache->Ask('N', 8 * MiB).error);
  ASSERT_TRUE(Eventually([&] { return Stat(leaf, "file") >= 8 * MiB; }));
  // leaf is now the reclaim root, so neither its own min nor low may block
  // this explicit reclaim. Separate from the pressure model: over-reclaim
  // here must not invalidate that model's later effective protection budget.
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.reclaim", "4M"));
  EXPECT_TRUE(Eventually([&] { return Stat(leaf, "file") <= 4 * MiB; }));
}

TEST_F(CgroupMemory, MemoryEventsInotifyWithoutOpenFileReportsRealOomKill) {
  const std::string leaf = Leaf("inotify_oom");
  auto* victim = Worker(leaf);
  ASSERT_NE(nullptr, victim);
  ASSERT_TRUE(Allocate(victim, 8 * MiB));
  ASSERT_GE(Stat(leaf, "anon"), 8 * MiB);
  const uint64_t before = Event(leaf, "oom_kill");  // Read closes its OFD.
  ASSERT_EQ(0u, before);
  InotifySource source;
  ASSERT_TRUE(source.valid());
  const int watch = source.Watch(leaf + "/memory.events");
  ASSERT_GE(watch, 0);
  ASSERT_TRUE(source.Modified({watch}, kSecond / 4, false));
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "4M"));
  int status = 0;
  ASSERT_TRUE(victim->Wait(&status));
  ASSERT_TRUE(WIFSIGNALED(status));
  ASSERT_EQ(SIGKILL, WTERMSIG(status));
  // The leaf remains online and is not removed until TearDown. No event-file
  // OFD could register its poll state while the real OOM transition occurred.
  EXPECT_TRUE(source.Modified({watch}, 8 * kSecond));
  EXPECT_GT(Event(leaf, "oom_kill"), before);
}

TEST_F(CgroupMemory, PopulationInotifyWithoutOpenFileReportsOnlyZeroCrossings) {
  const std::string leaf = Leaf("watched"), sibling = Leaf("sibling");
  ASSERT_EQ(0u, Field(Read(leaf + "/cgroup.events"), "populated"));
  ASSERT_EQ(0u, Field(Read(group_ + "/cgroup.events"), "populated"));
  InotifySource source;
  ASSERT_TRUE(source.valid());
  const int local = source.Watch(leaf + "/cgroup.events");
  const int parent = source.Watch(group_ + "/cgroup.events");
  ASSERT_GE(local, 0);
  ASSERT_GE(parent, 0);
  ASSERT_TRUE(source.Modified({local, parent}, kSecond / 4, false));
  auto* first = Worker(leaf);
  ASSERT_NE(nullptr, first);
  ASSERT_TRUE(source.Modified({local, parent}, 8 * kSecond));
  EXPECT_EQ(1u, Field(Read(leaf + "/cgroup.events"), "populated"));
  EXPECT_EQ(1u, Field(Read(group_ + "/cgroup.events"), "populated"));
  auto* second = Worker(leaf);
  ASSERT_NE(nullptr, second);
  EXPECT_TRUE(source.Modified({local, parent}, kSecond / 4, false));
  ASSERT_EQ(0, Write(sibling + "/cgroup.procs", std::to_string(first->pid())));
  EXPECT_TRUE(source.Modified({local, parent}, kSecond / 4, false));
  ASSERT_EQ(0, Write(sibling + "/cgroup.procs", std::to_string(second->pid())));
  ASSERT_TRUE(source.Modified({local}, 8 * kSecond));  // Common parent stays 1.
  EXPECT_TRUE(source.Modified({local, parent}, kSecond / 4, false));
  EXPECT_EQ(0u, Field(Read(leaf + "/cgroup.events"), "populated"));
  EXPECT_EQ(1u, Field(Read(group_ + "/cgroup.events"), "populated"));
  first->Stop();
  EXPECT_TRUE(source.Modified({local, parent}, kSecond / 4, false));
  second->Stop();
  EXPECT_TRUE(source.Modified({parent}, 8 * kSecond));
  EXPECT_EQ(0u, Field(Read(group_ + "/cgroup.events"), "populated"));
}

TEST_F(CgroupMemory, DirectoryInotifyReportsKernelEventFileNamesWithoutOpenFile) {
  const std::string leaf = Leaf("directory_watch");
  InotifySource source;
  ASSERT_TRUE(source.valid());
  const int directory = source.Watch(leaf);
  ASSERT_GE(directory, 0);
  auto* victim = Worker(leaf);
  ASSERT_NE(nullptr, victim);
  ASSERT_TRUE(source.Modified({directory}, 8 * kSecond, true, "cgroup.events"));
  ASSERT_TRUE(Allocate(victim, 8 * MiB));
  ASSERT_GE(Stat(leaf, "anon"), 8 * MiB);
  const uint64_t before = Event(leaf, "oom_kill");
  ASSERT_EQ(0u, before);
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "4M"));
  int status = 0;
  ASSERT_TRUE(victim->Wait(&status));
  ASSERT_TRUE(WIFSIGNALED(status));
  ASSERT_EQ(SIGKILL, WTERMSIG(status));
  // A MODIFY named memory.max from the policy writer is not evidence.
  EXPECT_TRUE(source.Modified({directory}, 8 * kSecond, true, "memory.events"));
  EXPECT_GT(Event(leaf, "oom_kill"), before);
}

TEST_F(CgroupMemory, RuntimeMaxLoweringKillsExistingUsageThenNewWorkerCanRun) {
  std::string leaf = Leaf("limited"), sibling = Leaf("witness");
  auto* witness = Worker(sibling);
  ASSERT_NE(nullptr, witness);
  ASSERT_EQ(0, witness->Ask('J').error);
  ASSERT_TRUE(Allocate(witness, 32 * MiB));
  auto* victim = Worker(leaf);
  ASSERT_TRUE(Allocate(victim, 24 * MiB));
  ASSERT_TRUE(Eventually([&] { return Stat(leaf, "anon") >= 24 * MiB; }));
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "16M"));
  // Lowering may return before exit/uncharge finishes. Require the eventual
  // local OOM result, not an instantaneous current<=max observation.
  int status = 0;
  ASSERT_TRUE(victim->Wait(&status));
  ASSERT_TRUE(WIFSIGNALED(status));
  EXPECT_EQ(SIGKILL, WTERMSIG(status));
  EXPECT_GT(Event(leaf, "oom_kill"), 0u);
  EXPECT_EQ(0, witness->Ask('G').error);
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", "32M"));
  auto* replacement = Worker(leaf);
  ASSERT_TRUE(Allocate(replacement, 24 * MiB));
  EXPECT_EQ(0, replacement->Ask('G').error);
}

TEST_F(CgroupMemory, RuntimeHighLoweringDoesNotKillAndFutureChargesThrottle) {
  std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_NE(nullptr, worker);
  ASSERT_EQ(0, worker->Ask('M', 24 * MiB).error);
  ASSERT_EQ(0, worker->Ask('T', 16 * MiB).error);
  ASSERT_TRUE(Eventually([&] { return Stat(leaf, "anon") >= 16 * MiB; }));
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.high", "8M"));
  EXPECT_GE(Stat(leaf, "anon"), 16 * MiB);
  EXPECT_EQ(0u, Event(leaf, "oom_kill"));
  uint64_t high = Event(leaf, "high");
  ASSERT_TRUE(worker->Begin('T'));
  EXPECT_TRUE(Eventually([&] { return Event(leaf, "high") > high; }));
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.high", "max"));
  ASSERT_EQ(0, worker->Finish().error);
  EXPECT_EQ(0u, Event(leaf, "oom_kill"));
  EXPECT_EQ(0, worker->Ask('G').error);
}

TEST_F(CgroupMemory, DisableReenableCreatesNewGenerationWithoutLosingOldPages) {
  std::string a = Leaf("a"), b = Leaf("b");
  auto* worker = Worker(a);
  ASSERT_TRUE(Allocate(worker, 8 * MiB));
  ASSERT_TRUE(Eventually([&] { return Stat(a, "anon") >= 8 * MiB; }));
  ASSERT_EQ(0, WriteBounded(a + "/memory.max", "16M"));
  int old = OwnFd(open((a + "/memory.current").c_str(), O_RDONLY));
  ASSERT_GE(old, 0);
  ASSERT_EQ(0, Write(b + "/cgroup.procs", std::to_string(worker->pid())));
  ASSERT_EQ(0, Write(group_ + "/cgroup.subtree_control", "-memory"));
  EXPECT_EQ(-1, access((a + "/memory.current").c_str(), F_OK));
  EXPECT_GE(Stat(group_, "anon"), 8 * MiB);
  char bytes[64];
  errno = 0;
  EXPECT_EQ(-1, read(old, bytes, sizeof(bytes)));
  EXPECT_EQ(ENODEV, errno);
  ASSERT_EQ(0, Write(group_ + "/cgroup.subtree_control", "+memory"));
  EXPECT_EQ("max\n", Read(a + "/memory.max"));
  EXPECT_LT(Stat(a, "anon"), MiB);
  EXPECT_GE(Stat(group_, "anon"), 8 * MiB);
  ASSERT_EQ(0, worker->Ask('U').error);
  EXPECT_TRUE(Eventually([&] { return Stat(group_, "anon") < MiB; }));
  ASSERT_TRUE(Allocate(worker, 8 * MiB));
  EXPECT_TRUE(Eventually([&] { return Stat(b, "anon") >= 8 * MiB; }));
}

TEST_F(CgroupMemory, ThreadedTransitionKeepsHistoricalCacheInResourceDomain) {
  std::string leaf = Leaf();
  std::string escape = Make(parent_, group_.substr(group_.find_last_of('/') + 1) + "-escape");
  ASSERT_FALSE(escape.empty());
  auto* worker = Worker(leaf, Scratch());
  ASSERT_NE(nullptr, worker);
  ASSERT_EQ(0, worker->Ask('P', 8 * MiB).error);
  ASSERT_EQ(0, worker->Ask('N', 8 * MiB).error);
  ASSERT_TRUE(Eventually([&] { return Stat(group_, "file") >= 8 * MiB; }));
  int old = OwnFd(open((leaf + "/memory.current").c_str(), O_RDONLY));
  ASSERT_GE(old, 0);
  ASSERT_EQ(0, Write(escape + "/cgroup.procs", std::to_string(worker->pid())));
  ASSERT_EQ(0, Write(group_ + "/cgroup.subtree_control", "-memory"));
  ASSERT_EQ(0, Write(leaf + "/cgroup.type", "threaded"));
  EXPECT_EQ("domain threaded\n", Read(group_ + "/cgroup.type"));
  EXPECT_EQ("threaded\n", Read(leaf + "/cgroup.type"));
  char bytes[64];
  errno = 0;
  EXPECT_EQ(-1, read(old, bytes, sizeof(bytes)));
  EXPECT_EQ(ENODEV, errno);
  errno = 0;
  EXPECT_EQ(-1, access((leaf + "/memory.current").c_str(), F_OK));
  EXPECT_EQ(ENOENT, errno);
  EXPECT_EQ(EOPNOTSUPP, Write(group_ + "/cgroup.subtree_control", "+memory"));
  EXPECT_GE(Stat(group_, "file"), 8 * MiB);
  ASSERT_EQ(0, Write(leaf + "/cgroup.procs", std::to_string(worker->pid())));
  ASSERT_TRUE(Allocate(worker, 8 * MiB));
  EXPECT_TRUE(Eventually([&] { return Stat(group_, "anon") >= 8 * MiB; }));
  EXPECT_GE(Stat(group_, "file"), 8 * MiB);
}

TEST_F(CgroupMemory, SparseUserPagetablesAreChargedAndFreedAfterMmExit) {
  std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_NE(nullptr, worker);
  uint64_t before = Stat(leaf, "pagetables");
  ASSERT_EQ(0, worker->Ask('Q').error);
  ASSERT_TRUE(Eventually([&] { return Stat(leaf, "pagetables") > before; }));
  EXPECT_GE(Stat(leaf, "kernel"), Stat(leaf, "pagetables"));
  ASSERT_TRUE(worker->Begin('X'));
  int status = 0;
  ASSERT_TRUE(worker->Wait(&status));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
  // Empty intermediate tables may be retained until mm teardown; munmap
  // alone is not a portable proof that their physical owners were freed.
  EXPECT_TRUE(Eventually([&] { return Stat(leaf, "pagetables") == 0; }));
}

TEST_F(CgroupMemory, UserThreadKernelStacksChargeAndEventuallyRelease) {
  std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_NE(nullptr, worker);
  uint64_t before = Stat(leaf, "kernel_stack");
  ASSERT_EQ(0, worker->Ask('B').error);
  ASSERT_TRUE(Eventually([&] { return Stat(leaf, "kernel_stack") > before; }));
  EXPECT_GE(Stat(leaf, "kernel"), Stat(leaf, "kernel_stack"));
  ASSERT_EQ(0, worker->Ask('E').error);
  // join is not a release notification. Exit, reap, then wait for delayed
  // stack/frame destruction rather than requiring an instantaneous delta.
  ASSERT_TRUE(worker->Begin('X'));
  int status = 0;
  ASSERT_TRUE(worker->Wait(&status));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
  EXPECT_TRUE(Eventually([&] { return Stat(leaf, "kernel_stack") == 0; }));
}

TEST_F(CgroupMemory, CompletedCpuClockWaitDoesNotAllocateAfterActualMaxLowering) {
  const bool dragon = DragonCpuStorageOracle();
  (void)dragon;
  std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_NE(nullptr, worker);
  ASSERT_GT(worker->pid(), 0);
  ASSERT_EQ(0, worker->AskUntil(worker_op::CpuFastClock, 0, Now() + 10 * kSecond).error);
  ASSERT_TRUE(Eventually([&] { return Sleeping(worker->pid()); }));
  const uint64_t limit = Current(leaf);
  ASSERT_GT(limit, 0u);
  ASSERT_EQ(0, WriteBounded(leaf + "/memory.max", std::to_string(limit)));
  ASSERT_TRUE(Eventually([&] { return Sleeping(worker->pid()); }));
  const uint64_t current = Current(leaf), kernel = Stat(leaf, "kernel"), slab = Stat(leaf, "slab");
  const uint64_t max_events = Event(leaf, "max"), killed = Event(leaf, "oom_kill");
  ASSERT_EQ(0, worker->AskUntil(worker_op::CpuFastClock, 0, Now() + 10 * kSecond).error);
  ASSERT_TRUE(Eventually([&] { return Sleeping(worker->pid()); }));
  EXPECT_EQ(current, Current(leaf));
  EXPECT_EQ(kernel, Stat(leaf, "kernel"));
  EXPECT_EQ(slab, Stat(leaf, "slab"));
  EXPECT_EQ(max_events, Event(leaf, "max"));
  EXPECT_EQ(killed, Event(leaf, "oom_kill"));
}

TEST_F(CgroupMemory, CpuClockWaitRegistrationChargesCallerAndRefundsBeforeThreadExit) {
  const bool dragon = DragonCpuStorageOracle();
  std::string leaf = Leaf();
  const std::string source_birth_membership = Read("/proc/self/cgroup");
  ASSERT_FALSE(source_birth_membership.empty());
  // MemoryWorker forked its ProcessSignalState in root. All these already-born
  // threads share that source, but their new wait registrations use leaf.
  auto* worker = Worker(leaf, "", true);
  ASSERT_NE(nullptr, worker);
  ASSERT_GT(worker->pid(), 0);
  Reply setup = worker->AskUntil(worker_op::CpuSetup, 0, Now() + 10 * kSecond);
  ASSERT_EQ(0, setup.error);
  ASSERT_EQ(pid_t(kCpuSleepers), setup.child);
  const auto ids = Threads(leaf);
  ASSERT_EQ(kCpuSleepers + 1, ids.size());
  ASSERT_TRUE(Eventually([&] { return AllSleeping(ids); }));
  const uint64_t before_kernel = Stat(leaf, "kernel"), before_slab = Stat(leaf, "slab");
  const uint64_t before_stack = Stat(leaf, "kernel_stack"), before_tables = Stat(leaf, "pagetables");
  const uint64_t before_anon = Stat(leaf, "anon"), before_file = Stat(leaf, "file");
  ASSERT_EQ(0, worker->AskUntil(worker_op::CpuStart, 0, Now() + 10 * kSecond).error);
  ASSERT_TRUE(CpuStage(worker, 1, ids));
  const uint64_t waiting_kernel = Stat(leaf, "kernel"), waiting_slab = Stat(leaf, "slab");
  RecordProperty("cpu_wait_source_birth_membership", source_birth_membership);
  RecordProperty("cpu_wait_waiter_membership", Read("/proc/" + std::to_string(worker->pid()) + "/cgroup"));
  RecordProperty("cpu_wait_before_kernel", std::to_string(before_kernel));
  RecordProperty("cpu_wait_blocked_kernel", std::to_string(waiting_kernel));
  RecordProperty("cpu_wait_before_slab", std::to_string(before_slab));
  RecordProperty("cpu_wait_blocked_slab", std::to_string(waiting_slab));
  EXPECT_EQ(before_stack, Stat(leaf, "kernel_stack"));
  EXPECT_EQ(before_tables, Stat(leaf, "pagetables"));
  EXPECT_EQ(before_anon, Stat(leaf, "anon"));
  EXPECT_EQ(before_file, Stat(leaf, "file"));
  if (dragon) {
    EXPECT_GT(waiting_kernel, before_kernel);
    EXPECT_GT(waiting_slab, before_slab);
    if (waiting_kernel >= before_kernel && waiting_slab >= before_slab) {
      EXPECT_EQ(waiting_kernel - before_kernel, waiting_slab - before_slab);
    }
  }
  EXPECT_GE(Current(leaf), Stat(leaf, "anon") + Stat(leaf, "file") + Stat(leaf, "kernel"));
  ASSERT_EQ(0, worker->AskUntil(worker_op::CpuInterrupt, 0, Now() + 10 * kSecond).error);
  ASSERT_TRUE(CpuStage(worker, 2, ids));
  EXPECT_EQ(ids, Threads(leaf)); // No join/stack removal can stand in for refund.
  EXPECT_TRUE(Eventually([&] {
    return Stat(leaf, "kernel") == before_kernel && Stat(leaf, "slab") == before_slab;
  }));
  EXPECT_EQ(before_stack, Stat(leaf, "kernel_stack"));
  EXPECT_EQ(before_tables, Stat(leaf, "pagetables"));
  ASSERT_EQ(0, worker->AskUntil(worker_op::CpuDestroy, 0, Now() + 10 * kSecond).error);
  ASSERT_TRUE(worker->Begin('X'));
  int status = 0;
  ASSERT_TRUE(worker->Wait(&status));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
  EXPECT_TRUE(Eventually([&] { return Stat(leaf, "kernel_stack") == 0; }));
}

TEST_F(CgroupMemory, OrdinarySharedMmScoreAdjustmentIsSynchronized) {
  auto* worker = Worker(Leaf("ordinary"));
  ASSERT_NE(nullptr, worker);
  ASSERT_EQ(0, worker->Ask('J').error);
  Reply clone = worker->Ask(worker_op::CloneSharedMm);
  ASSERT_EQ(0, clone.error);
  ASSERT_GT(clone.child, 0);
  ASSERT_EQ(0, worker->Ask('Z').error);
  ASSERT_EQ(0, Write("/proc/" + std::to_string(clone.child) + "/oom_score_adj", "-1000"));
  EXPECT_EQ("-1000\n", Read("/proc/" + std::to_string(clone.child) + "/oom_score_adj"));
  EXPECT_EQ("-1000\n", Read("/proc/" + std::to_string(worker->pid()) + "/oom_score_adj"));
}

TEST_F(CgroupMemory, OrdinarySharedMmOomCleanupKillsOtherGroup) {
  std::string limited = Leaf("limited"), sharer_group = Leaf("shared"), independent = Leaf("witness");
  auto* witness = Worker(independent);
  ASSERT_NE(nullptr, witness);
  ASSERT_EQ(0, witness->Ask('J').error);
  ASSERT_TRUE(Allocate(witness, 32 * MiB));
  auto* victim = Worker(limited);
  ASSERT_NE(nullptr, victim);
  ASSERT_EQ(0, victim->Ask('J').error);
  Reply clone = victim->Ask(worker_op::CloneSharedMm);
  ASSERT_EQ(0, clone.error);
  ASSERT_GT(clone.child, 0);
  ASSERT_EQ(0, Write(sharer_group + "/cgroup.procs", std::to_string(clone.child)));
  ASSERT_EQ(0, victim->Ask('Z').error);
  ASSERT_EQ("1000\n", Read("/proc/" + std::to_string(clone.child) + "/oom_score_adj"));
  ASSERT_EQ("1000\n", Read("/proc/" + std::to_string(victim->pid()) + "/oom_score_adj"));
  ASSERT_EQ(0, WriteBounded(limited + "/memory.max", "16M"));
  ASSERT_EQ(0, victim->Ask('M', 32 * MiB).error);
  victim->Ask('T');
  // Observe this BEFORE Wait/Stop's defensive process-group cleanup, which
  // could otherwise disguise a missing kernel shared-mm kill.
  EXPECT_TRUE(Eventually([&] { return GoneOrZombie(clone.child); }));
  EXPECT_GT(Event(limited, "oom_kill"), 0u);
  EXPECT_EQ(0, witness->Ask('G').error);
  int status = 0;
  ASSERT_TRUE(victim->Wait(&status));
  ASSERT_TRUE(WIFSIGNALED(status));
  EXPECT_EQ(SIGKILL, WTERMSIG(status));
}

TEST_F(CgroupMemory, VforkSharedMmCleanupKillsMinus1000Sharer) {
  const std::string limited = Leaf("limited"), shared = Leaf("shared");
  auto* witness = Worker(Leaf("witness"));
  ASSERT_NE(nullptr, witness);
  ASSERT_EQ(0, witness->Ask('J').error);
  auto* victim = Worker(limited);
  ASSERT_NE(nullptr, victim);
  ASSERT_EQ(0, victim->Ask('J').error);
  ASSERT_TRUE(Allocate(victim, 32 * MiB));
  Reply clone = victim->Ask(worker_op::CloneVforkMm);
  ASSERT_EQ(0, clone.error);
  ASSERT_GT(clone.child, 0);
  ASSERT_EQ(0, Write(shared + "/cgroup.procs", std::to_string(clone.child)));
  // Linux proc/base.c skips active vfork children when synchronizing adj;
  // in_vfork() excludes that child from selection, NOT the suspended parent.
  ASSERT_EQ(0, Write("/proc/" + std::to_string(clone.child) + "/oom_score_adj", "-1000"));
  ASSERT_EQ("-1000\n", Read("/proc/" + std::to_string(clone.child) + "/oom_score_adj"));
  ASSERT_EQ("1000\n", Read("/proc/" + std::to_string(victim->pid()) + "/oom_score_adj"));
  ASSERT_EQ(0, WriteBounded(limited + "/memory.max", "16M"));
  // __oom_kill_process kills other groups sharing the selected victim mm
  // even when their score is -1000. Observe before fixture group cleanup.
  EXPECT_TRUE(Eventually([&] { return GoneOrZombie(clone.child); }));
  EXPECT_GT(Event(limited, "oom_kill"), 0u);
  EXPECT_EQ(0, witness->Ask('G').error);
  int status = 0;
  ASSERT_TRUE(victim->Wait(&status));
  ASSERT_TRUE(WIFSIGNALED(status));
  EXPECT_EQ(SIGKILL, WTERMSIG(status));
}

TEST_F(CgroupMemory, TcpBufferStockKeepsOriginalOwnerAcrossMigrationAndOffline) {
  const std::string original = Leaf("original"), moved = Leaf("moved");
  auto* worker = Worker(original);
  ASSERT_NE(nullptr, worker);
  const uint64_t baseline = Stat(original, "sock");
  ASSERT_EQ(0, worker->Ask('K').error);
  ASSERT_TRUE(Eventually([&] { return Stat(original, "sock") > baseline; }));
  EXPECT_TRUE(Eventually([&] { return Current(original) >= Stat(original, "sock"); }));
  ASSERT_EQ(0, Write(moved + "/cgroup.procs", std::to_string(worker->pid())));
  // ACKs can release transmit buffers while receive data remains queued.
  // Ownership is stable; byte occupancy need not be an exact snapshot.
  EXPECT_GT(Stat(original, "sock"), baseline);
  EXPECT_EQ(0u, Stat(moved, "sock"));
  ASSERT_EQ(0, rmdir(original.c_str()));
  // Socket stocks retain their offline CSS, unlike reparented objcg objects.
  EXPECT_GT(Stat(group_, "sock"), baseline);
  EXPECT_EQ(0u, Stat(moved, "sock"));
  ASSERT_EQ(0, worker->Ask('O').error);
  EXPECT_TRUE(Eventually([&] { return Stat(group_, "sock") == 0; }));
  EXPECT_EQ(0, worker->Ask('G').error);
}

TEST_F(CgroupMemory, MemfdMappingPinsOfflineOwnerAfterLastFdCloses) {
  const std::string original = Leaf("original"), moved = Leaf("moved");
  auto* worker = Worker(original);
  ASSERT_TRUE(Allocate(worker, 8 * MiB, 'D'));
  ASSERT_TRUE(Eventually([&] { return Stat(original, "shmem") >= 8 * MiB; }));
  ASSERT_EQ(0, Write(moved + "/cgroup.procs", std::to_string(worker->pid())));
  ASSERT_EQ(0, worker->Ask('L').error);
  ASSERT_EQ(0, rmdir(original.c_str()));
  EXPECT_GE(Stat(group_, "shmem"), 8 * MiB);
  EXPECT_LT(Stat(moved, "shmem"), MiB);
  ASSERT_EQ(0, worker->Ask('V').error);
  EXPECT_GE(Stat(group_, "shmem"), 8 * MiB);
  ASSERT_EQ(0, worker->Ask('U').error);
  EXPECT_TRUE(Eventually([&] { return Stat(group_, "shmem") == 0; }));
}

TEST_F(CgroupMemory, BufferedIoReclaimReloadChargesOnlyMissingPagesToReader) {
  const std::string producer = Leaf("producer"), reader = Leaf("reader");
  auto* worker = Worker(producer, Scratch());
  ASSERT_NE(nullptr, worker);
  ASSERT_EQ(0, worker->Ask('P', 8 * MiB).error);
  ASSERT_EQ(0, worker->Ask('N', 8 * MiB).error);
  ASSERT_TRUE(Eventually([&] { return Stat(producer, "file") >= 8 * MiB; }));
  // P completed real fsync; background writeback need not expose a positive
  // dirty interval to a sampler. It must eventually expose the clean result.
  ASSERT_TRUE(Eventually([&] {
    return Stat(producer, "file_dirty") == 0 && Stat(producer, "file_writeback") == 0;
  }));
  ASSERT_EQ(0, WriteBounded(producer + "/memory.reclaim", "4M"));
  ASSERT_TRUE(Eventually([&] { return Stat(producer, "file") <= 4 * MiB; }));
  const uint64_t remaining = Stat(producer, "file");
  ASSERT_EQ(0, Write(reader + "/cgroup.procs", std::to_string(worker->pid())));
  // Real pread verifies each evicted page's contents and charges only holes;
  // cache hits retain the producer's original account.
  ASSERT_EQ(0, worker->Ask('N', 8 * MiB).error);
  ASSERT_TRUE(Eventually([&] { return Stat(reader, "file") >= 4 * MiB; }));
  EXPECT_EQ(remaining, Stat(producer, "file"));
  EXPECT_GE(Stat(group_, "file"), 8 * MiB);
  EXPECT_EQ(0u, Stat(reader, "shmem"));
}

TEST_F(CgroupMemory, EmptyPositionalEventsReadDoesNotAcknowledgeNotification) {
  const std::string leaf = Leaf();
  for (const char* name : {"memory.events", "memory.events.local"}) {
    int fd = OwnFd(open((leaf + "/" + name).c_str(), O_RDONLY | O_CLOEXEC));
    ASSERT_GE(fd, 0);
    const auto pending = [&] {
      pollfd watched{fd, POLLPRI, 0};
      return poll(&watched, 1, 0) == 1 && (watched.revents & POLLPRI);
    };
    ASSERT_TRUE(pending());
    EXPECT_EQ(0, pread(fd, nullptr, 0, 0));
    EXPECT_TRUE(pending());
    iovec empty{nullptr, 0};
    EXPECT_EQ(0, preadv(fd, &empty, 1, 0));
    EXPECT_TRUE(pending());
    char contents[1024];
    ASSERT_GT(pread(fd, contents, sizeof(contents), 0), 0);
    EXPECT_FALSE(pending());
  }
}

TEST_F(CgroupMemory, LocalEventsPollCannotBeReusedByNewControllerGeneration) {
  const std::string leaf = Leaf();
  int old = OwnFd(open((leaf + "/memory.events.local").c_str(), O_RDONLY | O_CLOEXEC));
  ASSERT_GE(old, 0);
  int duplicate = OwnFd(dup(old));
  ASSERT_GE(duplicate, 0);
  char contents[1024];
  ASSERT_GT(read(old, contents, sizeof(contents)), 0);
  ASSERT_EQ(0, Write(group_ + "/cgroup.subtree_control", "-memory"));
  ASSERT_EQ(0, Write(group_ + "/cgroup.subtree_control", "+memory"));
  for (int fd : {old, duplicate}) {
    errno = 0;
    EXPECT_EQ(-1, pread(fd, contents, sizeof(contents), 0));
    EXPECT_EQ(ENODEV, errno);
    pollfd revoked{fd, POLLPRI, 0};
    EXPECT_EQ(1, poll(&revoked, 1, 0));
    EXPECT_NE(0, revoked.revents & (POLLERR | POLLHUP));
  }
  int fresh = OwnFd(open((leaf + "/memory.events.local").c_str(), O_RDONLY | O_CLOEXEC));
  ASSERT_GE(fresh, 0);
  ASSERT_GT(read(fresh, contents, sizeof(contents)), 0);
  pollfd unaffected{fresh, POLLPRI, 0};
  EXPECT_EQ(0, poll(&unaffected, 1, 0));
  for (const char* name : {"low", "high", "max", "oom", "oom_kill", "oom_group_kill"})
    EXPECT_EQ(0u, Event(leaf, name, true));
}

TEST_F(CgroupMemory, EpollRegistrationBodiesReleaseOnDeleteAndSourceClose) {
  const std::string leaf = Leaf();
  auto* worker = Worker(leaf);
  ASSERT_NE(nullptr, worker);
  // The protocol uses temporary poll registrations itself. Sample only after
  // the worker has entered its next stable, blocked request wait; this avoids
  // confusing that transient registration with the 64 persistent targets.
  auto idle = [&] {
    return Eventually([&] {
      std::string stat = Read("/proc/" + std::to_string(worker->pid()) + "/stat");
      size_t end = stat.rfind(')');
      return end != std::string::npos && end + 2 < stat.size() && stat[end + 2] == 'S';
    });
  };
  ASSERT_EQ(0, worker->Ask(worker_op::EpollSetup).error);
  ASSERT_TRUE(idle());
  const uint64_t baseline = Stat(leaf, "slab_unreclaimable");
  ASSERT_EQ(0, worker->Ask(worker_op::EpollAdd).error);
  ASSERT_TRUE(idle());
  const uint64_t live = Stat(leaf, "slab_unreclaimable");
  EXPECT_GT(live, baseline);
  ASSERT_EQ(0, worker->Ask(worker_op::EpollDuplicate).error);
  ASSERT_TRUE(idle());
  EXPECT_EQ(live, Stat(leaf, "slab_unreclaimable"));
  ASSERT_EQ(0, worker->Ask(worker_op::EpollModify).error);
  ASSERT_TRUE(idle());
  EXPECT_EQ(live, Stat(leaf, "slab_unreclaimable"));
  ASSERT_EQ(0, worker->Ask(worker_op::EpollDelete).error);
  ASSERT_TRUE(idle());
  EXPECT_EQ(baseline, Stat(leaf, "slab_unreclaimable"));
  ASSERT_EQ(0, worker->Ask(worker_op::EpollAdd).error);
  ASSERT_EQ(0, worker->Ask(worker_op::EpollClose).error);
  ASSERT_TRUE(idle());
  EXPECT_LE(Stat(leaf, "slab_unreclaimable"), baseline);
}

TEST_F(CgroupMemory, SyscallObjectsReparentOfflineAndReleaseAfterWorkerExit) {
  const std::string original = Leaf("original"), moved = Leaf("moved");
  auto* worker = Worker(original);
  ASSERT_NE(nullptr, worker);
  const uint64_t baseline = Stat(original, "slab");
  ASSERT_EQ(0, worker->Ask('j').error);
  ASSERT_TRUE(Eventually([&] { return Stat(original, "slab") > baseline; }));
  const uint64_t live = Stat(original, "slab");
  EXPECT_GE(Stat(original, "kernel"), live);
  ASSERT_EQ(0, Write(moved + "/cgroup.procs", std::to_string(worker->pid())));
  EXPECT_GE(Stat(original, "slab"), live);
  ASSERT_EQ(0, rmdir(original.c_str()));
  // objcg kernel objects reparent to the nearest live ancestor, while their
  // storage remains resident. Do not claim the moved leaf now owns old fds.
  EXPECT_GE(Stat(group_, "slab"), live);
  ASSERT_EQ(0, worker->Ask('k').error);
  ASSERT_TRUE(worker->Begin('X'));
  int status = 0;
  ASSERT_TRUE(worker->Wait(&status));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
  // fd tables/PCB metadata may survive close; only exit+reap establishes
  // their eventual physical retirement. No exact object size is assumed.
  EXPECT_TRUE(Eventually([&] { return Stat(group_, "slab") < live; }));
}

}  // namespace

int main(int argc, char** argv) {
  // Closed worker pipes are evidence, not a reason to terminate the runner.
  signal(SIGPIPE, SIG_IGN);
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
