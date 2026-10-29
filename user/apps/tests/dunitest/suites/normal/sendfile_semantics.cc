#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/sendfile.h>
#include <sys/socket.h>
#include <unistd.h>

#include <algorithm>
#include <string>

namespace {

// DKC051: Linux 6.6 fs/read_write.c do_sendfile/sendfile64 semantics.
class TempFile {
  public:
    TempFile() {
        char path[] = "/tmp/dunitest_sendfile_XXXXXX";
        fd_ = mkstemp(path);
        if (fd_ >= 0) {
            unlink(path);
        }
    }
    ~TempFile() {
        if (fd_ >= 0) close(fd_);
    }
    TempFile(const TempFile&) = delete;
    TempFile& operator=(const TempFile&) = delete;
    int fd() const { return fd_; }

  private:
    int fd_ = -1;
};

class Pipe {
  public:
    Pipe() {
        if (pipe2(fds_, O_NONBLOCK) < 0) {
            fds_[0] = fds_[1] = -1;
        }
    }
    ~Pipe() {
        for (int fd : fds_) {
            if (fd >= 0) close(fd);
        }
    }
    int read_fd() const { return fds_[0]; }
    int write_fd() const { return fds_[1]; }
    int CloseReadEnd() {
        const int fd = fds_[0];
        fds_[0] = -1;
        return close(fd);
    }

  private:
    int fds_[2] = {-1, -1};
};

class SocketPair {
  public:
    SocketPair() {
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, fds_) < 0) {
            fds_[0] = fds_[1] = -1;
        }
    }
    ~SocketPair() {
        for (int fd : fds_) {
            if (fd >= 0) close(fd);
        }
    }
    int receive_fd() const { return fds_[0]; }
    int send_fd() const { return fds_[1]; }

  private:
    int fds_[2] = {-1, -1};
};

std::string Payload(size_t size) {
    std::string data(size, '\0');
    for (size_t i = 0; i < size; ++i) {
        data[i] = static_cast<char>('!' + (i * 17 + i / 101) % 90);
    }
    return data;
}

void WriteAll(int fd, const std::string& data) {
    size_t done = 0;
    while (done < data.size()) {
        const ssize_t n = write(fd, data.data() + done, data.size() - done);
        ASSERT_GT(n, 0) << errno;
        done += static_cast<size_t>(n);
    }
}

std::string ReadBytes(int fd, size_t size) {
    std::string data(size, '\0');
    size_t done = 0;
    while (done < size) {
        const ssize_t n = read(fd, &data[done], size - done);
        if (n <= 0) {
            ADD_FAILURE() << "read returned " << n << ", errno=" << errno;
            data.resize(done);
            break;
        }
        done += static_cast<size_t>(n);
    }
    return data;
}

class SendfileOffsets : public ::testing::TestWithParam<bool> {};

TEST_P(SendfileOffsets, SuccessiveCallsAdvanceOnlyAcceptedBytesAndReachEof) {
    TempFile input, output;
    ASSERT_GE(input.fd(), 0);
    ASSERT_GE(output.fd(), 0);
    const std::string data = Payload(10003);
    ASSERT_NO_FATAL_FAILURE(WriteAll(input.fd(), data));
    ASSERT_NO_FATAL_FAILURE(WriteAll(output.fd(), "prefix"));
    const bool explicit_offset = GetParam();
    const off_t initial = 19;
    ASSERT_EQ(explicit_offset ? 3 : initial,
              lseek(input.fd(), explicit_offset ? 3 : initial, SEEK_SET));
    off_t offset = initial;
    off_t progress = initial;
    for (size_t count : {4097u, 4097u, 9000u, 1u}) {
        const ssize_t expected = static_cast<ssize_t>(
            std::min(count, data.size() - static_cast<size_t>(progress)));
        ASSERT_EQ(expected, sendfile(output.fd(), input.fd(),
                                     explicit_offset ? &offset : nullptr, count))
            << errno;
        progress += expected;
        EXPECT_EQ(explicit_offset ? progress : initial, offset);
        EXPECT_EQ(explicit_offset ? 3 : progress, lseek(input.fd(), 0, SEEK_CUR));
        EXPECT_EQ(6 + progress - initial, lseek(output.fd(), 0, SEEK_CUR));
    }
    ASSERT_EQ(0, lseek(output.fd(), 0, SEEK_SET));
    EXPECT_EQ("prefix" + data.substr(initial),
              ReadBytes(output.fd(), 6 + data.size() - initial));
}

