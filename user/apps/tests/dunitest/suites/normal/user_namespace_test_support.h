#pragma once
#include <errno.h>
#include <fcntl.h>
#include <cstdio>
#include <cstring>
#include <unistd.h>

namespace dunitest {
// A single self mapping is permitted without capabilities in the parent
// namespace. Call immediately after CLONE_NEWUSER with the saved caller IDs.
inline bool install_self_user_namespace_maps(uid_t parent_uid, gid_t parent_gid) {
    auto write_map = [](const char* path, const char* value) {
        int fd = open(path, O_WRONLY | O_CLOEXEC);
        if (fd < 0) return false;
        const size_t length = strlen(value);
        const bool success = write(fd, value, length) == static_cast<ssize_t>(length);
        const int saved = errno;
        close(fd);
        errno = saved;
        return success;
    };
    char map[64];
    snprintf(map, sizeof(map), "0 %u 1\n", static_cast<unsigned>(parent_uid));
    if (!write_map("/proc/self/uid_map", map) ||
        !write_map("/proc/self/setgroups", "deny\n")) return false;
    snprintf(map, sizeof(map), "0 %u 1\n", static_cast<unsigned>(parent_gid));
    return write_map("/proc/self/gid_map", map);
}
} // namespace dunitest
