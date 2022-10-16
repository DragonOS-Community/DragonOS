
CFLAGS += -I .


all: sched.o cfs.o

sched.o: sched.c
	gcc $(CFLAGS) -c sched.c -o sched.o

cfs.o: cfs.c
	gcc $(CFLAGS) -c cfs.c -o cfs.o

clean:
	echo "Done."