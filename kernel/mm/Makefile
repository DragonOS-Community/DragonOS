
CFLAGS += -I .


all:mm.o slab.o mm-stat.o vma.o mmap.o

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
