SRC = $(wildcard *.c)
OBJ = $(SRC:.c=.o)
CFLAGS += -I .

.PHONY: all

all: $(OBJ)

%.o: %.c
	$(CC) $(CFLAGS) -c $< -o $@
