
CFLAGS += -I .


kernel_process_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"


$(kernel_process_objs): ECHO
	gcc $(CFLAGS) -c $@ -o $@.o

procs.o: proc.S
	gcc -E proc.S > _proc.s
	as $(ASFLAGS) -o procs.o _proc.s

all: procs.o $(kernel_process_objs)


clean:
	echo "Done."