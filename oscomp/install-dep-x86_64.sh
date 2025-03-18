# Script to install dependencies for building the x86_64 version of the user packages in DragonOS

apt install unzip
bash install_musl_gcc.sh
rustup install nightly-2024-11-05-x86_64-unknown-linux-gnu
rustup target add x86_64-unknown-linux-musl --toolchain nightly-2024-11-05-x86_64-unknown-linux-gnu
