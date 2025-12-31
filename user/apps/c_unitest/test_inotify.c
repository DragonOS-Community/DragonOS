#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

static int has_event(const struct inotify_event *ev, uint32_t mask) {
    return (ev->mask & mask) == mask;
}

int main(void) {
    const char *dir = "inotify_tmp";
    const char *file_a = "inotify_tmp/a.txt";
    const char *file_b = "inotify_tmp/b.txt";

    if (mkdir(dir, 0700) != 0 && errno != EEXIST) {
        perror("mkdir");
        return 1;
    }

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    if (ifd < 0) {
        perror("inotify_init1");
        return 1;
    }

    int wd_dir = inotify_add_watch(ifd, dir, IN_CREATE | IN_DELETE | IN_MOVED_FROM | IN_MOVED_TO);
    if (wd_dir < 0) {
        perror("inotify_add_watch(dir)");
        return 1;
    }

    int fd = open(file_a, O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (fd < 0) {
        perror("open(a)");
        return 1;
    }

    int wd_file = inotify_add_watch(ifd, file_a, IN_MODIFY | IN_MOVE_SELF | IN_DELETE_SELF);
    if (wd_file < 0) {
        perror("inotify_add_watch(file)");
        return 1;
    }

    const char msg[] = "hello\n";
    if (write(fd, msg, sizeof(msg) - 1) != (ssize_t)(sizeof(msg) - 1)) {
        perror("write");
        return 1;
    }

    if (rename(file_a, file_b) != 0) {
        perror("rename");
        return 1;
    }

    if (unlink(file_b) != 0) {
        perror("unlink");
        return 1;
    }

    close(fd);

    int seen_create = 0;
    int seen_modify = 0;
    int seen_moved_from = 0;
    int seen_moved_to = 0;
    int seen_move_self = 0;
    int seen_delete = 0;
    int seen_delete_self = 0;
    int seen_ignored = 0;

    char buf[4096];

    for (int iter = 0; iter < 50; iter++) {
        struct pollfd pfd = {0};
        pfd.fd = ifd;
        pfd.events = POLLIN;
        int pr = poll(&pfd, 1, 50);
        if (pr < 0) {
            perror("poll");
            return 1;
        }
        if (pr == 0) {
            continue;
        }

        ssize_t n = read(ifd, buf, sizeof(buf));
        if (n < 0) {
            if (errno == EAGAIN) {
                continue;
            }
            perror("read(inotify)");
            return 1;
        }
        if (n == 0) {
            continue;
        }

        for (ssize_t off = 0; off < n;) {
            struct inotify_event *ev = (struct inotify_event *)(buf + off);

            if (ev->wd == wd_dir) {
                if (has_event(ev, IN_CREATE) && ev->len && strcmp(ev->name, "a.txt") == 0) {
                    seen_create = 1;
                }
                if (has_event(ev, IN_MOVED_FROM) && ev->len && strcmp(ev->name, "a.txt") == 0) {
                    seen_moved_from = 1;
                }
                if (has_event(ev, IN_MOVED_TO) && ev->len && strcmp(ev->name, "b.txt") == 0) {
                    seen_moved_to = 1;
                }
                if (has_event(ev, IN_DELETE) && ev->len && strcmp(ev->name, "b.txt") == 0) {
                    seen_delete = 1;
                }
            }

            if (ev->wd == wd_file) {
                if (has_event(ev, IN_MODIFY)) {
                    seen_modify = 1;
                }
                if (has_event(ev, IN_MOVE_SELF)) {
                    seen_move_self = 1;
                }
                if (has_event(ev, IN_DELETE_SELF)) {
                    seen_delete_self = 1;
                }
                if (has_event(ev, IN_IGNORED)) {
                    seen_ignored = 1;
                }
            }

            off += (ssize_t)(sizeof(struct inotify_event) + ev->len);
        }

        if (seen_create && seen_modify && seen_moved_from && seen_moved_to && seen_move_self &&
            seen_delete && seen_delete_self && seen_ignored) {
            break;
        }
    }

    close(ifd);

    if (!(seen_create && seen_modify && seen_moved_from && seen_moved_to && seen_move_self &&
          seen_delete && seen_delete_self && seen_ignored)) {
        fprintf(stderr,
                "inotify test failed: create=%d modify=%d moved_from=%d moved_to=%d move_self=%d delete=%d delete_self=%d ignored=%d\n",
                seen_create,
                seen_modify,
                seen_moved_from,
                seen_moved_to,
                seen_move_self,
                seen_delete,
                seen_delete_self,
                seen_ignored);
        return 1;
    }

    printf("inotify test passed\n");
    return 0;
}
