REGET=${REGET:-0}

RV_TEST="../bin/riscv64/sdcard-rv.img"
RV_TEST_URL="https://github.com/Samuka007/testsuits-for-oskernel/releases/download/pre-2025-03-21/sdcard-rv.img.gz"
RV_TEST_DIR="../bin/riscv64"

if [ ! -f "$RV_TEST" ] || [ "$REGET" -eq 1 ]; then
    echo "Downloading..."
    mkdir -p "$RV_TEST_DIR"
    wget -O "$RV_TEST_DIR/sdcard-rv.img.gz" "$RV_TEST_URL"
    gunzip "$RV_TEST_DIR/sdcard-rv.img.gz"
    echo "Download and extraction complete."
else
    echo "$RV_TEST already exists."
fi

LA_TEST="../bin/loongarch64/sdcard-la.img"
LA_TEST_URL="https://github.com/Samuka007/testsuits-for-oskernel/releases/download/pre-2025-03-21/sdcard-la.img.gz"
LA_TEST_DIR="../bin/loongarch64"

if [ ! -f "$LA_TEST" ] || [ "$REGET" -eq 1 ]; then
    echo "$LA_TEST does not exist. Downloading..."
    mkdir -p "$LA_TEST_DIR"
    wget -O "$LA_TEST_DIR/sdcard-la.img.gz" "$LA_TEST_URL"
    gunzip "$LA_TEST_DIR/sdcard-la.img.gz"
    echo "Download and extraction complete."
else
    echo "$LA_TEST already exists."
fi