TEST_P(SendfileOffsets, NonblockingPipeShortSendAndEagainPreserveProgress) {
    TempFile input;
    Pipe output;
    ASSERT_GE(input.fd(), 0);
    ASSERT_GE(output.read_fd(), 0);
    const long page = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page, 0);
    const int capacity = fcntl(output.write_fd(), F_GETPIPE_SZ);
    ASSERT_GE(capacity, page);
    const std::string filler(static_cast<size_t>(capacity), '#');
    ASSERT_NO_FATAL_FAILURE(WriteAll(output.write_fd(), filler));
    ASSERT_EQ(std::string(static_cast<size_t>(page), '#'),
              ReadBytes(output.read_fd(), static_cast<size_t>(page)));
    const std::string data = Payload(static_cast<size_t>(capacity) * 2 + 17);
    ASSERT_NO_FATAL_FAILURE(WriteAll(input.fd(), data));
    const bool explicit_offset = GetParam();
    ASSERT_EQ(explicit_offset ? 3 : 7,
              lseek(input.fd(), explicit_offset ? 3 : 7, SEEK_SET));
    off_t offset = 7;
    const ssize_t sent = sendfile(output.write_fd(), input.fd(),
                                  explicit_offset ? &offset : nullptr, data.size() - 7);
    ASSERT_GT(sent, 0) << errno;
    ASSERT_LE(sent, page);
    EXPECT_EQ(explicit_offset ? 7 + sent : 7, offset);
    EXPECT_EQ(explicit_offset ? 3 : 7 + sent, lseek(input.fd(), 0, SEEK_CUR));

    errno = 0;
    EXPECT_EQ(-1, sendfile(output.write_fd(), input.fd(),
                           explicit_offset ? &offset : nullptr, 1));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(explicit_offset ? 7 + sent : 7, offset);
    EXPECT_EQ(explicit_offset ? 3 : 7 + sent, lseek(input.fd(), 0, SEEK_CUR));
    EXPECT_EQ(std::string(static_cast<size_t>(capacity - page), '#') +
                  data.substr(7, static_cast<size_t>(sent)),
              ReadBytes(output.read_fd(), static_cast<size_t>(capacity - page + sent)));

    const ssize_t next = sendfile(output.write_fd(), input.fd(),
                                  explicit_offset ? &offset : nullptr, page);
    ASSERT_GT(next, 0) << errno;
    EXPECT_EQ(data.substr(static_cast<size_t>(7 + sent), static_cast<size_t>(next)),
              ReadBytes(output.read_fd(), static_cast<size_t>(next)));
    EXPECT_EQ(explicit_offset ? 7 + sent + next : 7, offset);
    EXPECT_EQ(explicit_offset ? 3 : 7 + sent + next, lseek(input.fd(), 0, SEEK_CUR));
}

INSTANTIATE_TEST_SUITE_P(ImplicitAndExplicit, SendfileOffsets, ::testing::Bool());

TEST(SendfileSemantics, ZeroCountStillValidatesDescriptorsModesAndAppend) {
    TempFile input, output;
    ASSERT_GE(input.fd(), 0);
    ASSERT_GE(output.fd(), 0);
    for (size_t count : {0u, 1u}) {
        SCOPED_TRACE(count);
        for (bool bad_input : {false, true}) {
            errno = 0;
            EXPECT_EQ(-1, sendfile(bad_input ? output.fd() : -1,
                                   bad_input ? -1 : input.fd(), nullptr, count));
            EXPECT_EQ(EBADF, errno);
        }
        Pipe pipe;
        ASSERT_GE(pipe.read_fd(), 0);
        errno = 0;
        EXPECT_EQ(-1, sendfile(output.fd(), pipe.write_fd(), nullptr, count));
        EXPECT_EQ(EBADF, errno);
        errno = 0;
        EXPECT_EQ(-1, sendfile(pipe.read_fd(), input.fd(), nullptr, count));
        EXPECT_EQ(EBADF, errno);

        ASSERT_EQ(0, fcntl(output.fd(), F_SETFL, O_APPEND));
        errno = 0;
        EXPECT_EQ(-1, sendfile(output.fd(), input.fd(), nullptr, count));
        EXPECT_EQ(EINVAL, errno);
        ASSERT_EQ(0, fcntl(output.fd(), F_SETFL, 0));
        off_t offset = -1;
        errno = 0;
        EXPECT_EQ(-1, sendfile(output.fd(), input.fd(), &offset, count));
        EXPECT_EQ(EINVAL, errno);
        EXPECT_EQ(-1, offset);
        errno = 0;
        EXPECT_EQ(-1, sendfile(output.fd(), -1, &offset, count));
        EXPECT_EQ(EBADF, errno);
        EXPECT_EQ(-1, offset);
    }
    EXPECT_EQ(0, sendfile(output.fd(), input.fd(), nullptr, 0));
    EXPECT_EQ(0, lseek(input.fd(), 0, SEEK_CUR));
    EXPECT_EQ(0, lseek(output.fd(), 0, SEEK_CUR));
}

