#include <gtest/gtest.h>

#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <ucontext.h>
#include <unistd.h>

#include <cerrno>
#include <cstring>

namespace {

// Every case runs in a disposable process. The parent timeout also bounds a
// broken signal-return path that prevents the child's alarm from being handled.
int RunChild(void (*body)(int), int argument = 0) {
  const pid_t child = fork();
  if (child < 0) return -errno;
  if (child == 0) {
    struct rlimit limit = {0, 0};
    setrlimit(RLIMIT_CORE, &limit);
    struct sigaction timeout_action {};
    timeout_action.sa_handler = SIG_DFL;
    sigemptyset(&timeout_action.sa_mask);
    sigaction(SIGALRM, &timeout_action, nullptr);
    sigset_t empty;
    sigemptyset(&empty);
    sigprocmask(SIG_SETMASK, &empty, nullptr);
    alarm(5);
    body(argument);
    _exit(0);
  }
  struct timespec start {};
  if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
    kill(child, SIGKILL);
    while (waitpid(child, nullptr, 0) < 0 && errno == EINTR) {}
    return -EIO;
  }
  int status = 0;
  for (;;) {
    const pid_t result = waitpid(child, &status, WNOHANG);
    if (result == child) return status;
    if (result < 0 && errno != EINTR) return -errno;
    struct timespec now {};
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 ||
        now.tv_sec - start.tv_sec >= 10) {
      kill(child, SIGKILL);
      while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
      return -ETIMEDOUT;
    }
    const struct timespec pause = {0, 10000000};
    nanosleep(&pause, nullptr);
  }
}

void ExpectSuccess(int status) {
  ASSERT_GE(status, 0) << strerror(-status);
  ASSERT_TRUE(WIFEXITED(status)) << "wait status=" << status;
  EXPECT_EQ(0, WEXITSTATUS(status)) << "child failure checkpoint";
}

volatile sig_atomic_t handled;
void ReturningHandler(int signal, siginfo_t* info, void* context) {
  if (signal != SIGUSR1 || info == nullptr || info->si_signo != SIGUSR1 ||
      context == nullptr)
    _exit(10);
  handled = 1;
}

void LibcDelivery(int) {
  handled = 0;
  struct sigaction action {};
  action.sa_sigaction = ReturningHandler;
  action.sa_flags = SA_SIGINFO;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, nullptr) != 0) _exit(11);
  if (raise(SIGUSR1) != 0 || handled != 1) _exit(12);
  // A second delivery checks that rt_sigreturn restored the blocked mask.
  handled = 0;
  if (raise(SIGUSR1) != 0 || handled != 1) _exit(13);
}

void ResetHandler(int) {
  struct sigaction current {};
  if (sigaction(SIGUSR1, nullptr, &current) != 0 ||
      current.sa_handler != SIG_DFL)
    _exit(20);
  handled = 1;
}

void ResetHand(int second_delivery) {
  handled = 0;
  struct sigaction action {};
  action.sa_handler = ResetHandler;
  action.sa_flags = SA_RESETHAND;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, nullptr) != 0) _exit(21);
  if (raise(SIGUSR1) != 0 || handled != 1) _exit(22);
  if (second_delivery) {
    raise(SIGUSR1);
    _exit(23);
  }
}

uintptr_t alt_begin;
size_t alt_size;
volatile sig_atomic_t inner_handled;
void AltstackHandler(int signal, siginfo_t*, void* context) {
  char local;
  const uintptr_t address = reinterpret_cast<uintptr_t>(&local);
  if (address < alt_begin || address - alt_begin >= alt_size) _exit(30);
  auto* uc = static_cast<ucontext_t*>(context);
  // Linux 6.6 __save_altstack stores sas_ss_flags, not sas_ss_flags(sp).
  // The dynamically computed SS_ONSTACK belongs only to sigaltstack's query.
  if (reinterpret_cast<uintptr_t>(uc->uc_stack.ss_sp) != alt_begin ||
      uc->uc_stack.ss_size != alt_size || uc->uc_stack.ss_flags != 0)
    _exit(31);
  stack_t queried {};
  if (sigaltstack(nullptr, &queried) != 0 || queried.ss_flags != SS_ONSTACK ||
      reinterpret_cast<uintptr_t>(queried.ss_sp) != alt_begin ||
      queried.ss_size != alt_size)
    _exit(32);
  if (signal == SIGUSR2) {
    inner_handled = 1;
    return;
  }
  if (signal != SIGUSR1 || raise(SIGUSR2) != 0 || inner_handled != 1)
    _exit(33);
}

