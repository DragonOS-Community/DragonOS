#include <gtest/gtest.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>

namespace {
    
const char* control_path = "/sys/kernel/sysfs_group_test";
const char* register_path = "/sys/kernel/sysfs_group_test/register";
const char* unregister_path = "/sys/kernel/sysfs_group_test/unregister";
const char* device_path = "/sys/devices/sysfs_group_test";
const char* group1_path = "/sys/devices/sysfs_group_test/test_group1";
const char* group2_path = "/sys/devices/sysfs_group_test/test_group2";
    
bool file_exists(const char *path) {
    struct stat st;
    return stat(path, &st) == 0;
}

int write_file(const char *path, const char *buf, size_t size) {
    int fd = open(path, O_WRONLY);
    if (fd < 0) return -1;
    ssize_t n = write(fd, buf, size);
    close(fd);
    return n == (ssize_t)size ? 0 : -1;
}

void ensure_device_unregistered(void) {
    if (file_exists(device_path)) {
        write_file(unregister_path, "1\n", 2);
        usleep(10000);
    }
    if (file_exists(device_path)) {
        FAIL() << "Fail to remove device";
    }
}

} // namespace

TEST(SysfsGroupTest, CreateGroups) {
    if (!file_exists(control_path)) {
        GTEST_SKIP() << "sysfs_group_test kernel module not loaded";
    }
    ensure_device_unregistered();    

    const char* group1_path_status = "/sys/devices/sysfs_group_test/test_group1/status";
    const char* group2_path_status = "/sys/devices/sysfs_group_test/test_group2/status";

    EXPECT_TRUE(file_exists(control_path)) << "Control interface not exists";
    EXPECT_FALSE(file_exists(device_path)) << "Device registered initially as unexpected";

    EXPECT_EQ(0, write_file(register_path, "1\n", 2)) << "Register device fails";
    EXPECT_TRUE(file_exists(device_path)) << "Device directory not created";
    EXPECT_TRUE(file_exists(group1_path)) << "group1 not created";
    EXPECT_TRUE(file_exists(group2_path)) << "group2 not created";
    EXPECT_TRUE(file_exists(group1_path_status)) << "group1 attributes not created";
    EXPECT_TRUE(file_exists(group2_path_status)) << "group2 attributes not created";

    ensure_device_unregistered();
}

TEST(SysfsGroupTest, RemoveGroups) {
    if (!file_exists(control_path)) {
        GTEST_SKIP() << "sysfs_group_test kernel module not loaded";
    }
    ensure_device_unregistered();    

    EXPECT_EQ(0, write_file(register_path, "1\n", 2)) << "Register device fail";
    EXPECT_EQ(0, write_file(unregister_path, "1\n", 2)) << "Unregister device fail";
    EXPECT_FALSE(file_exists(device_path)) << "Device directory not removed";
    EXPECT_FALSE(file_exists(group1_path)) << "group1 not removed";
    EXPECT_FALSE(file_exists(group2_path)) << "group2 not removed";

    ensure_device_unregistered();
}

TEST(SysfsGroupTest, Lifecycle) {
    if (!file_exists(control_path)) {
        GTEST_SKIP() << "sysfs_group_test kernel module not loaded";
    }
    ensure_device_unregistered();    
    
    for (int i = 0; i < 3; i++) {
        EXPECT_EQ(0, write_file(register_path, "1\n", 2)) << "Register device fails" << i;
        EXPECT_TRUE(file_exists(device_path)) << "Device not exists" << i;
        EXPECT_EQ(0, write_file(unregister_path, "1\n", 2)) << "Unregister device fails" << i;
        EXPECT_FALSE(file_exists(device_path)) << "Device not removed" << i;
    }

    ensure_device_unregistered();
}

TEST(SysfsGroupTest, FailureRollback) {
    if (!file_exists(control_path)) {
        GTEST_SKIP() << "sysfs_group_test kernel module not loaded";
    }
    ensure_device_unregistered();    
    
    const char* set_fail_path = "/sys/kernel/sysfs_group_test/fail_on_create";

    EXPECT_EQ(0, write_file(set_fail_path, "1\n", 2)) << "Set fail flag fails";
    EXPECT_FALSE(file_exists(group1_path)) << "Group1 created as unexpected";

    ensure_device_unregistered();
    EXPECT_EQ(0, write_file(set_fail_path, "0\n", 2)) << "Clear fail flag fails";
}

TEST(SysfsGroupTest, ErrorHandling) {
    if (!file_exists(control_path)) {
        GTEST_SKIP() << "sysfs_group_test kernel module not loaded";
    }
    ensure_device_unregistered();    
    
    EXPECT_EQ(0, write_file(register_path, "1\n", 2)) << "First registration fails";
    EXPECT_NE(0, write_file(register_path, "1\n", 2)) << "Double registration success as unexpected";
    EXPECT_TRUE(file_exists(device_path)) << "Device not exists after error";

    EXPECT_EQ(0, write_file(unregister_path, "1\n", 2)) << "First unregister fails";
    EXPECT_EQ(0, write_file(unregister_path, "1\n", 2)) << "Double unregister fails";
    EXPECT_FALSE(file_exists(device_path)) << "Device not removed";

    ensure_device_unregistered();
}

int main(int argc, char **argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