TEST(SendfileSemantics, ReadOnlyOffsetCopyoutFaultOccursAfterTransferAndOverridesError) {
    TempFile input, output;
    ASSERT_GE(input.fd(), 0);
    ASSERT_GE(output.fd(), 0);
    ASSERT_NO_FATAL_FAILURE(WriteAll(input.fd(), "abcdef"));
    ASSERT_EQ(1, lseek(input.fd(), 1, SEEK_SET));
    const long page = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page, 0);
    void* mapping = mmap(nullptr, page, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    auto* offset = static_cast<off_t*>(mapping);
    *offset = 2;
    ASSERT_EQ(0, mprotect(mapping, page, PROT_READ));
    errno = 0;
    EXPECT_EQ(-1, sendfile(output.fd(), input.fd(), offset, 3));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(2, *offset);
    EXPECT_EQ(1, lseek(input.fd(), 0, SEEK_CUR));
    EXPECT_EQ(3, lseek(output.fd(), 0, SEEK_CUR));
    EXPECT_EQ(0, lseek(output.fd(), 0, SEEK_SET));
    EXPECT_EQ("cde", ReadBytes(output.fd(), 3));
    // sendfile64 performs put_user even when do_sendfile fails or returns zero.
    errno = 0;
    EXPECT_EQ(-1, sendfile(output.fd(), input.fd(), offset, 0));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(3, lseek(output.fd(), 0, SEEK_CUR));
    for (size_t count : {0u, 1u}) {
        errno = 0;
        EXPECT_EQ(-1, sendfile(output.fd(), -1, offset, count));
        EXPECT_EQ(EFAULT, errno);
    }
    ASSERT_EQ(0, mprotect(mapping, page, PROT_READ | PROT_WRITE));
    *offset = -1;
    ASSERT_EQ(0, mprotect(mapping, page, PROT_READ));
    errno = 0;
    EXPECT_EQ(-1, sendfile(output.fd(), input.fd(), offset, 1));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(-1, *offset);
    ASSERT_EQ(0, munmap(mapping, page));
}

TEST(SendfileSemantics, LargeUnsignedCountIsClampedButNegativeSignedCountIsRejected) {
    TempFile input, output;
    ASSERT_GE(input.fd(), 0);
    ASSERT_GE(output.fd(), 0);
    const std::string data = "short source avoids a multi-gigabyte transfer";
    ASSERT_NO_FATAL_FAILURE(WriteAll(input.fd(), data));
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    const size_t max_rw_count = static_cast<size_t>(INT_MAX) & ~(page - 1);
    for (size_t count : {max_rw_count + page, static_cast<size_t>(SSIZE_MAX)}) {
        off_t offset = 0;
        ASSERT_EQ(static_cast<ssize_t>(data.size()),
                  sendfile(output.fd(), input.fd(), &offset, count)) << errno;
        EXPECT_EQ(static_cast<off_t>(data.size()), offset);
    }
    const off_t output_pos = lseek(output.fd(), 0, SEEK_CUR);
    off_t offset = 0;
    errno = 0;
    EXPECT_EQ(-1, sendfile(output.fd(), input.fd(), &offset, SIZE_MAX));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0, offset);
    EXPECT_EQ(static_cast<off_t>(data.size()), lseek(input.fd(), 0, SEEK_CUR));
    EXPECT_EQ(output_pos, lseek(output.fd(), 0, SEEK_CUR));
    ASSERT_EQ(0, lseek(output.fd(), 0, SEEK_SET));
    EXPECT_EQ(data + data, ReadBytes(output.fd(), data.size() * 2));
}

