#include <sys/types.h>
#include <unistd.h>
#include <stdio.h>
#include <assert.h>

int main()
{
    printf("Current uid: %d, euid: %d, gid: %d, egid: %d\n\n", getuid(), geteuid(), getgid(), getegid());

    // 测试uid
    printf("Set uid 1000\n");
    setuid(1000);
    int uid = getuid();
    assert(uid == 1000);
    printf("Current uid:%d\n\n", uid);

    // 测试gid
    printf("Set gid 1000\n");
    setgid(1000);
    int gid = getgid();
    assert(gid == 1000);
    printf("Current gid:%d\n\n", gid);

    // 测试euid
    printf("Setg euid 1000\n");
    seteuid(1000);
    int euid = geteuid();
    assert(euid == 1000);
    printf("Current euid:%d\n\n", euid);

    // 测试egid
    printf("Set egid 1000\n");
    setegid(1000);
    int egid = getegid();
    assert(egid == 1000);
    printf("Current egid:%d\n\n", egid);

    // 测试uid在非root用户下无法修改
    printf("Try to setuid for non_root.\n");
    assert(setuid(0) < 0); // 非root用户无法修改uid
    printf("Current uid: %d, euid: %d, gid: %d, egid: %d\n", getuid(), geteuid(), getgid(), getegid());
}