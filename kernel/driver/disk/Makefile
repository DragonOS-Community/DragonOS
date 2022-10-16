
all: ata.o ahci.o

CFLAGS += -I .

ata.o: ata.c
	$(CC) $(CFLAGS) -c ata.c -o ata.o

ahci.o: ahci/ahci.c
	$(CC) $(CFLAGS) -c ahci/ahci.c -o ahci/ahci.o