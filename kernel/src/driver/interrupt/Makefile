
all: pic.o

# 中断处理芯片的驱动程序
ifeq ($(PIC), _INTR_8259A_)
pic.o: 8259A/8259A.c
	$(CC) $(CFLAGS) -c 8259A/8259A.c -o pic.o
else
pic.o: apic/apic.c apic_timer.o
	$(CC) $(CFLAGS) -c apic/apic.c -o pic.o

apic_timer.o: apic/apic_timer.c
	$(CC) $(CFLAGS) -c apic/apic_timer.c -o apic/apic_timer.o
endif