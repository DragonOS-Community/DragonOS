#!/bin/sh
# Run inside a private mount namespace: no host /tmp or /var/tmp fixtures.
set -eu
SUITE=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TEST_SHELL=${TEST_SHELL:-/bin/sh}
exec bwrap --unshare-user --unshare-pid --unshare-net --die-with-parent \
    --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp --tmpfs /var/tmp \
    --ro-bind "$TEST_SHELL" /tmp/test-shell/sh /tmp/test-shell/sh -c '
set -eu
mkdir -p /tmp/ext4 /tmp/ram /tmp/log /tmp/bin
printf "#!/bin/sh\nexit 0\n" > /tmp/bin/hello
chmod +x /tmp/bin/hello
: > /tmp/ext4/unrelated
export LMBENCH_MANAGE_EXT4=0 LMBENCH_EXT4_DIR=/tmp/ext4
export LMBENCH_TMP_DIR=/tmp/ram LMBENCH_RUN_TMP=/tmp/log LMBENCH_BIN_DIR=/tmp/bin
export LMBENCH_ZERO_FILE=fixture_zero LMBENCH_TEST_FILE=fixture_test
"$1" "$2/runner/init.sh"
for base in /tmp/ext4 /tmp/ram; do
    [ "$(wc -c < "$base/fixture_zero")" -eq 67108864 ]
    [ "$(wc -c < "$base/fixture_test")" -eq 67108864 ]
done
[ -x /var/tmp/lmbench/hello ]
"$1" "$2/runner/clean_up.sh"
[ -f /tmp/ext4/unrelated ]
[ ! -f /tmp/ext4/fixture_zero ]
[ ! -f /tmp/ram/fixture_test ]
# Repeated cleanup must be harmless.
"$1" "$2/runner/clean_up.sh"
# A supplied filesystem can contain unrelated data with the fixture name.
printf "keep me\n" > /tmp/ext4/fixture_zero
if "$1" "$2/runner/init.sh"; then
    echo "FAIL: existing fixture was overwritten"; exit 1
fi
"$1" "$2/runner/clean_up.sh"
[ "$(cat /tmp/ext4/fixture_zero)" = "keep me" ]
echo "fixture regression: PASS"
' sh /tmp/test-shell/sh "$SUITE"
