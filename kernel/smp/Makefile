
CFLAGS += -I .


all: apu_boot.o smp.o


apu_boot.o: apu_boot.S
	$(CC) -E apu_boot.S > _apu_boot.s # 预处理
	as $(ASFLAGS) -o apu_boot.o _apu_boot.s

smp.o: smp.c
	$(CC) $(CFLAGS) -c smp.c -o smp.o