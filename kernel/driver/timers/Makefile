
all: rtc.o HPET.o

CFLAGS += -I .

rtc.o: rtc/rtc.c
	$(CC) $(CFLAGS) -c rtc/rtc.c -o rtc/rtc.o

HPET.o: HPET/HPET.c
	$(CC) $(CFLAGS) -c HPET/HPET.c -o HPET/HPET.o
