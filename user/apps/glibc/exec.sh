if [ ! -f "glibc-2.35.tar.gz" ]; then
  wget https://ftp.gnu.org/gnu/glibc/glibc-2.35.tar.gz
fi
if [ ! -d "glibc-2.35" ]; then
  tar -xvf glibc-2.35.tar.gz
  cp ./install_deps.sh ./glibc-2.35/
  cp ./default_configure.sh ./glibc-2.35/
fi
cd glibc-2.35
bash install_deps.sh
bash default_configure.sh
cd build
make -j $(nproc)
DESTDIR=$DADK_CURRENT_BUILD_DIR make install -j $(nproc)

mkdir -p $DADK_CURRENT_BUILD_DIR/lib64
cp -r $DADK_CURRENT_BUILD_DIR/lib/* $DADK_CURRENT_BUILD_DIR/lib64