#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 TARGET VERSION OUTPUT_DIRECTORY" >&2
  exit 2
fi

target="$1"
version="$2"
output_dir="$3"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
package="trace-db-${version}-${target}"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

case "$target" in
  *-apple-darwin) extension="dylib" ;;
  *-linux-*) extension="so" ;;
  *) echo "unsupported Unix release target: $target" >&2; exit 2 ;;
esac

mkdir -p "$stage/$package/proto/tracedb/v1" "$output_dir"
cp "$root/target/$target/release/trace-db" "$stage/$package/trace-db"
cp "$root/target/$target/release/trace-db-bench" "$stage/$package/trace-db-bench"
cp "$root/target/$target/release/libfts5jieba.$extension" "$stage/$package/"
cp "$root/README.md" "$root/LICENSE" "$stage/$package/"
cp "$root/proto/tracedb/v1/tracedb.proto" "$stage/$package/proto/tracedb/v1/"
tar -C "$stage" -czf "$output_dir/$package.tar.gz" "$package"
