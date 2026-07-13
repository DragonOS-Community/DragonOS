#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <fstream>
#include <sstream>
#include <string>
#include <vector>

#ifndef CLONE_NEWNS
#define CLONE_NEWNS 0x00020000
#endif

#ifndef MS_REC
#define MS_REC 16384
#endif

#ifndef MS_BIND
#define MS_BIND 4096
#endif

#ifndef MS_PRIVATE
#define MS_PRIVATE (1 << 18)
#endif

namespace {

struct MountInfoEntry {
    size_t mount_id;
    size_t parent_id;
    std::string root;
    std::string mountpoint;
};

int ensure_dir(const std::string& path) {
    struct stat st = {};
    if (stat(path.c_str(), &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path.c_str(), 0755);
}

int create_file(const std::string& path, const char* contents) {
    int fd = open(path.c_str(), O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd < 0) {
        return -1;
    }

    const size_t len = strlen(contents);
    const ssize_t written = write(fd, contents, len);
    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return written == static_cast<ssize_t>(len) ? 0 : -1;
}

std::string read_file(const std::string& path) {
    std::ifstream input(path);
    std::ostringstream contents;
    contents << input.rdbuf();
    return contents.str();
}

bool path_exists(const std::string& path) {
    struct stat st = {};
    return stat(path.c_str(), &st) == 0;
}

std::vector<MountInfoEntry> read_mountinfo() {
    std::ifstream input("/proc/self/mountinfo");
    std::vector<MountInfoEntry> entries;
    std::string line;
    while (std::getline(input, line)) {
        std::istringstream fields(line);
        std::string ignored;
        MountInfoEntry entry;
        // mountinfo: id parent major:minor root mountpoint ...
        if (fields >> entry.mount_id >> entry.parent_id >> ignored >> entry.root >>
                entry.mountpoint) {
            entries.push_back(std::move(entry));
        }
    }
    return entries;
}

bool has_mountinfo_entry(const std::vector<MountInfoEntry>& entries,
                         const std::string& root,
                         const std::string& mountpoint) {
    for (const auto& entry : entries) {
        if (entry.root == root && entry.mountpoint == mountpoint) {
            return true;
        }
    }
    return false;
}

bool has_mountpoint(const std::vector<MountInfoEntry>& entries,
                    const std::string& mountpoint) {
    for (const auto& entry : entries) {
        if (entry.mountpoint == mountpoint) {
            return true;
        }
    }
    return false;
}

size_t count_mountpoint(const std::vector<MountInfoEntry>& entries,
                        const std::string& mountpoint) {
    size_t count = 0;
    for (const auto& entry : entries) {
        count += entry.mountpoint == mountpoint;
    }
    return count;
}

const MountInfoEntry* find_mountpoint(const std::vector<MountInfoEntry>& entries,
                                      const std::string& mountpoint) {
    for (const auto& entry : entries) {
        if (entry.mountpoint == mountpoint) {
            return &entry;
        }
    }
    return nullptr;
}

void best_effort_umount(const std::string& path) {
    for (int i = 0; i < 16; ++i) {
        if (umount2(path.c_str(), MNT_DETACH) == 0) {
            continue;
        }
        if (errno == EINVAL || errno == ENOENT) {
            return;
        }
        return;
    }
}

class MountObjectTopologyTest : public ::testing::Test {
protected:
    std::string base_;

    void SetUp() override {
        ASSERT_EQ(0, ensure_dir("/tmp")) << strerror(errno);
        base_ = "/tmp/mount_object_topology_" + std::to_string(getpid());
        ASSERT_EQ(0, ensure_dir(base_)) << strerror(errno);

        if (unshare(CLONE_NEWNS) != 0) {
            GTEST_SKIP() << "unshare(CLONE_NEWNS): " << strerror(errno);
        }
        ASSERT_EQ(0, mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr))
            << strerror(errno);
    }

    void TearDown() override {
        // A detached namespace is discarded when the test process exits, but
        // clean all paths because later cases clone this process's namespace.
        const char* mount_suffixes[] = {
            "/dest/b/c", "/dest/c", "/dest",       "/source/b/c",
            "/source/a/c", "/source/ab/c", "/source", "/alias_a",
            "/stack/proc", "/stack", "/ordinary/jail/proc", "/ordinary",
            "/source/child",
        };
        for (const char* suffix : mount_suffixes) {
            best_effort_umount(base_ + suffix);
        }

        const char* files[] = {
            "/source_file", "/alias_a", "/alias_b", "/source/b/c/marker",
            "/source/a/c/a_marker", "/source/ab/c/ab_marker",
        };
        for (const char* suffix : files) {
            unlink((base_ + suffix).c_str());
        }

        const char* dirs[] = {
            "/dest/b/c", "/dest/b", "/dest/c", "/dest", "/source/a/b/c",
            "/source/a/b", "/source/a/c", "/source/ab/c", "/source/b/c",
            "/source/a", "/source/ab", "/source/b", "/source",
            "/stack/proc", "/stack", "/ordinary/jail/proc", "/ordinary/jail",
            "/ordinary",
            "/source/child", "/source/other",
        };
        for (const char* suffix : dirs) {
            rmdir((base_ + suffix).c_str());
        }
        rmdir(base_.c_str());
    }
};

}  // namespace

