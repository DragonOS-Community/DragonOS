#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/wait.h>
#include <unistd.h>

#include <chrono>
#include <cstdio>
#include <string>
#include <vector>

namespace {

volatile sig_atomic_t sigpipe_count = 0;

void CountSigpipe(int) {
  ++sigpipe_count;
}

int PollReadable(int fd, int timeout_ms) {
  pollfd event = {fd, POLLIN, 0};
  const auto deadline = std::chrono::steady_clock::now() +
                        std::chrono::milliseconds(timeout_ms);
  for (;;) {
    const int result = poll(&event, 1, timeout_ms);
    if (result >= 0 || errno != EINTR) return result;
    const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
        deadline - std::chrono::steady_clock::now());
    if (remaining.count() <= 0) return 0;
    timeout_ms = static_cast<int>(remaining.count());
  }
}

struct TransferResult {
  ssize_t count;
  int error;
};

class PipeTransferState : public testing::TestWithParam<bool> {
 protected:
  void SetUp() override {
    ASSERT_EQ(0, pipe2(source_, O_CLOEXEC));
    ASSERT_EQ(0, pipe2(destination_, O_CLOEXEC));
    ASSERT_EQ(0, pipe2(ready_, O_CLOEXEC));
    ASSERT_EQ(0, pipe2(result_, O_CLOEXEC));
    const long page = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page, 0);
    ASSERT_EQ(page, fcntl(destination_[1], F_SETPIPE_SZ, page));
    filler_.assign(page, 'f');
    struct sigaction action = {};
    action.sa_handler = CountSigpipe;
    sigemptyset(&action.sa_mask);
    ASSERT_EQ(0, sigaction(SIGPIPE, &action, &old_sigpipe_));
    signal_installed_ = true;
    sigpipe_count = 0;
  }

  void TearDown() override {
    if (child_ > 0) {
      const pid_t child = child_;
      kill(child_, SIGKILL);
      int status = 0;
      EXPECT_TRUE(ReapChild(&status))
          << "child " << child << " did not exit after SIGKILL";
    }
    for (int* pair : {source_, destination_, ready_, result_}) {
      for (int index = 0; index < 2; ++index) {
        if (pair[index] >= 0) close(pair[index]);
      }
    }
    if (epoll_ >= 0) close(epoll_);
    if (signal_installed_) sigaction(SIGPIPE, &old_sigpipe_, nullptr);
  }

  ssize_t Transfer(unsigned int flags) {
    return GetParam()
        ? tee(source_[0], destination_[1], 1, flags)
        : splice(source_[0], nullptr, destination_[1], nullptr, 1, flags);
  }

  void CloseWriter() {
    ASSERT_EQ(0, close(source_[1]));
    source_[1] = -1;
  }

  void FillDestination() {
    ASSERT_EQ(static_cast<ssize_t>(filler_.size()),
              write(destination_[1], filler_.data(), filler_.size()));
  }

  void DrainDestination() {
    std::vector<char> contents(filler_.size());
    ASSERT_EQ(static_cast<ssize_t>(contents.size()),
              read(destination_[0], contents.data(), contents.size()));
    ASSERT_EQ(filler_, contents);
  }

  bool WaitForTransferSleep() {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/stat", child_);
    const auto deadline = std::chrono::steady_clock::now() +
                          std::chrono::seconds(2);
    while (std::chrono::steady_clock::now() < deadline) {
      FILE* stat = fopen(path, "r");
      if (stat != nullptr) {
        std::string contents;
        char buffer[512];
        size_t count;
        while ((count = fread(buffer, 1, sizeof(buffer), stat)) != 0) {
          contents.append(buffer, count);
        }
        fclose(stat);
        const size_t end = contents.rfind(')');
        if (end != std::string::npos && end + 2 < contents.size() &&
            contents[end + 1] == ' ' &&
            (contents[end + 2] == 'S' || contents[end + 2] == 'D')) {
          return true;
        }
      }
      if (PollReadable(result_[0], 0) != 0) return false;
      usleep(1000);
    }
    return false;
  }

  bool ReapChild(int* status) {
    const auto deadline = std::chrono::steady_clock::now() +
                          std::chrono::seconds(2);
    while (std::chrono::steady_clock::now() < deadline) {
      const pid_t waited = waitpid(child_, status, WNOHANG);
      if (waited == child_) {
        child_ = -1;
        return true;
      }
      if (waited < 0 && errno != EINTR) return false;
      usleep(1000);
    }
    return false;
  }

  void StartBlockedTransfer() {
    child_ = fork();
    ASSERT_GE(child_, 0) << strerror(errno);
    if (child_ == 0) {
      close(source_[1]);
      close(destination_[0]);
      close(ready_[0]);
      close(result_[0]);
      alarm(5);
      // This one-byte acknowledgement fits an empty control pipe. The only
      // subsequent blocking operation before the result is the transfer.
      if (write(ready_[1], "r", 1) != 1) _exit(1);
      close(ready_[1]);
      TransferResult result;
      result.count = Transfer(0);
      result.error = errno;
      if (write(result_[1], &result, sizeof(result)) != sizeof(result)) _exit(2);
      _exit(0);
    }
    close(ready_[1]);
    ready_[1] = -1;
    close(result_[1]);
    result_[1] = -1;
    ASSERT_EQ(1, PollReadable(ready_[0], 2000));
    char token;
    ASSERT_EQ(1, read(ready_[0], &token, 1));
    ASSERT_EQ('r', token);
    // With initial source data (or EOF) and a full destination, S/D after
    // the acknowledgement proves the destination preparation is waiting.
    ASSERT_TRUE(WaitForTransferSleep());
  }

  void ExpectResult(ssize_t expected) {
    ASSERT_EQ(1, PollReadable(result_[0], 3000))
        << "transfer did not finish after destination became writable";
    TransferResult result = {};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(result)),
              read(result_[0], &result, sizeof(result)));
    ASSERT_EQ(expected, result.count) << "errno=" << result.error
                                     << " (" << strerror(result.error) << ")";
    int status = 0;
    ASSERT_TRUE(ReapChild(&status))
        << "child " << child_ << " reported a result but did not exit";
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
  }

  int source_[2] = {-1, -1};
  int destination_[2] = {-1, -1};
  int ready_[2] = {-1, -1};
  int result_[2] = {-1, -1};
  int epoll_ = -1;
  pid_t child_ = -1;
  std::vector<char> filler_;
  struct sigaction old_sigpipe_ = {};
  bool signal_installed_ = false;
};

