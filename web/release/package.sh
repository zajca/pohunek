#!/usr/bin/env bash

set -euo pipefail

readonly release_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly web_root="$(CDPATH= cd -- "${release_dir}/.." && pwd)"
readonly repository_root="$(CDPATH= cd -- "${web_root}/.." && pwd)"
readonly version="${1:-}"
readonly compile_target="bun-linux-x64-baseline"

if [[ ! "${version}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  printf '%s\n' "usage: release/package.sh <X.Y.Z>" >&2
  exit 2
fi

readonly name="pohunek-web-${version}-linux-x86_64"
readonly output_dir="${web_root}/dist"
readonly staging="${output_dir}/${name}"
readonly archive="${output_dir}/${name}.tar.gz"
readonly checksum="${archive}.sha256"

cd "${web_root}"
rm -rf -- "${staging}"
rm -f -- "${archive}" "${checksum}"
mkdir -p "${staging}/frontend"

bun run build:frontend
bun build \
  --compile \
  --target="${compile_target}" \
  --no-compile-autoload-dotenv \
  --no-compile-autoload-bunfig \
  --outfile="${staging}/pohunek-web" \
  ./backend/src/entrypoint.ts

cp -R frontend/dist/. "${staging}/frontend/"
cp backend/systemd/pohunek-backend.service.in "${staging}/"
cp release/backend.env.example release/install.sh release/README.md "${staging}/"
cp "${repository_root}/LICENSE" "${staging}/LICENSE"
chmod 0755 "${staging}/install.sh"

test -x "${staging}/pohunek-web"
test -f "${staging}/frontend/index.html"
test -f "${staging}/LICENSE"
bash -n "${staging}/install.sh"

bun run release/smoke.ts "${staging}/pohunek-web" "${staging}/frontend"

tar -czf "${archive}" -C "${output_dir}" "${name}"
(
  cd "${output_dir}"
  sha256sum "${name}.tar.gz" > "${name}.tar.gz.sha256"
)

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  printf 'archive=dist/%s.tar.gz\n' "${name}" >> "${GITHUB_OUTPUT}"
  printf 'checksum=dist/%s.tar.gz.sha256\n' "${name}" >> "${GITHUB_OUTPUT}"
fi

printf 'Created %s and %s\n' "${archive}" "${checksum}"
