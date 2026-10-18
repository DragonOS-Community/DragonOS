#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <algorithm>

namespace {

class PipeTransferCopy : public testing::TestWithParam<bool> {
 protected:
  void SetUp() override {
    ASSERT_EQ(0, pipe2(source_, O_CLOEXEC | O_NONBLOCK));
    ASSERT_EQ(0, pipe2(destination_, O_CLOEXEC | O_NONBLOCK));
    const long page = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page, 0);
    ASSERT_EQ(page, fcntl(source_[1], F_SETPIPE_SZ, page));
    ASSERT_EQ(0, fcntl(source_[1], F_SETFL, 0));
  }

  void TearDown() override {
    if (writer_ > 0) {
      kill(writer_, SIGKILL);
      int status;
      while (waitpid(writer_, &status, 0) < 0 && errno == EINTR) {}
    }
    for (int fd : source_) if (fd >= 0) close(fd);
    for (int fd : destination_) if (fd >= 0) close(fd);
  }

  static char ByteAt(size_t offset) {
    return static_cast<char>((offset * 17 + offset / 257) % 251);
  }

  int source_[2] = {-1, -1};
  int destination_[2] = {-1, -1};
  pid_t writer_ = -1;
};

TEST_P(PipeTransferCopy, NonblockingTransferWaitsForConcurrentSourceCopy) {
  constexpr size_t record_size = 257;
  constexpr size_t records = 8192;
  constexpr size_t expected = record_size * records;
  writer_ = fork();
  ASSERT_GE(writer_, 0) << strerror(errno);
  if (writer_ == 0) {
    close(source_[0]);
    close(destination_[0]);
    close(destination_[1]);
    alarm(12);
    char record[record_size];
    for (size_t index = 0; index < records; ++index) {
      for (size_t byte = 0; byte < record_size; ++byte) {
        record[byte] = ByteAt(index * record_size + byte);
      }
      if (write(source_[1], record, sizeof(record)) != sizeof(record)) _exit(1);
    }
    _exit(0);
  }
  close(source_[1]);
  source_[1] = -1;

  // The parent is the only source consumer and empties the destination before
  // each transfer. Neither endpoint can legitimately lose readiness to another
  // consumer. A source writer's copy must serialize with tee/splice even when
  // SPLICE_F_NONBLOCK is set; never retry an unexpected EAGAIN here.
  const bool duplicate = GetParam();
  size_t received = 0;
  ssize_t transferred = 0;
  int transfer_error = 0;
  short events = 0;
  const char* failure = "deadline";
  timespec start = {};
  ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &start));
  while (received < expected) {
    timespec now = {};
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 ||
        now.tv_sec - start.tv_sec >= 15) break;
    pollfd ready = {source_[0], POLLIN, 0};
    const int count = poll(&ready, 1, 100);
    if (count == 0) continue;
    events = ready.revents;
    failure = "source readiness";
    if (count != 1 || !(events & POLLIN)) break;
    const size_t length = std::min<size_t>(113, expected - received);
    transferred = duplicate
        ? tee(source_[0], destination_[1], length, SPLICE_F_NONBLOCK)
        : splice(source_[0], nullptr, destination_[1], nullptr, length,
                 SPLICE_F_NONBLOCK);
    transfer_error = errno;
    failure = "transfer after POLLIN";
    if (transferred <= 0 || transferred > static_cast<ssize_t>(length)) break;
    char output[113];
    failure = "destination length";
    if (read(destination_[0], output, transferred) != transferred) break;
    if (duplicate) {
      // Consume exactly the duplicated prefix, rather than repeatedly counting
      // the same source bytes as progress. No other reader can take them.
      char input[113];
      failure = "tee source length";
      if (read(source_[0], input, transferred) != transferred) break;
      failure = "tee source/destination mismatch";
      if (memcmp(input, output, transferred) != 0) break;
    }
    bool matches = true;
    for (ssize_t byte = 0; byte < transferred; ++byte) {
      if (output[byte] != ByteAt(received + byte)) matches = false;
    }
    failure = "stream contents";
    if (!matches) break;
    received += transferred;
  }
  if (received != expected) kill(writer_, SIGKILL);
  int status = 0;
  pid_t waited;
  do { waited = waitpid(writer_, &status, 0); } while (waited < 0 && errno == EINTR);
  if (waited == writer_) writer_ = -1;
  ASSERT_GT(waited, 0) << strerror(errno);
  ASSERT_EQ(expected, received) << failure << ": transfer=" << transferred
      << " errno=" << transfer_error << " (" << strerror(transfer_error)
      << ") revents=" << events;
  ASSERT_TRUE(WIFEXITED(status)) << "writer status=" << status;
  EXPECT_EQ(0, WEXITSTATUS(status));
}

