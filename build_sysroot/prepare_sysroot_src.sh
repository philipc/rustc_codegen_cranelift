#!/bin/bash
set -e
cd $(dirname "$0")

RUST_DIR=$(dirname "$(rustup which rustc)")
SRC_DIR="$RUST_DIR/../lib/rustlib/src/rust/"
DST_DIR="sysroot_src"

if [ ! -e "$SRC_DIR" ]; then
    echo "Please install rust-src component"
    exit 1
fi

rm -rf $DST_DIR
mkdir -p $DST_DIR/src
cp -r "$SRC_DIR/src" $DST_DIR/

pushd $DST_DIR
echo "[GIT] init"
git init
echo "[GIT] add"
git add .
echo "[GIT] commit"
git commit -m "Initial commit" -q
# Fix line endings on Windows
rm -rf src
git checkout src
for file in $(ls ../../patches/ | grep -v patcha); do
echo "[GIT] apply" $file
git apply ../../patches/$file
git commit --no-gpg-sign -am "Patch $file"
done
popd

echo "Successfully prepared libcore for building"
