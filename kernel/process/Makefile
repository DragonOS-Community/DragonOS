
all: procs.o process.o

CFLAGS += -I .



procs.o: proc.S
	gcc -E proc.S > _proc.s
	as $(ASFLAGS) -o procs.o _proc.s

process.o: process.c
	gcc $(CFLAGS) -c process.c -o process.o



clean:
	echo "Done."