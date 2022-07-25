
all: ata.o ahci.o

CFLAGS += -I .

ata.o: ata.c
	gcc $(CFLAGS) -c ata.c -o ata.o

ahci.o: ahci/ahci.c
	gcc $(CFLAGS) -c ahci/ahci.c -o ahci/ahci.o