TEST_F(MountObjectTopologyTest, AncestorRenameRecursiveBindTracksObjectTopology) {
    const std::string source = base_ + "/source";
    const std::string old_parent = source + "/a";
    const std::string new_parent = source + "/b";
    const std::string child_mount = old_parent + "/c";
    const std::string renamed_child_mount = new_parent + "/c";
    const std::string dest = base_ + "/dest";
    const std::string dest_child = dest + "/c";

    ASSERT_EQ(0, ensure_dir(source)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(dest)) << strerror(errno);
    ASSERT_EQ(0, mount("none", source.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(old_parent)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(child_mount)) << strerror(errno);
    ASSERT_EQ(0, mount("none", child_mount.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_file(child_mount + "/marker", "child")) << strerror(errno);

    ASSERT_EQ(0, rename(old_parent.c_str(), new_parent.c_str())) << strerror(errno);

    const auto renamed_entries = read_mountinfo();
    EXPECT_TRUE(has_mountinfo_entry(renamed_entries, "/", renamed_child_mount));
    EXPECT_FALSE(has_mountpoint(renamed_entries, child_mount));

    ASSERT_EQ(0, mount(new_parent.c_str(), dest.c_str(), nullptr, MS_BIND | MS_REC, nullptr))
        << strerror(errno);
    EXPECT_EQ("child", read_file(dest_child + "/marker"));

    const auto rebound_entries = read_mountinfo();
    EXPECT_TRUE(has_mountinfo_entry(rebound_entries, "/b", dest))
        << "recursive bind root must report its live superblock-relative root";
    EXPECT_TRUE(has_mountinfo_entry(rebound_entries, "/", dest_child));
    EXPECT_FALSE(has_mountpoint(rebound_entries, child_mount));
}

TEST_F(MountObjectTopologyTest, RecursiveBindFiltersSimilarPrefixSibling) {
    const std::string source = base_ + "/source";
    const std::string source_a = source + "/a";
    const std::string source_ab = source + "/ab";
    const std::string a_child = source_a + "/c";
    const std::string ab_child = source_ab + "/c";
    const std::string misleading_target = source_a + "/b/c";
    const std::string dest = base_ + "/dest";

    ASSERT_EQ(0, ensure_dir(source)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(dest)) << strerror(errno);
    ASSERT_EQ(0, mount("none", source.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(source_a)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(source_ab)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(a_child)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(source_a + "/b")) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(misleading_target)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(ab_child)) << strerror(errno);

    ASSERT_EQ(0, mount("none", a_child.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_file(a_child + "/a_marker", "a")) << strerror(errno);
    ASSERT_EQ(0, mount("none", ab_child.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, create_file(ab_child + "/ab_marker", "ab")) << strerror(errno);

    ASSERT_EQ(0, mount(source_a.c_str(), dest.c_str(), nullptr, MS_BIND | MS_REC, nullptr))
        << strerror(errno);
    EXPECT_EQ("a", read_file(dest + "/c/a_marker"));
    EXPECT_FALSE(path_exists(dest + "/b/c/ab_marker"));
    EXPECT_FALSE(has_mountpoint(read_mountinfo(), dest + "/b/c"));
}

TEST_F(MountObjectTopologyTest, HardlinkAliasesDoNotShareMountpointIdentity) {
    const std::string source = base_ + "/source_file";
    const std::string alias_a = base_ + "/alias_a";
    const std::string alias_b = base_ + "/alias_b";

    ASSERT_EQ(0, create_file(source, "mounted")) << strerror(errno);
    ASSERT_EQ(0, create_file(alias_a, "underlying")) << strerror(errno);
    ASSERT_EQ(0, link(alias_a.c_str(), alias_b.c_str())) << strerror(errno);

    ASSERT_EQ(0, mount(source.c_str(), alias_a.c_str(), nullptr, MS_BIND, nullptr))
        << "bind mount on regular-file alias failed: " << strerror(errno);

    EXPECT_EQ("mounted", read_file(alias_a));
    EXPECT_EQ("underlying", read_file(alias_b));
    EXPECT_EQ(-1, unlink(alias_a.c_str()));
    EXPECT_EQ(EBUSY, errno);
    EXPECT_EQ(0, unlink(alias_b.c_str())) << strerror(errno);
    EXPECT_EQ("mounted", read_file(alias_a));
    EXPECT_EQ(0, umount(alias_a.c_str())) << strerror(errno);
    EXPECT_EQ("underlying", read_file(alias_a));
}

TEST_F(MountObjectTopologyTest, BindViewRenameOverLocalMountpointIsBusy) {
    const std::string source = base_ + "/source";
    const std::string child = source + "/child";
    const std::string other = source + "/other";
    const std::string dest = base_ + "/dest";

    ASSERT_EQ(0, ensure_dir(source)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(dest)) << strerror(errno);
    ASSERT_EQ(0, mount("none", source.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(child)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(other)) << strerror(errno);
    ASSERT_EQ(0, mount(source.c_str(), dest.c_str(), nullptr, MS_BIND, nullptr))
        << strerror(errno);
    ASSERT_EQ(0, mount("none", child.c_str(), "ramfs", 0, nullptr)) << strerror(errno);

    EXPECT_EQ(-1, rename((dest + "/other").c_str(), (dest + "/child").c_str()));
    EXPECT_EQ(EBUSY, errno);
    EXPECT_TRUE(has_mountpoint(read_mountinfo(), child));
}

TEST_F(MountObjectTopologyTest, ChrootTopMountExcludesCoveredSiblingFromMountinfo) {
    const std::string stack = base_ + "/stack";
    const std::string proc = stack + "/proc";
    ASSERT_EQ(0, ensure_dir(stack)) << strerror(errno);
    ASSERT_EQ(0, mount("none", stack.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, mount("none", stack.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(proc)) << strerror(errno);
    ASSERT_EQ(0, mount("/proc", proc.c_str(), nullptr, MS_BIND, nullptr)) << strerror(errno);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (chroot(stack.c_str()) != 0 || chdir("/") != 0) {
            _exit(10);
        }
        const auto entries = read_mountinfo();
        const auto* root_entry = find_mountpoint(entries, "/");
        _exit(root_entry != nullptr && root_entry->mount_id != root_entry->parent_id &&
                      has_mountpoint(entries, "/proc")
                  ? 0
                  : 11);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}


TEST_F(MountObjectTopologyTest, ChrootOrdinaryDirectoryDoesNotSynthesizeRootMount) {
    const std::string outer = base_ + "/ordinary";
    const std::string jail = outer + "/jail";
    const std::string proc = jail + "/proc";
    ASSERT_EQ(0, ensure_dir(outer)) << strerror(errno);
    ASSERT_EQ(0, mount("none", outer.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(jail)) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(proc)) << strerror(errno);
    ASSERT_EQ(0, mount("/proc", proc.c_str(), nullptr, MS_BIND, nullptr)) << strerror(errno);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (chroot(jail.c_str()) != 0 || chdir("/") != 0) {
            _exit(20);
        }
        const auto entries = read_mountinfo();
        _exit(count_mountpoint(entries, "/") == 0 && has_mountpoint(entries, "/proc") ? 0 : 21);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
