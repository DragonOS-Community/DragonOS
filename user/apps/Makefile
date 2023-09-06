rust_apps_dirs := test_drsstd
user_apps_sub_dirs=shell about test_signal

ECHO:
	@echo "$@"

$(rust_apps_dirs): ECHO
	$(MAKE) -C $@ fmt

$(user_apps_sub_dirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" tmp_output_dir="$(tmp_output_dir)" output_dir="$(output_dir)" sys_libs_dir="$(sys_libs_dir)"

all: $(user_apps_sub_dirs)

.PHONY: clean
fmt: $(rust_apps_dirs)
	@echo "格式化代码: user/apps"