TEST(SendfileSemantics, SameOpenFileDescriptionUsesOnePositionAndContinuesShortReads) {
    TempFile implicit_file, explicit_file;
    ASSERT_GE(implicit_file.fd(), 0);
    ASSERT_GE(explicit_file.fd(), 0);
    ASSERT_NO_FATAL_FAILURE(WriteAll(implicit_file.fd(), "abcdef"));
    ASSERT_EQ(0, lseek(implicit_file.fd(), 0, SEEK_SET));
    ASSERT_EQ(3, sendfile(implicit_file.fd(), implicit_file.fd(), nullptr, 3)) << errno;
    // Linux commits the input position last, rather than adding both positions.
    EXPECT_EQ(3, lseek(implicit_file.fd(), 0, SEEK_CUR));

    ASSERT_NO_FATAL_FAILURE(WriteAll(explicit_file.fd(), "x"));
    off_t offset = 0;
    // Each short read exposes the bytes just appended. A short read alone is
    // not EOF; continue until the requested count is accepted by the output.
    ASSERT_EQ(8193, sendfile(explicit_file.fd(), explicit_file.fd(), &offset, 8193))
        << errno;
    EXPECT_EQ(8193, offset);
    EXPECT_EQ(8194, lseek(explicit_file.fd(), 0, SEEK_CUR));
    ASSERT_EQ(0, lseek(explicit_file.fd(), 0, SEEK_SET));
    EXPECT_EQ(std::string(8194, 'x'), ReadBytes(explicit_file.fd(), 8194));
}

TEST(SendfileSemantics, OriginalCountOverflowIsCheckedBeforeClamping) {
    TempFile input, output;
    ASSERT_GE(input.fd(), 0);
    ASSERT_GE(output.fd(), 0);
    ASSERT_NO_FATAL_FAILURE(WriteAll(input.fd(), "abc"));
    // Even an otherwise clampable count must pass rw_verify_area first.
    off_t offset = 1;
    errno = 0;
    EXPECT_EQ(-1, sendfile(output.fd(), input.fd(), &offset,
                           static_cast<size_t>(SSIZE_MAX)));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(1, offset);
    offset = static_cast<off_t>(SSIZE_MAX) - 1;
    errno = 0;
    EXPECT_EQ(-1, sendfile(output.fd(), input.fd(), &offset, 3));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(static_cast<off_t>(SSIZE_MAX) - 1, offset);
    EXPECT_EQ(3, lseek(input.fd(), 0, SEEK_CUR));
    EXPECT_EQ(0, lseek(output.fd(), 0, SEEK_END));
}

TEST(SendfileSemantics, UnreadableAndCrossPageOffsetFailBeforeTransfer) {
    TempFile input, output;
    ASSERT_GE(input.fd(), 0);
    ASSERT_GE(output.fd(), 0);
    ASSERT_NO_FATAL_FAILURE(WriteAll(input.fd(), "abcdef"));
    const long page = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page, static_cast<long>(sizeof(off_t)));
    void* mapping = mmap(nullptr, page * 2, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    auto* bytes = static_cast<char*>(mapping);
    ASSERT_EQ(0, mprotect(bytes + page, page, PROT_NONE));
    auto* unreadable = reinterpret_cast<off_t*>(bytes + page);
    auto* crossing = reinterpret_cast<off_t*>(bytes + page - sizeof(off_t) / 2);
    // The kernel must copy all of off_t; a readable first half is insufficient.
    for (off_t* offset : {unreadable, crossing}) {
        errno = 0;
        EXPECT_EQ(-1, sendfile(output.fd(), input.fd(), offset, 3));
        EXPECT_EQ(EFAULT, errno);
        EXPECT_EQ(6, lseek(input.fd(), 0, SEEK_CUR));
        EXPECT_EQ(0, lseek(output.fd(), 0, SEEK_END));
    }
    ASSERT_EQ(0, munmap(mapping, page * 2));
}