void NestedAltstack(int) {
  alt_size = 128 * 1024;
  void* memory = mmap(nullptr, alt_size, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (memory == MAP_FAILED) _exit(34);
  alt_begin = reinterpret_cast<uintptr_t>(memory);
  stack_t alternate {};
  alternate.ss_sp = memory;
  alternate.ss_size = alt_size;
  if (sigaltstack(&alternate, nullptr) != 0) _exit(35);
  struct sigaction action {};
  action.sa_sigaction = AltstackHandler;
  action.sa_flags = SA_SIGINFO | SA_ONSTACK;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGUSR1, &action, nullptr) != 0 ||
      sigaction(SIGUSR2, &action, nullptr) != 0)
    _exit(36);
  inner_handled = 0;
  if (raise(SIGUSR1) != 0 || inner_handled != 1) _exit(37);
  stack_t queried {};
  if (sigaltstack(nullptr, &queried) != 0 || queried.ss_flags != 0) _exit(38);
}

int expected_signal;
int expected_code;
uintptr_t expected_address;
void FaultHandler(int signal, siginfo_t* info, void*) {
  if (signal != expected_signal || info == nullptr ||
      info->si_signo != expected_signal)
    _exit(40);
  if (info->si_code != expected_code) _exit(41);
  if (reinterpret_cast<uintptr_t>(info->si_addr) != expected_address) _exit(42);
  _exit(0);
}

