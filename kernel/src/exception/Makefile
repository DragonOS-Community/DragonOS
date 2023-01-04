
CFLAGS += -I .


all: entry.o irq.o trap.o

entry.o: entry.S
	$(CC) -E entry.S > _entry.s
	as $(ASFLAGS) -o entry.o _entry.s

trap.o: trap.c
	$(CC) $(CFLAGS) -c trap.c -o trap.o

irq.o: irq.c
	$(CC) $(CFLAGS) -c irq.c -o irq.o