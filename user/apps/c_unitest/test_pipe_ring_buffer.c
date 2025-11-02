#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define KBUF 1024

static void die(const char *msg) {
    perror(msg);
    exit(1);
}

static void fill(char *buf, size_t n, char c) {
    memset(buf, c, n);
}

static void expect_all(const char *buf, size_t n, char c, const char *label) {
    for (size_t i = 0; i < n; ++i) {
        if (buf[i] != c) {
            fprintf(stderr, "%s: mismatch at %zu: got %d expect %d\n", label, i, (int)buf[i], (int)c);
            exit(2);
        }
    }
}

static void must_write(int fd, const void *buf, size_t n, const char *label) {
    size_t off = 0;
    while (off < n) {
        ssize_t w = write(fd, (const char *)buf + off, n - off);
        if (w < 0) die(label);
        off += (size_t)w;
    }
}

static void must_read(int fd, void *buf, size_t n, const char *label) {
    size_t off = 0;
    while (off < n) {
        ssize_t r = read(fd, (char *)buf + off, n - off);
        if (r < 0) die(label);
        off += (size_t)r;
    }
}

int main(void) {
    int p[2];
    if (pipe(p) < 0) die("pipe");

    char wbuf[KBUF];
    char rbuf[KBUF];

    // Test 1: write exactly 1024 bytes to empty pipe, then read 1024 back.
    fill(wbuf, KBUF, 'a');
    must_write(p[1], wbuf, KBUF, "write full 1024");
    memset(rbuf, 0, sizeof(rbuf));
    must_read(p[0], rbuf, KBUF, "read full 1024");
    expect_all(rbuf, KBUF, 'a', "test1");

    // Test 2: advance positions and force wrap-around: write 600, read 600, then write 1024 and read 1024.
    fill(wbuf, 600, 'b');
    must_write(p[1], wbuf, 600, "write 600");
    memset(rbuf, 0, 600);
    must_read(p[0], rbuf, 600, "read 600");

    fill(wbuf, KBUF, 'c');
    must_write(p[1], wbuf, KBUF, "write wrap 1024");
    memset(rbuf, 0, sizeof(rbuf));
    must_read(p[0], rbuf, KBUF, "read wrap 1024");
    expect_all(rbuf, KBUF, 'c', "test2");

    // Test 3: two half writes then one full read.
    fill(wbuf, KBUF, 'd');
    must_write(p[1], wbuf, 512, "write 512 #1");
    must_write(p[1], wbuf + 512, 512, "write 512 #2");
    memset(rbuf, 0, sizeof(rbuf));
    must_read(p[0], rbuf, KBUF, "read 1024 after two writes");
    for (size_t i = 0; i < 512; ++i) {
        if (rbuf[i] != 'd') { fprintf(stderr, "test3 mismatch at %zu (first half)\n", i); return 3; }
    }
    for (size_t i = 512; i < KBUF; ++i) {
        if (rbuf[i] != 'd') { fprintf(stderr, "test3 mismatch at %zu (second half)\n", i); return 3; }
    }

    close(p[0]);
    close(p[1]);
    printf("test_pipe_ring_buffer: PASS\n");
    return 0;
}
