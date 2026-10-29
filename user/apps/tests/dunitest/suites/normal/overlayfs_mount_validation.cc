#include <gtest/gtest.h>

#include <dirent.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <string>
#include <utility>
#include <vector>
#include <unistd.h>

#ifndef MS_BIND
#define MS_BIND 4096
#endif

namespace {

int ensure_dir(const char* path) {
    struct stat st = {};
    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path, 0755);
}

std::string join_path(const std::string& dir, const char* name) {
    return dir + "/" + name;
}

int write_text(const std::string& path, const char* text) {
    FILE* file = fopen(path.c_str(), "w");
    if (file == nullptr) {
        return -1;
    }
    int ret = fputs(text, file);
    int saved_errno = errno;
    fclose(file);
    errno = saved_errno;
    return ret < 0 ? -1 : 0;
}

std::string read_text(const std::string& path) {
    char buf[128] = {};
    FILE* file = fopen(path.c_str(), "r");
    if (file == nullptr) {
        return {};
    }
    size_t n = fread(buf, 1, sizeof(buf) - 1, file);
    int saved_errno = errno;
    fclose(file);
    errno = saved_errno;
    return std::string(buf, n);
}

void remove_recursive(const std::string& path) {
    struct stat st = {};
    if (lstat(path.c_str(), &st) != 0) {
        return;
    }
    if (!S_ISDIR(st.st_mode)) {
        unlink(path.c_str());
        return;
    }

    DIR* dir = opendir(path.c_str());
    if (dir != nullptr) {
        while (dirent* ent = readdir(dir)) {
            if (strcmp(ent->d_name, ".") == 0 || strcmp(ent->d_name, "..") == 0) {
                continue;
            }
            remove_recursive(join_path(path, ent->d_name));
        }
        closedir(dir);
    }
    rmdir(path.c_str());
}

int count_entries(const std::string& path) {
    DIR* dir = opendir(path.c_str());
    if (dir == nullptr) {
        return -1;
    }

    int count = 0;
    while (dirent* ent = readdir(dir)) {
        if (strcmp(ent->d_name, ".") == 0 || strcmp(ent->d_name, "..") == 0) {
            continue;
        }
        count++;
    }
    closedir(dir);
    return count;
}

struct OverlayMountValidationEnv {
    explicit OverlayMountValidationEnv(const char* name) {
        root = std::string("/tmp/") + name + "_" + std::to_string(getpid());
        upper = join_path(root, "u");
        upper_sibling = join_path(root, "u2");
        upper_child_work = join_path(upper, "w");
        work = join_path(root, "w");
        upper_in_work = join_path(work, "u");
        lower1 = join_path(root, "l1");
        lower1_child = join_path(lower1, "child");
        lower2 = join_path(root, "l2");
        merged = join_path(root, "m");
        lower_file = join_path(root, "lower_file");
    }

    ~OverlayMountValidationEnv() {
        cleanup_mount();
        cleanup_bind_mounts();
        remove_recursive(join_path(root, "u"));
        remove_recursive(join_path(root, "u2"));
        remove_recursive(join_path(root, "w"));
        remove_recursive(join_path(root, "l1"));
        remove_recursive(join_path(root, "l2"));
        unlink(lower_file.c_str());
        rmdir(merged.c_str());
        rmdir(root.c_str());
    }

    void prepare() {
        ASSERT_EQ(0, ensure_dir("/tmp")) << strerror(errno);
        ASSERT_EQ(0, ensure_dir(root.c_str())) << strerror(errno);
        ASSERT_EQ(0, ensure_dir(upper.c_str())) << strerror(errno);
        ASSERT_EQ(0, ensure_dir(upper_sibling.c_str())) << strerror(errno);
        ASSERT_EQ(0, ensure_dir(upper_child_work.c_str())) << strerror(errno);
        ASSERT_EQ(0, ensure_dir(work.c_str())) << strerror(errno);
        ASSERT_EQ(0, ensure_dir(upper_in_work.c_str())) << strerror(errno);
        ASSERT_EQ(0, ensure_dir(lower1.c_str())) << strerror(errno);
        ASSERT_EQ(0, ensure_dir(lower1_child.c_str())) << strerror(errno);
        ASSERT_EQ(0, ensure_dir(lower2.c_str())) << strerror(errno);
        ASSERT_EQ(0, ensure_dir(merged.c_str())) << strerror(errno);
        ASSERT_EQ(0, write_text(lower_file, "not a directory")) << strerror(errno);
    }

