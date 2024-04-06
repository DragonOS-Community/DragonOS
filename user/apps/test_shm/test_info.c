#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/shm.h>
#include <sys/ipc.h>
#include <string.h>
#include <sys/wait.h>

void print_shmidds(struct shmid_ds *shminfo)
{
    struct ipc_perm shm_perm = shminfo->shm_perm;

    printf("ipc_perm:\n");
    printf("ipc_perm_key: %d\n", shm_perm.__key);
    printf("uid: %d\n", shm_perm.uid);
    printf("gid: %d\n", shm_perm.gid);
    printf("cuid: %d\n", shm_perm.cuid);
    printf("cgid: %d\n", shm_perm.cgid);
    printf("mode: %d\n", shm_perm.mode);
    printf("seq: %d\n", shm_perm.__seq);
    printf("\n");

    printf("shmid_ds:\n");
    printf("shm_atime: %lu\n", shminfo->shm_atime);
    printf("shm_dtime: %lu\n", shminfo->shm_dtime);
    printf("shm_ctime: %lu\n", shminfo->shm_ctime);
    printf("shm_segsz: %lu\n", shminfo->shm_segsz);
    printf("shm_cpid: %d\n", shminfo->shm_cpid);
    printf("shm_lpid: %d\n", shminfo->shm_lpid);
    printf("shm_nattch: %lu\n", shminfo->shm_nattch);
    printf("\n");
}

const int SHM_SIZE = 9999;

int main()
{
    int shmid;
    char *shmaddr;
    key_t key = 6666;

    // 测试shmget
    shmid = shmget(key, SHM_SIZE, 0666 | IPC_CREAT);
    if (shmid < 0)
    {
        perror("shmget failed");
        exit(EXIT_FAILURE);
    }

    // 测试shmat
    shmaddr = shmat(shmid, 0, 0);

    // IPC_STAT:
    printf("\n");
    printf("IPC_STAT:\n");
    struct shmid_ds shminfo2;
    if (shmctl(shmid, IPC_STAT, &shminfo2) == -1)
    {
        // 获取共享内存段信息
        perror("shmctl");
        exit(EXIT_FAILURE);
    }
    print_shmidds(&shminfo2);

    // 测试shmctl
    // IPC_INFO
    printf("IPC_INFO:\n");
    struct shminfo shmmetainfo;
    if (shmctl(shmid, IPC_INFO, &shmmetainfo) == -1)
    { // 获取共享内存段信息
        perror("shmctl");
        exit(EXIT_FAILURE);
    }
    printf("shmmax: %lu\n", shmmetainfo.shmmax);
    printf("shmmin: %lu\n", shmmetainfo.shmmin);
    printf("shmmni: %lu\n", shmmetainfo.shmmni);
    printf("shmseg: %lu\n", shmmetainfo.shmseg);
    printf("shmall: %lu\n", shmmetainfo.shmall);

    // SHM_INFO
    printf("\n");
    printf("SHM_INFO:\n");
    struct shm_info shm_info;
    if (shmctl(shmid, SHM_INFO, &shm_info) == -1)
    { // 获取共享内存段信息
        perror("shmctl");
        exit(EXIT_FAILURE);
    }
    printf("__used_ids: %lu\n", shm_info.__used_ids);
    printf("shm_tot: %lu\n", shm_info.shm_tot);
    printf("shm_rss: %lu\n", shm_info.shm_rss);
    printf("shm_swp: %lu\n", shm_info.shm_swp);
    printf("__swap_attempts: %lu\n", shm_info.__swap_attempts);
    printf("__swap_successes: %lu\n", shm_info.__swap_successes);

    // SHM_STAT
    printf("\n");
    printf("SHM_STAT:\n");
    struct shmid_ds shminfo0;
    if (shmctl(shmid, SHM_STAT, &shminfo0) == -1)
    { // 获取共享内存段信息
        perror("shmctl");
        exit(EXIT_FAILURE);
    }
    print_shmidds(&shminfo0);

    // SHM_STAT_ANY
    printf("SHM_STAT_ANY:\n");
    struct shmid_ds shminfo1;
    if (shmctl(shmid, SHM_STAT_ANY, &shminfo1) == -1)
    { // 获取共享内存段信息
        perror("shmctl");
        exit(EXIT_FAILURE);
    }
    print_shmidds(&shminfo1);

    // IPC_SET
    printf("\n");
    printf("IPC_SET:\n");
    struct shmid_ds shminfo;
    shminfo.shm_atime = 1;
    shminfo.shm_dtime = 2;
    shminfo.shm_ctime = 3;
    shminfo.shm_segsz = 4;
    shminfo.shm_cpid = 5;
    shminfo.shm_lpid = 6;
    shminfo.shm_nattch = 7;
    if (shmctl(shmid, IPC_SET, &shminfo) == -1)
    { // 获取共享内存段信息
        perror("shmctl");
        exit(EXIT_FAILURE);
    }

    // IPC_RMID
    printf("\n");
    printf("IPC_RMID:\n");
    if (shmctl(shmid, IPC_RMID, NULL) == -1)
    { // 获取共享内存段信息
        perror("shmctl");
        exit(EXIT_FAILURE);
    }

    // SHM_LOCK
    printf("\n");
    printf("SHM_LOCK:\n");
    if (shmctl(shmid, SHM_LOCK, NULL) == -1)
    { // 获取共享内存段信息
        perror("shmctl");
        exit(EXIT_FAILURE);
    }

    // SHM_UNLOCK
    printf("\n");
    printf("SHM_UNLOCK:\n");
    if (shmctl(shmid, SHM_UNLOCK, NULL) == -1)
    { // 获取共享内存段信息
        perror("shmctl");
        exit(EXIT_FAILURE);
    }
}