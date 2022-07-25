
all: rtc.o HPET.o

CFLAGS += -I .

rtc.o: rtc/rtc.c
	gcc $(CFLAGS) -c rtc/rtc.c -o rtc/rtc.o

HPET.o: HPET/HPET.c
	gcc $(CFLAGS) -c HPET/HPET.c -o HPET/HPET.o
