
all: pci.o  pci_irq.o msi.o

CFLAGS += -I .

pci.o: pci.c
	$(CC) $(CFLAGS) -c pci.c -o pci.o
	
pci_irq.o: pci_irq.c
	$(CC) $(CFLAGS) -c pci_irq.c -o pci_irq.o
	
msi.o: msi.c
	$(CC) $(CFLAGS) -c msi.c -o msi.o
	
