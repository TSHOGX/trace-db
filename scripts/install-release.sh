#!/usr/bin/env bash
set -euo pipefail

# Install the native TraceDB release archive for the current Unix platform.
# The default prefix is user-local and can be overridden with --prefix or
# TRACEDB_INSTALL_PREFIX. A release version is recommended for reproducibility;
# when omitted, the latest GitHub release tag is resolved first.

repo="${TRACEDB_REPO:-TSHOGX/trace-db}"
version="${TRACEDB_VERSION:-}"
prefix="${TRACEDB_INSTALL_PREFIX:-${HOME:-}/.local}"
base_url="${TRACEDB_RELEASE_BASE_URL:-}"

usage() {
  cat >&2 <<'EOF'
usage: install-release.sh [--version VERSION] [--prefix DIRECTORY] [--repo OWNER/REPO]

Environment overrides:
  TRACEDB_VERSION, TRACEDB_INSTALL_PREFIX, TRACEDB_REPO,
  TRACEDB_RELEASE_BASE_URL (for mirrors or local release testing)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      [[ $# -ge 2 ]] || { usage; exit 2; }
      version="$2"
      shift 2
      ;;
    --prefix)
      [[ $# -ge 2 ]] || { usage; exit 2; }
      prefix="$2"
      shift 2
      ;;
    --repo)
      [[ $# -ge 2 ]] || { usage; exit 2; }
      repo="$2"
      shift 2
      ;;
    -h|--help)
      usage >&1
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

case "$(uname -s):$(uname -m)" in
  Darwin:x86_64) target="x86_64-apple-darwin"; extension="libfts5jieba.dylib" ;;
  Darwin:arm64|Darwin:aarch64) target="aarch64-apple-darwin"; extension="libfts5jieba.dylib" ;;
  Linux:x86_64) target="x86_64-unknown-linux-gnu"; extension="libfts5jieba.so" ;;
  Linux:aarch64|Linux:arm64) target="aarch64-unknown-linux-gnu"; extension="libfts5jieba.so" ;;
  *)
    echo "unsupported platform: $(uname -s) $(uname -m); use a published archive manually" >&2
    exit 2
    ;;
esac

fetch() {
  local url="$1"
  local destination="$2"
  if command -v curl >/dev/null 2>&1; then
    curl --fail --location --silent --show-error "$url" --output "$destination"
  elif command -v wget >/dev/null 2>&1; then
    wget --quiet --output-document="$destination" "$url"
  else
    echo "install requires curl or wget" >&2
    exit 2
  fi
}

if [[ -z "$version" ]]; then
  latest_url="https://github.com/$repo/releases/latest"
  if command -v curl >/dev/null 2>&1; then
    location="$(curl --fail --location --silent --show-error --output /dev/null --write-out '%{url_effective}' "$latest_url")"
  elif command -v wget >/dev/null 2>&1; then
    location="$(wget --server-response --max-redirect=20 --spider "$latest_url" 2>&1 | sed -n 's/^  Location: //p' | tail -1)"
  else
    echo "install requires curl or wget" >&2
    exit 2
  fi
  version="${location##*/v}"
  [[ "$version" != "$location" && -n "$version" ]] || {
    echo "could not resolve the latest release version from $location" >&2
    exit 1
  }
fi
version="${version#v}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]] || {
  echo "invalid release version: $version" >&2
  exit 2
}

asset="trace-db-${version}-${target}.tar.gz"
if [[ -z "$base_url" ]]; then
  base_url="https://github.com/$repo/releases/download/v$version"
fi
base_url="${base_url%/}"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
fetch "$base_url/$asset" "$stage/$asset"
fetch "$base_url/SHA256SUMS" "$stage/SHA256SUMS"

expected="$(awk -v asset="$asset" '$2 == asset { print $1; found = 1 } END { if (!found) exit 1 }' "$stage/SHA256SUMS")" || {
  echo "SHA256SUMS has no entry for $asset" >&2
  exit 1
}
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$stage/$asset" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$stage/$asset" | awk '{print $1}')"
else
  echo "install requires sha256sum or shasum for checksum verification" >&2
  exit 2
fi
[[ "$actual" == "$expected" ]] || {
  echo "checksum mismatch for $asset" >&2
  exit 1
}

while IFS= read -r member; do
  case "$member" in
    /*|../*|*/../*|*/./*|./*)
      echo "archive contains an unsafe member path: $member" >&2
      exit 1
      ;;
  esac
done < <(tar -tzf "$stage/$asset")
tar -xzf "$stage/$asset" -C "$stage"
package="$stage/trace-db-${version}-${target}"
[[ -d "$package" ]] || { echo "archive has no expected package directory" >&2; exit 1; }
for binary in trace-db trace-db-bench trace-db-relevance; do
  [[ -x "$package/$binary" ]] || { echo "archive is missing executable $binary" >&2; exit 1; }
done
[[ -f "$package/$extension" ]] || { echo "archive is missing $extension" >&2; exit 1; }
[[ -f "$package/README.md" && -f "$package/LICENSE" ]] || {
  echo "archive is missing README.md or LICENSE" >&2
  exit 1
}

mkdir -p "$prefix/bin" "$prefix/lib" "$prefix/share/doc/trace-db-$version"
for binary in trace-db trace-db-bench trace-db-relevance; do
  install -m 0755 "$package/$binary" "$prefix/bin/$binary"
done
install -m 0644 "$package/$extension" "$prefix/lib/$extension"
cp "$package/README.md" "$package/LICENSE" "$prefix/share/doc/trace-db-$version/"
mkdir -p "$prefix/share/doc/trace-db-$version/proto/tracedb/v1"
cp "$package/proto/tracedb/v1/tracedb.proto" "$prefix/share/doc/trace-db-$version/proto/tracedb/v1/"

installed_version="$("$prefix/bin/trace-db" --version | awk '{print $2}')"
[[ "$installed_version" == "$version" ]] || {
  echo "installed trace-db reports version $installed_version, expected $version" >&2
  exit 1
}
echo "installed TraceDB $version for $target in $prefix"
if [[ ":${PATH}:" != *":$prefix/bin:"* ]]; then
  echo "add $prefix/bin to PATH to use trace-db" >&2
fi
