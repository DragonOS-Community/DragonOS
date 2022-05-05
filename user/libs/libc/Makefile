all: libc

CFLAGS += -I .


libc: unistd.o fcntl.o

unistd.o: unistd.c
	gcc $(CFLAGS) -c unistd.c -o unistd.o

fcntl.o: fcntl.c
	gcc $(CFLAGS) -c fcntl.c -o fcntl.o
