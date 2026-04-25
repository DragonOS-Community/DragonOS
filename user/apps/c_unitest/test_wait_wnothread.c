#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv_child_ready = PTHREAD_COND_INITIALIZER;
static pthread_cond_t cv_main_done = PTHREAD_COND_INITIALIZER;

static pid_t g_child_pid = 0;
static int g_main_done = 0;

static void *forker_thread(void *arg) {
  (void)arg;

  pid_t pid = fork();
  if (pid < 0) {
    perror("fork");
    _exit(2);
  }
  if (pid == 0) {
    sleep(3);
    _exit(0);
  }

  pthread_mutex_lock(&mu);
  g_child_pid = pid;
  pthread_cond_broadcast(&cv_child_ready);
  while (!g_main_done) {
    pthread_cond_wait(&cv_main_done, &mu);
  }
  pthread_mutex_unlock(&mu);

  int status = 0;
  pid_t got = wait4(-1, &status, __WNOTHREAD, NULL);
  if (got < 0) {
    perror("wait4(__WNOTHREAD) in forker thread");
    _exit(3);
  }
  if (got != pid) {
    fprintf(stderr, "wait4 returned %d, expected %d\n", got, pid);
    _exit(4);
  }
  if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
    fprintf(stderr, "child status unexpected: 0x%x\n", status);
    _exit(5);
  }

  return NULL;
}

int main(void) {
  pthread_t th;
  if (pthread_create(&th, NULL, forker_thread, NULL) != 0) {
    perror("pthread_create");
    return 1;
  }

  pthread_mutex_lock(&mu);
  while (g_child_pid == 0) {
    pthread_cond_wait(&cv_child_ready, &mu);
  }
  pthread_mutex_unlock(&mu);

  alarm(5);
  int status = 0;
  pid_t got = wait4(-1, &status, __WNOTHREAD, NULL);
  if (got != -1 || errno != ECHILD) {
    fprintf(stderr, "main wait4 expected -1/ECHILD, got=%d errno=%d\n", got,
            errno);
    return 2;
  }

  pthread_mutex_lock(&mu);
  g_main_done = 1;
  pthread_cond_broadcast(&cv_main_done);
  pthread_mutex_unlock(&mu);

  if (pthread_join(th, NULL) != 0) {
    perror("pthread_join");
    return 3;
  }

  printf("test_wait_wnothread: PASS\n");
  return 0;
}

