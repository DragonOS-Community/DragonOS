source pkg-config.sh
path=(
    gmp/6.2.1
    mpfr/4.1.1
    mpc/1.2.1
    flex/2.6.4
)

current_path=$(pwd)

for i in ${path[@]}; do
    echo "Building $i"
    cd $i
    ./build.sh || exit 1
    cd $current_path
done
cd $current_path