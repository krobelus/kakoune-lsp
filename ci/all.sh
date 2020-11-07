#!/bin/sh

set -ex

sort=sort
if command -v gsort >/dev/null; then
    sort=gsort # for `sort --sort-version`, from brew's coreutils.
fi

# This fetches latest stable release
tag=$(git ls-remote --tags --refs --exit-code https://github.com/rust-embedded/cross \
                   | cut -d/ -f3 \
                   | grep -E '^v[0.1.0-9.]+$' \
                   | $sort --version-sort \
                   | tail -n1)

curl -LSfs https://japaric.github.io/trust/install.sh | \
    sh -s -- \
       --force \
       --git rust-embedded/cross \
       --tag $tag \
       --target $TARGET

source ~/.cargo/env || true

cross build --target $TARGET --release --verbose
cross test  --target $TARGET --release --verbose

src=$PWD
stage=$(mktemp -d)

cp target/$TARGET/release/kak-lsp $stage
cp kak-lsp.toml $stage
cp README.asciidoc $stage
cp COPYING $stage
cp MIT $stage
cp UNLICENSE $stage

cd $stage
tar czf $src/$CRATE_NAME-$TRAVIS_TAG-$TARGET.tar.gz *
cd $src

rm -rf $stage
