#!/bin/busybox sh

cd $SYSCALL_TEST_DIR
./gvisor-test-runner --stdout