TEST_P(PipeTransferState, EmptyEofWithWritableOutputReturnsZero) {
  ASSERT_NO_FATAL_FAILURE(CloseWriter());
  for (unsigned int flags : {0u, static_cast<unsigned int>(SPLICE_F_NONBLOCK)}) {
    EXPECT_EQ(0, Transfer(flags)) << strerror(errno);
  }
}

TEST_P(PipeTransferState, EmptyEofDoesNotRepublishEdgeTriggeredHangup) {
  epoll_ = epoll_create1(EPOLL_CLOEXEC);
  ASSERT_GE(epoll_, 0);
  epoll_event watch = {};
  watch.events = EPOLLIN | EPOLLET;
  watch.data.fd = source_[0];
  ASSERT_EQ(0, epoll_ctl(epoll_, EPOLL_CTL_ADD, source_[0], &watch));
  ASSERT_NO_FATAL_FAILURE(CloseWriter());
  epoll_event event = {};
  ASSERT_EQ(1, epoll_wait(epoll_, &event, 1, 1000));
  ASSERT_EQ(source_[0], event.data.fd);
  ASSERT_NE(0u, event.events & EPOLLHUP);
  ASSERT_EQ(0, epoll_wait(epoll_, &event, 1, 0));
  for (unsigned int flags : {0u, static_cast<unsigned int>(SPLICE_F_NONBLOCK)}) {
    for (int iteration = 0; iteration < 4; ++iteration) {
      ASSERT_EQ(0, Transfer(flags)) << strerror(errno);
      EXPECT_EQ(0, epoll_wait(epoll_, &event, 1, 0))
          << "EOF transfer republished an unchanged hangup: flags=" << flags
          << " iteration=" << iteration << " events=" << event.events;
    }
  }
}

