#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

go_mod="${root_dir}/go.mod"
rust_toolchain="${root_dir}/rust-toolchain.toml"
go_docker="${root_dir}/deploy/docker/go.Dockerfile"
rust_docker="${root_dir}/deploy/docker/Dockerfile.rust"
ansible_playbook="${root_dir}/deploy/ansible/playbook.yml"
readme="${root_dir}/README.md"

if [[ ! -f "${rust_toolchain}" ]]; then
  echo "rust-toolchain.toml not found"
  exit 1
fi

if [[ ! -f "${go_mod}" ]]; then
  echo "go.mod not found"
  exit 1
fi

go_version=$(awk '/^go[[:space:]]+[0-9]+\.[0-9]+\.[0-9]+/ {print $2; exit}' "${go_mod}")
if [[ -z "${go_version}" ]]; then
  echo "Failed to read Go version from go.mod"
  exit 1
fi

rust_version=$(awk -F'"' '/^channel[[:space:]]*=/ {print $2; exit}' "${rust_toolchain}")
if [[ -z "${rust_version}" ]]; then
  echo "Failed to read Rust version from rust-toolchain.toml"
  exit 1
fi

go_version_re=${go_version//./\\.}
rust_version_re=${rust_version//./\\.}

fail=0

check() {
  local file="$1"
  local pattern="$2"
  local label="$3"

  if ! grep -qE "${pattern}" "${file}"; then
    echo "Mismatch: ${label} in ${file}"
    echo "  Expected pattern: ${pattern}"
    fail=1
  fi
}

check "${go_docker}" "^FROM[[:space:]]+golang:${go_version_re}-alpine" "Go Docker image"
check "${rust_docker}" "^FROM[[:space:]]+rust:${rust_version_re}-slim" "Rust Docker image"
check "${ansible_playbook}" "go_version:[[:space:]]+\"${go_version_re}\"" "Ansible go_version"
check "${ansible_playbook}" "rust_version:[[:space:]]+\"${rust_version_re}\"" "Ansible rust_version"
check "${readme}" "Rust[^0-9]*${rust_version_re}" "README Rust version"
check "${readme}" "Go[^0-9]*${go_version_re}" "README Go version"

if [[ ${fail} -ne 0 ]]; then
  echo "Toolchain version drift detected"
  exit 1
fi

echo "Toolchain versions are aligned"
