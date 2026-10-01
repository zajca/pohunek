#!/usr/bin/env bash

set -euo pipefail

readonly release_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly web_root="$(CDPATH= cd -- "${release_dir}/.." && pwd)"
readonly repository_root="$(CDPATH= cd -- "${web_root}/.." && pwd)"
readonly version="${1:-}"
readonly target="${2:-linux-x86_64}"
readonly mode="${3:-}"

if [[ ! "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf '%s\n' "usage: release/package.sh <X.Y.Z> [linux-x86_64|aarch64-apple-darwin [--input-only]]" >&2
  exit 2
fi

# <target> names the archive; the manifest carries the Rust-style triple the
# installers compare with their host.
case "${target}" in
  linux-x86_64)
    readonly compile_target="bun-linux-x64-baseline"
    readonly manifest_triple="x86_64-unknown-linux-gnu"
    ;;
  aarch64-apple-darwin)
    readonly compile_target="bun-darwin-arm64"
    readonly manifest_triple="aarch64-apple-darwin"
    ;;
  *)
    printf '%s\n' "unsupported target: ${target}" >&2
    exit 2
    ;;
esac
if [[ -n "${mode}" && "${mode}" != "--input-only" ]]; then
  printf '%s\n' "unsupported option: ${mode}" >&2
  exit 2
fi
if [[ "${mode}" == "--input-only" && "${target}" != "aarch64-apple-darwin" ]]; then
  printf '%s\n' "--input-only applies to the macOS target, whose archive is signed by packaging/macos/package" >&2
  exit 2
fi

readonly name="pohunek-web-${version}-${target}"
readonly output_dir="${web_root}/dist"
readonly input="${output_dir}/input-${target}"
readonly archive="${output_dir}/${name}.tar.gz"
readonly checksum="${archive}.sha256"

cd "${web_root}"
rm -rf -- "${input}"
rm -f -- "${archive}" "${checksum}"
mkdir -p "${input}/frontend"

bun run build:frontend
bun build \
  --compile \
  --target="${compile_target}" \
  --no-compile-autoload-dotenv \
  --no-compile-autoload-bunfig \
  --outfile="${input}/pohunek-web" \
  ./backend/src/entrypoint.ts

cp -R frontend/dist/. "${input}/frontend/"
cp release/backend.env.example release/install.sh release/README.md "${input}/"
if [[ "${target}" == "linux-x86_64" ]]; then
  cp backend/systemd/pohunek-backend.service.in "${input}/"
fi
chmod 0755 "${input}/install.sh"

test -x "${input}/pohunek-web"
test -f "${input}/frontend/index.html"
bash -n "${input}/install.sh"

# The compiled backend serves the SPA with a fixture daemon. The macOS target
# is built and run natively on an arm64 Mac.
bun run release/smoke.ts "${input}/pohunek-web" "${input}/frontend"

if [[ "${mode}" == "--input-only" ]]; then
  if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
    printf 'input=%s\n' "${input}" >> "${GITHUB_OUTPUT}"
  fi
  printf '%s\n' "${input}"
  exit 0
fi

cd "${repository_root}"
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}"
export SOURCE_DATE_EPOCH
packaging/stage-archive web "${version}" "${target}" "${input}" "${web_root}" "${output_dir}" > /dev/null
test -x "${output_dir}/${name}/pohunek-web"
sh packaging/write-manifest "${output_dir}/${name}" web "${version}" "${manifest_triple}" none
sh packaging/archive "${output_dir}" "${name}" "${output_dir}"

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  printf 'archive=dist/%s.tar.gz\n' "${name}" >> "${GITHUB_OUTPUT}"
  printf 'checksum=dist/%s.tar.gz.sha256\n' "${name}" >> "${GITHUB_OUTPUT}"
fi

printf 'Created %s and %s\n' "${archive}" "${checksum}"
