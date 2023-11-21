
CFLAGS += -I .


all: irq.o trap.o

trap.o: trap.c
	$(CC) $(CFLAGS) -c trap.c -o trap.o

irq.o: irq.c
	$(CC) $(CFLAGS) -c irq.c -o irq.o