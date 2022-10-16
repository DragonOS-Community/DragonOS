CFLAGS += -I .

all: x86_64_ipi.o ia64_msi.o

x86_64_ipi.o: x86_64_ipi.c
	$(CC) $(CFLAGS) -c x86_64_ipi.c -o x86_64_ipi.o

ia64_msi.o: ia64_msi.c
	$(CC) $(CFLAGS) -c ia64_msi.c -o ia64_msi.o