TEST(SendfileSemantics, SocketToPipeAllowsZeroCountAndPipeAppendFlag) {
    // Since Linux 5.12 sendfile to a pipe follows the splice input rules.
    // O_APPEND is rejected for regular-file output, but not for pipe output.
    for (bool append : {false, true}) {
        SCOPED_TRACE(append);
        SocketPair input;
        Pipe output;
        ASSERT_GE(input.receive_fd(), 0);
        ASSERT_GE(output.write_fd(), 0);
        if (append) {
            ASSERT_EQ(0, fcntl(output.write_fd(), F_SETFL, O_NONBLOCK | O_APPEND));
        }
        ASSERT_NO_FATAL_FAILURE(WriteAll(input.send_fd(), "abc"));
        ASSERT_EQ(0, sendfile(output.write_fd(), input.receive_fd(), nullptr, 0))
            << errno;
        ASSERT_EQ(3, sendfile(output.write_fd(), input.receive_fd(), nullptr, 3))
            << errno;
        EXPECT_EQ("abc", ReadBytes(output.read_fd(), 3));
    }
}

TEST(SendfileSemantics, FullPipeDoesNotConsumeSocketAndRegularOutputRejectsSocket) {
    SocketPair input;
    Pipe output;
    TempFile regular_output;
    ASSERT_GE(input.receive_fd(), 0);
    ASSERT_GE(output.write_fd(), 0);
    ASSERT_GE(regular_output.fd(), 0);
    const int capacity = fcntl(output.write_fd(), F_GETPIPE_SZ);
    ASSERT_GT(capacity, 0);
    const std::string filler(static_cast<size_t>(capacity), '#');
    ASSERT_NO_FATAL_FAILURE(WriteAll(output.write_fd(), filler));
    ASSERT_NO_FATAL_FAILURE(WriteAll(input.send_fd(), "abcdef"));
    errno = 0;
    EXPECT_EQ(-1, sendfile(output.write_fd(), input.receive_fd(), nullptr, 3));
    EXPECT_EQ(EAGAIN, errno);
    char peek[6] = {};
    ASSERT_EQ(6, recv(input.receive_fd(), peek, sizeof(peek), MSG_PEEK | MSG_DONTWAIT))
        << errno;
    EXPECT_EQ("abcdef", std::string(peek, sizeof(peek)));
    EXPECT_EQ(filler, ReadBytes(output.read_fd(), static_cast<size_t>(capacity)));
    ASSERT_EQ(3, sendfile(output.write_fd(), input.receive_fd(), nullptr, 3)) << errno;
    EXPECT_EQ("abc", ReadBytes(output.read_fd(), 3));

    errno = 0;
    EXPECT_EQ(-1, sendfile(regular_output.fd(), input.receive_fd(), nullptr, 3));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0, lseek(regular_output.fd(), 0, SEEK_END));
    ASSERT_EQ(3, recv(input.receive_fd(), peek, sizeof(peek), MSG_PEEK | MSG_DONTWAIT))
        << errno;
    EXPECT_EQ("def", std::string(peek, 3));
}

TEST(SendfileSemantics, PipeSpaceAndReaderChecksPrecedeZeroCountAndInputEof) {
    TempFile empty_input;
    Pipe output;
    ASSERT_GE(empty_input.fd(), 0);
    ASSERT_GE(output.write_fd(), 0);
    const int capacity = fcntl(output.write_fd(), F_GETPIPE_SZ);
    ASSERT_GT(capacity, 0);
    ASSERT_NO_FATAL_FAILURE(WriteAll(output.write_fd(),
                                    std::string(static_cast<size_t>(capacity), '#')));
    // splice_file_to_pipe waits for output space before invoking splice_read,
    // including when the source is empty or no bytes were requested.
    for (size_t count : {0u, 1u}) {
        errno = 0;
        EXPECT_EQ(-1, sendfile(output.write_fd(), empty_input.fd(), nullptr, count));
        EXPECT_EQ(EAGAIN, errno);
        EXPECT_EQ(0, lseek(empty_input.fd(), 0, SEEK_CUR));
    }
    ASSERT_EQ(0, output.CloseReadEnd());
    struct sigaction ignore {}, previous {};
    ignore.sa_handler = SIG_IGN;
    ASSERT_EQ(0, sigemptyset(&ignore.sa_mask));
    ASSERT_EQ(0, sigaction(SIGPIPE, &ignore, &previous));
    // Use only nonfatal expectations while the temporary signal handler is set.
    for (size_t count : {0u, 1u}) {
        errno = 0;
        EXPECT_EQ(-1, sendfile(output.write_fd(), empty_input.fd(), nullptr, count));
        EXPECT_EQ(EPIPE, errno);
        EXPECT_EQ(0, lseek(empty_input.fd(), 0, SEEK_CUR));
    }
    EXPECT_EQ(0, sigaction(SIGPIPE, &previous, nullptr));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