TEST_P(PipeTransferState, EmptyEofStillRejectsFullNonblockingOutput) {
  ASSERT_NO_FATAL_FAILURE(CloseWriter());
  ASSERT_NO_FATAL_FAILURE(FillDestination());
  errno = 0;
  EXPECT_EQ(-1, Transfer(SPLICE_F_NONBLOCK));
  EXPECT_EQ(EAGAIN, errno);
}

TEST_P(PipeTransferState, EmptyEofStillSignalsBrokenOutput) {
  ASSERT_NO_FATAL_FAILURE(CloseWriter());
  ASSERT_EQ(0, close(destination_[0]));
  destination_[0] = -1;
  for (unsigned int flags : {0u, static_cast<unsigned int>(SPLICE_F_NONBLOCK)}) {
    const sig_atomic_t before = sigpipe_count;
    errno = 0;
    EXPECT_EQ(-1, Transfer(flags));
    EXPECT_EQ(EPIPE, errno);
    EXPECT_EQ(before + 1, sigpipe_count);
  }
}

TEST_P(PipeTransferState, EmptyLiveSourceNonblockingPrecedesOutputErrors) {
  errno = 0;
  EXPECT_EQ(-1, Transfer(SPLICE_F_NONBLOCK));
  EXPECT_EQ(EAGAIN, errno);
  ASSERT_EQ(0, close(destination_[0]));
  destination_[0] = -1;
  errno = 0;
  EXPECT_EQ(-1, Transfer(SPLICE_F_NONBLOCK));
  EXPECT_EQ(EAGAIN, errno);
  EXPECT_EQ(0, sigpipe_count);
}

TEST_P(PipeTransferState, EmptyEofWaitsForFullOutputThenReturnsZero) {
  ASSERT_NO_FATAL_FAILURE(CloseWriter());
  ASSERT_NO_FATAL_FAILURE(FillDestination());
  ASSERT_NO_FATAL_FAILURE(StartBlockedTransfer());
  ASSERT_NO_FATAL_FAILURE(DrainDestination());
  ExpectResult(0);
}

TEST_P(PipeTransferState, SourceConsumedAtEofDuringOutputWaitReturnsZero) {
  ASSERT_EQ(1, write(source_[1], "s", 1));
  ASSERT_NO_FATAL_FAILURE(FillDestination());
  ASSERT_NO_FATAL_FAILURE(StartBlockedTransfer());
  char old;
  ASSERT_EQ(1, read(source_[0], &old, 1));
  ASSERT_EQ('s', old);
  ASSERT_NO_FATAL_FAILURE(CloseWriter());
  ASSERT_NO_FATAL_FAILURE(DrainDestination());
  ExpectResult(0);
}

TEST_P(PipeTransferState, ConsumedSourceWithWriterUsesOperationSpecificResult) {
  ASSERT_EQ(1, write(source_[1], "s", 1));
  ASSERT_NO_FATAL_FAILURE(FillDestination());
  ASSERT_NO_FATAL_FAILURE(StartBlockedTransfer());
  char old;
  ASSERT_EQ(1, read(source_[0], &old, 1));
  ASSERT_EQ('s', old);
  ASSERT_NO_FATAL_FAILURE(DrainDestination());
  if (GetParam()) {
    // link_pipe's final empty snapshot returns zero even with a live writer.
    ExpectResult(0);
  } else {
    // splice must retry rather than report a false EOF while writers remain.
    ASSERT_EQ(0, PollReadable(result_[0], 100));
    ASSERT_EQ(1, write(source_[1], "n", 1));
    ASSERT_NO_FATAL_FAILURE(ExpectResult(1));
    char transferred;
    ASSERT_EQ(1, read(destination_[0], &transferred, 1));
    EXPECT_EQ('n', transferred);
  }
}

INSTANTIATE_TEST_SUITE_P(
    SpliceAndTee, PipeTransferState, testing::Values(false, true),
    [](const testing::TestParamInfo<bool>& info) {
      return info.param ? "Tee" : "Splice";
    });

}  // namespace

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
