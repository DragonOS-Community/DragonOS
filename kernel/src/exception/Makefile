
CFLAGS += -I .


all: entry.o irq.o softirq.o trap.o

entry.o: entry.S
	$(CC) -E entry.S > _entry.s
	as $(ASFLAGS) -o entry.o _entry.s

trap.o: trap.c
	$(CC) $(CFLAGS) -c trap.c -o trap.o

softirq.o: softirq.c
	$(CC) $(CFLAGS) -c softirq.c -o softirq.o

irq.o: irq.c
	$(CC) $(CFLAGS) -c irq.c -o irq.o