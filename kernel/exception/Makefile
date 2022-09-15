
CFLAGS += -I .


all: entry.o irq.o softirq.o trap.o

entry.o: entry.S
	gcc -E entry.S > _entry.s
	as $(ASFLAGS) -o entry.o _entry.s

trap.o: trap.c
	gcc $(CFLAGS) -c trap.c -o trap.o

softirq.o: softirq.c
	gcc $(CFLAGS) -c softirq.c -o softirq.o

irq.o: irq.c
	gcc $(CFLAGS) -c irq.c -o irq.o