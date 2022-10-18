
CFLAGS += -I .


all: sched.o cfs.o

cfs.o: cfs.c
	$(CC) $(CFLAGS) -c cfs.c -o cfs.o

sched.o: sched.c
	$(CC) $(CFLAGS) -c sched.c -o sched.o
clean:
	echo "Done."