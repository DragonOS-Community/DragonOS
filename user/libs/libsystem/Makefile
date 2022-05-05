all: libsystem
# echo $(shell pwd)


libsystem: syscall.o

syscall.o: syscall.c
	gcc $(CFLAGS) -c syscall.c  -o syscall.o