// Exercise the dual-pipe lock order from both directions with ready endpoints.
// Nonblocking controls capacity waits, not the order of internal pipe locks.
TEST_P(PipeTransferCopy, OppositeTransfersUseOneLockOrder) {
  const long page = sysconf(_SC_PAGESIZE);
  ASSERT_GT(page, 0);
  ASSERT_EQ(2 * page, fcntl(source_[1], F_SETPIPE_SZ, 2 * page));
  ASSERT_EQ(2 * page, fcntl(destination_[1], F_SETPIPE_SZ, 2 * page));
  for (int round = 0; round < 32; ++round) {
    ASSERT_EQ(1, write(source_[1], "q", 1));
    ASSERT_EQ(1, write(destination_[1], "q", 1));
    int start[2];
    ASSERT_EQ(0, pipe2(start, O_CLOEXEC));
    pid_t children[2] = {-1, -1};
    int statuses[2] = {};
    bool reaped[2] = {};
    for (int index = 0; index < 2; ++index) {
      children[index] = fork();
      if (children[index] == 0) {
        close(start[1]);
        alarm(5);
        char token;
        if (read(start[0], &token, 1) != 1) _exit(1);
        const int input = index == 0 ? source_[0] : destination_[0];
        const int output = index == 0 ? destination_[1] : source_[1];
        const ssize_t count = GetParam()
            ? tee(input, output, 1, SPLICE_F_NONBLOCK)
            : splice(input, nullptr, output, nullptr, 1, SPLICE_F_NONBLOCK);
        _exit(count == 1 ? 0 : 2);
      }
      if (children[index] < 0) break;
    }
    // Keep the parent's read end open until release so a failed fork cannot
    // turn this control write into SIGPIPE before the children are cleaned up.
    const ssize_t released = write(start[1], "ss", 2);
    close(start[0]);
    close(start[1]);
    bool complete = false;
    timespec begin = {};
    clock_gettime(CLOCK_MONOTONIC, &begin);
    while (!complete) {
      complete = true;
      for (int index = 0; index < 2; ++index) {
        if (children[index] <= 0 || reaped[index]) continue;
        const pid_t result = waitpid(children[index], &statuses[index], WNOHANG);
        if (result == children[index]) reaped[index] = true;
        if (!reaped[index]) complete = false;
      }
      timespec now = {};
      clock_gettime(CLOCK_MONOTONIC, &now);
      if (complete || now.tv_sec - begin.tv_sec >= 5) break;
      usleep(1000);
    }
    // Never turn a lock-order failure into an unbounded blocking waitpid.
    for (int index = 0; index < 2; ++index) {
      if (children[index] > 0 && !reaped[index]) kill(children[index], SIGKILL);
    }
    for (int attempt = 0; attempt < 1000 && !complete; ++attempt) {
      complete = true;
      for (int index = 0; index < 2; ++index) {
        if (children[index] <= 0 || reaped[index]) continue;
        if (waitpid(children[index], &statuses[index], WNOHANG) == children[index]) {
          reaped[index] = true;
        }
        if (!reaped[index]) complete = false;
      }
      if (!complete) usleep(1000);
    }
    ASSERT_EQ(2, released);
    for (int index = 0; index < 2; ++index) {
      ASSERT_GT(children[index], 0) << "round=" << round;
      ASSERT_TRUE(reaped[index]) << "unresponsive transfer, round=" << round;
      ASSERT_TRUE(WIFEXITED(statuses[index])) << "round=" << round
                                           << " status=" << statuses[index];
      ASSERT_EQ(0, WEXITSTATUS(statuses[index])) << "round=" << round;
    }
    size_t bytes = 0;
    for (int fd : {source_[0], destination_[0]}) {
      char data[8];
      ssize_t count;
      while ((count = read(fd, data, sizeof(data))) > 0) {
        bytes += count;
        for (ssize_t index = 0; index < count; ++index) EXPECT_EQ('q', data[index]);
      }
      ASSERT_EQ(-1, count);
      ASSERT_EQ(EAGAIN, errno);
    }
    ASSERT_EQ(GetParam() ? 4u : 2u, bytes) << "round=" << round;
  }
}

INSTANTIATE_TEST_SUITE_P(
    SpliceAndTee, PipeTransferCopy, testing::Values(false, true),
    [](const testing::TestParamInfo<bool>& info) {
      return info.param ? "Tee" : "Splice";
    });

}  // namespace

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
