#!/usr/bin/env bash
# Makes the Open Build Service sources for one release tag (see README.md here):
#   prostir-vendored.tar.gz  the tagged source with every crate in vendor/ and the
#                            release's THIRD-PARTY-LICENSES.txt; unpacks to
#                            prostir-<version>/. Both the RPM and the Debian builds use it.
#   prostir.spec             RPM spec for openSUSE and Fedora
#   prostir.dsc              Debian source description for OBS (debtransform)
#   debian.tar.gz            the debian/ folder
# The packaging files come from this checkout, the source from the tag, so a tag made
# before packaging/obs existed can still be packaged.
#
# Usage: packaging/obs/make-sources.sh <tag> <THIRD-PARTY-LICENSES.txt> <output folder>
# Needs git, cargo (with network, for `cargo vendor`), python3, GNU tar and gzip.
set -euo pipefail

if [ $# -ne 3 ]; then
    echo "usage: $0 <tag> <THIRD-PARTY-LICENSES.txt> <output folder>" >&2
    exit 2
fi
tag="$1"
licenses="$(realpath "$2")"
out="$3"
here="$(cd "$(dirname "$0")" && pwd)"

if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "error: tag '$tag' is not vMAJOR.MINOR.PATCH" >&2
    exit 1
fi
version="${tag#v}"
commit="$(git rev-parse --verify "${tag}^{commit}")"
cargo_version="$(git show "${commit}:Cargo.toml" | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
if [ "$version" != "$cargo_version" ]; then
    echo "error: tag $tag does not match version $cargo_version in its Cargo.toml" >&2
    exit 1
fi
test -s "$licenses"

mkdir -p "$out"
out="$(realpath "$out")"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

src="$work/prostir-$version"
git archive --format=tar --prefix="prostir-$version/" "$commit" | tar -x -C "$work"
# Every crate in Cargo.lock, for every platform: cargo checks the whole lock file.
(cd "$src" && cargo vendor --quiet --locked --versioned-dirs vendor >/dev/null)
cp "$licenses" "$src/THIRD-PARTY-LICENSES.txt"

# The oldest Rust that builds the PC part: the newest rust-version among the crates it
# is made of (dependencies included), which can be newer than rust-version in
# Cargo.toml. The spec and the Debian build dependencies ask for it.
rust_min="$(cd "$src" && cargo metadata --format-version 1 --frozen \
    --config 'source.crates-io.replace-with="vendored-sources"' \
    --config 'source.vendored-sources.directory="vendor"' | python3 -c '
import json, sys
meta = json.load(sys.stdin)
packages = {p["id"]: p for p in meta["packages"]}
# Only what is built: dev-dependencies (the test renderer of the window) do not count.
deps = {n["id"]: [d["pkg"] for d in n["deps"]
                  if any(k["kind"] != "dev" for k in d["dep_kinds"])]
        for n in meta["resolve"]["nodes"]}
todo = [i for i, p in packages.items()
        if p["name"] in ("prostir-host", "prostir-proto", "prostir-settings")]
seen = set()
while todo:
    i = todo.pop()
    if i not in seen:
        seen.add(i)
        todo += deps[i]
versions = [tuple(map(int, packages[i]["rust_version"].split(".")[:2]))
            for i in seen if packages[i].get("rust_version")]
print("%d.%d" % max(versions))
')"
# Debian and Ubuntu: the distribution's cargo when it is new enough (cargo-web provides
# it on Debian 13), else one of Ubuntu's versioned toolchains, newest first. A year of
# Rust releases past the minimum.
minor="${rust_min#1.}"
cargo_dep="cargo (>= $rust_min)"
for ((n = minor + 8; n >= minor; n--)); do
    cargo_dep+=" | cargo-1.$n"
done
echo "Rust $rust_min or newer; Debian: $cargo_dep"

# Same bytes for the same tag: sorted entries, root owner, the commit's time.
epoch="$(git show -s --format=%ct "$commit")"
reproducible_tar() {
    tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$epoch" \
        --format=gnu -C "$1" -cf - "$2" | gzip -9n
}
reproducible_tar "$work" "prostir-$version" > "$out/prostir-vendored.tar.gz"

date_rfc="$(LC_ALL=C date -u -R -d "@$epoch")"
sed -e "s/@VERSION@/$version/g" -e "s/@RUST_MIN@/$rust_min/g" \
    "$here/prostir.spec" > "$out/prostir.spec"
mkdir "$work/deb"
cp -r "$here/debian" "$work/deb/"
sed -i -e "s/@VERSION@/$version/g" -e "s/@DATE@/$date_rfc/" "$work/deb/debian/changelog"
sed -i -e "s/@CARGO@/$cargo_dep/" "$work/deb/debian/control"
reproducible_tar "$work/deb" debian > "$out/debian.tar.gz"

# OBS installs the build dependencies listed in the .dsc, dpkg-buildpackage checks
# those in debian/control: one list, taken from debian/control.
build_depends="$(awk '
    /^#/ { next }
    /^Build-Depends:/ { on = 1; sub(/^Build-Depends:/, ""); print; next }
    on && /^[ \t]/ { print; next }
    { on = 0 }' "$work/deb/debian/control" | tr '\n' ' ' | tr -s ' \t' ' ' | sed 's/^ //; s/ $//')"
test -n "$build_depends"
sed -e "s/@VERSION@/$version/g" -e "s/@BUILD_DEPENDS@/$build_depends/" \
    "$here/prostir.dsc" > "$out/prostir.dsc"

if grep -n '@[A-Z_]*@' "$out/prostir.spec" "$out/prostir.dsc" "$work/deb/debian/control"; then
    echo "error: a placeholder is left" >&2
    exit 1
fi
ls -l "$out"
