all: libsystem
# echo $(shell pwd)


libsystem: syscall.o

syscall.o: syscall.c
	$(CC) $(CFLAGS) -c syscall.c  -o syscall.o
