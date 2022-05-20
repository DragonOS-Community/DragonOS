
all: procs.o process.o wait_queue.o

CFLAGS += -I .



procs.o: proc.S
	gcc -E proc.S > proc.s
	as $(ASFLAGS) -o procs.o proc.s

process.o: process.c
	gcc $(CFLAGS) -c process.c -o process.o

wait_queue.o: wait_queue.c
	gcc $(CFLAGS) -c wait_queue.c -o wait_queue.o