    std::string options(const std::string& lower, const std::string& upper_dir,
                        const std::string& work_dir) const {
        return "lowerdir=" + lower + ",upperdir=" + upper_dir + ",workdir=" + work_dir;
    }

    std::string repeated_lowerdirs(const std::string& dir, int count) const {
        std::string lower = dir;
        for (int i = 1; i < count; ++i) {
            lower += ":" + dir;
        }
        return options(lower, upper, work);
    }

    void bind_mount(const std::string& source, const std::string& target) {
        ASSERT_EQ(0, mount(source.c_str(), target.c_str(), nullptr, MS_BIND, nullptr))
            << strerror(errno);
        bind_mounts.push_back(target);
    }

    void cleanup_mount() {
        if (!mounted) {
            return;
        }
        if (umount(merged.c_str()) != 0) {
            umount2(merged.c_str(), MNT_DETACH);
        }
        mounted = false;
    }

    void cleanup_bind_mounts() {
        for (auto it = bind_mounts.rbegin(); it != bind_mounts.rend(); ++it) {
            if (umount(it->c_str()) != 0) {
                umount2(it->c_str(), MNT_DETACH);
            }
        }
        bind_mounts.clear();
    }

    void expect_mount_errno(const char* data, int expected_errno) {
        std::vector<std::pair<std::string, int>> before = {
            {upper, count_entries(upper)}, {work, count_entries(work)},
            {lower1, count_entries(lower1)}, {lower2, count_entries(lower2)},
            {merged, count_entries(merged)},
        };

        errno = 0;
        int ret = mount("overlay", merged.c_str(), "overlay", 0, data);
        int saved_errno = errno;
        if (ret == 0) {
            mounted = true;
            cleanup_mount();
            FAIL() << "mount unexpectedly succeeded with data="
                   << (data == nullptr ? "<null>" : data);
        }
        EXPECT_EQ(expected_errno, saved_errno)
            << "unexpected errno for data=" << (data == nullptr ? "<null>" : data) << ": "
            << strerror(saved_errno);

        for (const auto& entry : before) {
            EXPECT_EQ(entry.second, count_entries(entry.first))
                << "invalid mount changed " << entry.first;
        }
    }

    void expect_mount_errno(const std::string& data, int expected_errno) {
        expect_mount_errno(data.c_str(), expected_errno);
    }

    void expect_mount_ok(const std::string& data) {
        ASSERT_FALSE(mounted);
        ASSERT_EQ(0, mount("overlay", merged.c_str(), "overlay", 0, data.c_str()))
            << strerror(errno);
        mounted = true;
    }

    void expect_umount_ok() {
        ASSERT_TRUE(mounted);
        ASSERT_EQ(0, umount(merged.c_str())) << strerror(errno);
        mounted = false;
    }

    std::string root;
    std::string upper;
    std::string upper_sibling;
    std::string upper_child_work;
    std::string work;
    std::string upper_in_work;
    std::string lower1;
    std::string lower1_child;
    std::string lower2;
    std::string merged;
    std::string lower_file;
    std::vector<std::string> bind_mounts;
    bool mounted = false;
};

}  // namespace

TEST(OverlayFsMountValidation, RejectsMissingAndEmptyOptions) {
    OverlayMountValidationEnv env("overlayfs_mount_missing");
    env.prepare();

    env.expect_mount_errno(nullptr, EINVAL);
    env.expect_mount_errno("", EINVAL);
    env.expect_mount_errno("lowerdir=/tmp,workdir=/tmp", EINVAL);
    env.expect_mount_errno("lowerdir=/tmp,upperdir=/tmp", EINVAL);
    env.expect_mount_errno("upperdir=/tmp,workdir=/tmp", EINVAL);
    env.expect_mount_errno(env.options(env.lower1, "", env.work), EINVAL);
    env.expect_mount_errno(env.options("", env.upper, env.work), EINVAL);
    env.expect_mount_errno(env.options(env.lower1, env.upper, ""), EINVAL);
}

