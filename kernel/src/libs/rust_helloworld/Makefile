
rust_helloworld_objs:= 

ifneq ("$(RUSTC)", "")
	rust_helloworld_objs += helloworld.o
endif

all: $(rust_helloworld_objs)

helloworld.o: helloworld.rs
	@echo Compile helloworld.o
	@echo rustc=$(which rustc)
	$(shell $(RUSTC) --crate-type staticlib --target x86_64-unknown-none -o helloworld.o helloworld.rs)