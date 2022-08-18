
CFLAGS += -I .


all:mm.o slab.o mm-stat.o vma.o mmap.o utils.o mmio.o mmio-buddy.o

mm.o: mm.c
	gcc $(CFLAGS) -c mm.c -o mm.o

slab.o: slab.c
	gcc $(CFLAGS) -c slab.c -o slab.o

mm-stat.o: mm-stat.c
	gcc $(CFLAGS) -c mm-stat.c -o mm-stat.o

vma.o: vma.c
	gcc $(CFLAGS) -c vma.c -o vma.o

mmap.o: mmap.c
	gcc $(CFLAGS) -c mmap.c -o mmap.o

utils.o: utils.c
	gcc $(CFLAGS) -c utils.c -o utils.o

mmio.o: mmio.c
	gcc $(CFLAGS) -c mmio.c -o mmio.o

mmio-buddy.o: mmio-buddy.c
	gcc $(CFLAGS) -c mmio-buddy.c -o mmio-buddy.o