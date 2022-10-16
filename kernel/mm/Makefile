
CFLAGS += -I .


all:mm.o slab.o mm-stat.o vma.o mmap.o utils.o mmio.o mmio-buddy.o

mm.o: mm.c
	$(CC) $(CFLAGS) -c mm.c -o mm.o

slab.o: slab.c
	$(CC) $(CFLAGS) -c slab.c -o slab.o

mm-stat.o: mm-stat.c
	$(CC) $(CFLAGS) -c mm-stat.c -o mm-stat.o

vma.o: vma.c
	$(CC) $(CFLAGS) -c vma.c -o vma.o

mmap.o: mmap.c
	$(CC) $(CFLAGS) -c mmap.c -o mmap.o

utils.o: utils.c
	$(CC) $(CFLAGS) -c utils.c -o utils.o

mmio.o: mmio.c
	$(CC) $(CFLAGS) -c mmio.c -o mmio.o

mmio-buddy.o: mmio-buddy.c
	$(CC) $(CFLAGS) -c mmio-buddy.c -o mmio-buddy.o