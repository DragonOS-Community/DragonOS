
CFLAGS += -I .


all:mm.o slab.o mm-stat.o

mm.o: mm.c
	gcc $(CFLAGS) -c mm.c -o mm.o

slab.o: slab.c
	gcc $(CFLAGS) -c slab.c -o slab.o

mm-stat.o: mm-stat.c
	gcc $(CFLAGS) -c mm-stat.c -o mm-stat.o
