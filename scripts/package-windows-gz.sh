#!/bin/sh
# Package per-asset Windows qscn gz artifacts for a release tag.
#
# Produces, under dist/<tag>/:
#   qscn-windows-amd64.exe.gz          gzip of the x86_64 qscn.exe
#   qscn-windows-amd64.exe.gz.sha256   sha256 of the gz file
#   qscn-windows-amd64.exe.sha256      sha256 of the uncompressed exe
#   qscn-windows-arm64.exe.gz          (same trio for aarch64)
#   qscn-windows-arm64.exe.gz.sha256
#   qscn-windows-arm64.exe.sha256
#
# These assets are consumed by quicktui-installer on Windows, which downloads
# the .gz, verifies both hashes, and installs qscn.exe automatically. Asset
# names carry no tag because release download URLs are already scoped by tag,
# and use amd64/arm64 to match QuickTUI asset naming. The sha256 file format
# is "<64-hex>  <name>" to stay compatible with `shasum -a 256 -c` and the
# installer's checksum parser.
#
# Usage: scripts/package-windows-gz.sh <tag>
# Expects both release binaries to exist:
#   target/x86_64-pc-windows-gnu/release/qscn.exe
#   target/aarch64-pc-windows-gnullvm/release/qscn.exe

set -eu

die() {
    printf 'package-windows-gz: %s\n' "$*" >&2
    exit 1
}

[ "$#" -eq 1 ] || die "usage: scripts/package-windows-gz.sh <tag>"
tag=$1
case "$tag" in
    ''|*[!0-9-]*) die "tag must look like YYYYMMDD-NN: $tag" ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd)
out_dir="${root}/dist/${tag}"
mkdir -p "$out_dir"

sha256_of() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        sha256sum "$1" | awk '{print $1}'
    fi
}

package_one() {
    target_dir=$1
    arch=$2
    exe="${root}/target/${target_dir}/release/qscn.exe"
    [ -f "$exe" ] || die "missing binary: $exe (run cargo zigbuild for ${target_dir} first)"

    name="qscn-windows-${arch}.exe"
    gz="${out_dir}/${name}.gz"

    gzip -9 -n -c "$exe" > "$gz"
    printf '%s  %s\n' "$(sha256_of "$exe")" "$name" > "${out_dir}/${name}.sha256"
    printf '%s  %s\n' "$(sha256_of "$gz")" "${name}.gz" > "${out_dir}/${name}.gz.sha256"
    printf 'packaged %s\n' "${name}.gz" >&2
}

package_one x86_64-pc-windows-gnu amd64
package_one aarch64-pc-windows-gnullvm arm64

gzip -t "${out_dir}/qscn-windows-amd64.exe.gz" "${out_dir}/qscn-windows-arm64.exe.gz"
printf 'done: %s\n' "$out_dir" >&2