TEST(OverlayFsMountValidation, RejectsInvalidLowerdirComponentsAndTypes) {
    OverlayMountValidationEnv env("overlayfs_mount_lower");
    env.prepare();

    env.expect_mount_errno(env.options(env.lower1 + ":", env.upper, env.work), EINVAL);
    env.expect_mount_errno(env.options(":" + env.lower1, env.upper, env.work), EINVAL);
    env.expect_mount_errno(env.options(env.lower1 + "::" + env.lower2, env.upper, env.work),
                           EINVAL);
    env.expect_mount_errno(env.repeated_lowerdirs("/", 501), EINVAL);
    env.expect_mount_errno(env.options(env.lower_file, env.upper, env.work), EINVAL);
}

TEST(OverlayFsMountValidation, RejectsUpperWorkOverlap) {
    OverlayMountValidationEnv env("overlayfs_mount_overlap");
    env.prepare();

    env.expect_mount_errno(env.options(env.lower1, env.upper, env.upper), EINVAL);
    env.expect_mount_errno(env.options(env.lower1, env.upper, env.upper + "/."), EINVAL);
    env.expect_mount_errno(env.options(env.lower1, env.upper, env.upper_child_work), EINVAL);
    env.expect_mount_errno(env.options(env.lower1, env.upper_in_work, env.work), EINVAL);
}

TEST(OverlayFsMountValidation, RejectsLowerUpperWorkOverlap) {
    OverlayMountValidationEnv env("overlayfs_mount_lower_overlap");
    env.prepare();

    env.expect_mount_errno(env.options(env.upper, env.upper, env.work), ELOOP);
    env.expect_mount_errno(env.options(env.work, env.upper, env.work), ELOOP);
    env.expect_mount_errno(env.options(env.upper_child_work, env.upper, env.work), ELOOP);
    env.expect_mount_errno(env.options(env.upper_in_work, env.upper, env.work), ELOOP);
}

TEST(OverlayFsMountValidation, RejectsLowerLayerOverlap) {
    OverlayMountValidationEnv env("overlayfs_mount_lower_layer_overlap");
    env.prepare();

    env.expect_mount_errno(env.options(env.lower1 + ":" + env.lower1, env.upper, env.work),
                           ELOOP);
    env.expect_mount_errno(env.options(env.lower1 + ":" + env.lower1_child, env.upper, env.work),
                           ELOOP);
    env.expect_mount_errno(env.options(env.lower1_child + ":" + env.lower1, env.upper, env.work),
                           ELOOP);
}

TEST(OverlayFsMountValidation, RejectsBindMountedLowerAlias) {
    OverlayMountValidationEnv env("overlayfs_mount_bind_lower_alias");
    env.prepare();

    ASSERT_EQ(0, ensure_dir(join_path(env.lower1, "upper").c_str())) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(join_path(env.lower1, "work").c_str())) << strerror(errno);
    env.bind_mount(env.lower1, env.lower2);
    env.expect_mount_errno(
        env.options(env.lower1, join_path(env.lower1, "upper"), join_path(env.lower2, "work")),
        EINVAL);
    env.expect_mount_errno(env.options(env.lower1 + ":" + env.lower2, env.upper, env.work),
                           ELOOP);
}

TEST(OverlayFsMountValidation, AcceptsValidSingleAndMultipleLowerdirs) {
    OverlayMountValidationEnv env("overlayfs_mount_valid");
    env.prepare();

    std::string sibling_options = env.options(env.lower1, env.upper, env.upper_sibling);
    env.expect_mount_ok(sibling_options);
    env.expect_umount_ok();

    ASSERT_EQ(0, ensure_dir(join_path(env.lower1, "upper").c_str())) << strerror(errno);
    ASSERT_EQ(0, ensure_dir(join_path(env.lower1, "work").c_str())) << strerror(errno);
    std::string lower_parent_options =
        env.options(env.lower1, join_path(env.lower1, "upper"), join_path(env.lower1, "work"));
    env.expect_mount_ok(lower_parent_options);
    env.expect_umount_ok();

    ASSERT_EQ(0, write_text(join_path(env.lower1, "shared"), "lower1")) << strerror(errno);
    ASSERT_EQ(0, write_text(join_path(env.lower2, "shared"), "lower2")) << strerror(errno);
    ASSERT_EQ(0, write_text(join_path(env.lower2, "only2"), "only2")) << strerror(errno);

    std::string multi_options =
        env.options(env.lower1 + ":" + env.lower2, env.upper, env.work);
    env.expect_mount_ok(multi_options);
    EXPECT_EQ("lower1", read_text(join_path(env.merged, "shared")));
    EXPECT_EQ("only2", read_text(join_path(env.merged, "only2")));
    env.expect_umount_ok();
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