enum FaultKind {
  kTruncatedRead,
  kTruncatedWrite,
  kUnpopulatedTruncatedRead,
  kUnpopulatedTruncatedWrite,
  kProtection,
  kReadOnlyWrite,
  kUnmapped
};
void Fault(int kind) {
  const long page = sysconf(_SC_PAGESIZE);
  if (page <= 0) _exit(43);
  void* memory;
  if (kind == kTruncatedRead || kind == kTruncatedWrite ||
      kind == kUnpopulatedTruncatedRead || kind == kUnpopulatedTruncatedWrite) {
    char path[] = "/tmp/dunitest-signal-XXXXXX";
    const int fd = mkstemp(path);
    if (fd < 0) _exit(44);
    unlink(path);
    if (ftruncate(fd, 2 * page) != 0) _exit(45);
    memory = mmap(nullptr, 2 * page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (memory == MAP_FAILED) _exit(46);
    // Exercise both first fault beyond the new EOF and invalidation of a PTE
    // installed before truncation. Neither access may extend the backing file.
    if (kind == kTruncatedRead || kind == kTruncatedWrite)
      static_cast<volatile unsigned char*>(memory)[page] = 0x5a;
    if (ftruncate(fd, page) != 0) _exit(47);
    close(fd);
    expected_signal = SIGBUS;
    expected_code = BUS_ADRERR;
    // Use an interior byte to detect accidental page-rounding of si_addr.
    expected_address = reinterpret_cast<uintptr_t>(memory) + page + 37;
  } else {
    memory = mmap(nullptr, page, kind == kReadOnlyWrite ? PROT_READ : PROT_NONE,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (memory == MAP_FAILED) _exit(48);
    expected_signal = SIGSEGV;
    expected_code = kind == kUnmapped ? SEGV_MAPERR : SEGV_ACCERR;
    expected_address = reinterpret_cast<uintptr_t>(memory) + 37;
    if (kind == kUnmapped && munmap(memory, page) != 0) _exit(49);
  }
  struct sigaction action {};
  action.sa_sigaction = FaultHandler;
  action.sa_flags = SA_SIGINFO;
  sigemptyset(&action.sa_mask);
  if (sigaction(expected_signal, &action, nullptr) != 0) _exit(50);
  auto* address = reinterpret_cast<volatile unsigned char*>(expected_address);
  if (kind == kTruncatedWrite || kind == kUnpopulatedTruncatedWrite ||
      kind == kReadOnlyWrite)
    *address = 0x33;
  else {
    const unsigned char value = *address;
    (void)value;
  }
  _exit(51);  // Access must fault; a successful load/store is a failure.
}

void ProtectionRoundTrip(int) {
  const long page = sysconf(_SC_PAGESIZE);
  if (page <= 0) _exit(70);
  void* memory = mmap(nullptr, page, PROT_NONE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (memory == MAP_FAILED) _exit(71);
  if (mprotect(memory, page, PROT_READ | PROT_WRITE) != 0) _exit(72);
  auto* bytes = static_cast<volatile unsigned char*>(memory);
  if (bytes[37] != 0) _exit(73);
  bytes[37] = 0xa5;
  bytes[page - 1] = 0x5a;
  if (mprotect(memory, page, PROT_NONE) != 0) _exit(74);
  if (mprotect(memory, page, PROT_READ | PROT_WRITE) != 0) _exit(75);
  if (bytes[37] != 0xa5 || bytes[page - 1] != 0x5a) _exit(76);
  bytes[37] = 0x33;
  if (bytes[37] != 0x33) _exit(77);
}

void InheritedProtection(int restore) {
  if (restore) {
    const long page = sysconf(_SC_PAGESIZE);
    if (page <= 0 ||
        mprotect(reinterpret_cast<void*>(expected_address - 37), page,
                 PROT_READ | PROT_WRITE) != 0)
      _exit(80);
    auto* byte = reinterpret_cast<volatile unsigned char*>(expected_address);
    if (*byte != 0xa5) _exit(81);
    if (restore == 1) {
      *byte = 0x33;
      if (*byte != 0x33) _exit(90);
    } else {
      // The first write comes from copy_to_user, before any user store has
      // broken COW in this child. It must also preserve the parent's page.
      int descriptors[2];
      if (pipe(descriptors) != 0) _exit(91);
      const unsigned char replacement = 0x66;
      if (write(descriptors[1], &replacement, 1) != 1 ||
          read(descriptors[0], reinterpret_cast<void*>(expected_address), 1) != 1)
        _exit(92);
      close(descriptors[0]);
      close(descriptors[1]);
      if (*byte != replacement) _exit(93);
    }
    return;
  }
  struct sigaction action {};
  action.sa_sigaction = FaultHandler;
  action.sa_flags = SA_SIGINFO;
  sigemptyset(&action.sa_mask);
  if (sigaction(SIGSEGV, &action, nullptr) != 0) _exit(82);
  const unsigned char value =
      *reinterpret_cast<volatile unsigned char*>(expected_address);
  (void)value;
  _exit(83);
}

void ForkProtection(int) {
  const long page = sysconf(_SC_PAGESIZE);
  if (page <= 0) _exit(84);
  void* memory = mmap(nullptr, page, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (memory == MAP_FAILED) _exit(85);
  auto* bytes = static_cast<volatile unsigned char*>(memory);
  bytes[37] = 0xa5;
  if (mprotect(memory, page, PROT_NONE) != 0) _exit(86);
  expected_signal = SIGSEGV;
  expected_code = SEGV_ACCERR;
  expected_address = reinterpret_cast<uintptr_t>(memory) + 37;
  // Check inherited protection, then independent user and kernel COW writes.
  for (int restore : {0, 1, 2}) {
    const int status = RunChild(InheritedProtection, restore);
    if (status < 0 || !WIFEXITED(status)) _exit(87);
    if (WEXITSTATUS(status) != 0) _exit(WEXITSTATUS(status));
    if (mprotect(memory, page, PROT_READ | PROT_WRITE) != 0) _exit(88);
    if (bytes[37] != 0xa5) _exit(89);
    if (mprotect(memory, page, PROT_NONE) != 0) _exit(86);
  }
}

#if defined(__riscv) && __riscv_xlen == 64
// asm-generic/signal.h: RV64 does not define SA_RESTORER. In particular,
// inserting a restorer field here would shift the mask from offset 16 to 24.
struct RawSigaction {
  void (*handler)(int, siginfo_t*, void*);
  unsigned long flags;
  uint64_t mask;
};
static_assert(offsetof(RawSigaction, mask) == 16);
static_assert(sizeof(RawSigaction) == 24);
void RawAction(int) {
  handled = 0;
  const RawSigaction action = {
      ReturningHandler, SA_SIGINFO | SA_RESTART, uint64_t{1} << (SIGUSR2 - 1)};
  if (syscall(SYS_rt_sigaction, SIGUSR1, &action, nullptr, sizeof(action.mask)) != 0)
    _exit(60);
  struct {
    RawSigaction action;
    uint64_t guard;
  } queried = {{nullptr, ~0UL, 0}, 0xfedcba9876543210ULL};
  if (syscall(SYS_rt_sigaction, SIGUSR1, nullptr, &queried.action,
              sizeof(action.mask)) != 0)
    _exit(61);
  if ((queried.action.flags >> 32) != 0) _exit(64);
  if (queried.action.handler != action.handler ||
      queried.action.flags != action.flags || queried.action.mask != action.mask ||
      queried.guard != 0xfedcba9876543210ULL)
    _exit(62);
  if (raise(SIGUSR1) != 0 || handled != 1) _exit(63);
}
#endif

TEST(RiscvSignalSemantics, LibcHandlerReturnsAndRestoresMask) {
  ExpectSuccess(RunChild(LibcDelivery));
}
TEST(RiscvSignalSemantics, ResetHandIsDefaultInsideHandler) {
  ExpectSuccess(RunChild(ResetHand));
}
TEST(RiscvSignalSemantics, ResetHandSecondDeliveryUsesDefaultDisposition) {
  const int status = RunChild(ResetHand, 1);
  ASSERT_GE(status, 0);
  ASSERT_TRUE(WIFSIGNALED(status)) << "wait status=" << status;
  EXPECT_EQ(SIGUSR1, WTERMSIG(status));
}
TEST(RiscvSignalSemantics, NestedAltstackSavesConfiguredFlags) {
  ExpectSuccess(RunChild(NestedAltstack));
}
TEST(RiscvSignalSemantics, TruncatedMappedReadReportsBusAddress) {
  ExpectSuccess(RunChild(Fault, kTruncatedRead));
}
TEST(RiscvSignalSemantics, TruncatedMappedWriteReportsBusAddress) {
  ExpectSuccess(RunChild(Fault, kTruncatedWrite));
}
TEST(RiscvSignalSemantics, UnpopulatedTruncatedReadReportsBusAddress) {
  ExpectSuccess(RunChild(Fault, kUnpopulatedTruncatedRead));
}
TEST(RiscvSignalSemantics, UnpopulatedTruncatedWriteReportsBusAddress) {
  ExpectSuccess(RunChild(Fault, kUnpopulatedTruncatedWrite));
}
TEST(RiscvSignalSemantics, ProtectionFaultReportsAccerrAndAddress) {
  ExpectSuccess(RunChild(Fault, kProtection));
}
TEST(RiscvSignalSemantics, ReadOnlyWriteReportsAccerrAndAddress) {
  ExpectSuccess(RunChild(Fault, kReadOnlyWrite));
}
TEST(RiscvSignalSemantics, ProtectionRoundTripPreservesResidentData) {
  ExpectSuccess(RunChild(ProtectionRoundTrip));
}
TEST(RiscvSignalSemantics, ForkInheritsProtectionAndResidentData) {
  ExpectSuccess(RunChild(ForkProtection));
}
TEST(RiscvSignalSemantics, UnmappedFaultReportsMaperrAndAddress) {
  ExpectSuccess(RunChild(Fault, kUnmapped));
}
TEST(RiscvSignalSemantics, RawRtSigactionUsesRiscvLayout) {
#if defined(__riscv) && __riscv_xlen == 64
  ExpectSuccess(RunChild(RawAction));
#else
  GTEST_SKIP() << "RISC-V 64-bit raw rt_sigaction ABI";
#endif
}

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
