
CFLAGS += -I .

all: fat32.o MBR.o VFS.o fat_ent.o


fat32.o: fat32/fat32.c
	gcc $(CFLAGS) -c fat32/fat32.c -o fat32/fat32.o

MBR.o: MBR.c
	gcc $(CFLAGS) -c MBR.c -o MBR.o

VFS.o: VFS/VFS.c
	gcc $(CFLAGS) -c VFS/VFS.c -o VFS/VFS.o

fat_ent.o: fat32/fat_ent.c
	gcc $(CFLAGS) -c fat32/fat_ent.c -o fat32/fat_ent.o

clean:
	echo "Done."