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

# CI workflow must not override rust-toolchain.toml with a pinned action tag
ci_yml="${root_dir}/.github/workflows/ci.yml"
if [[ -f "${ci_yml}" ]]; then
  if grep -qE 'rust-toolchain@(stable|nightly|[0-9]+\.)' "${ci_yml}"; then
    echo "Mismatch: CI overrides rust-toolchain.toml via action tag"
    fail=1
  fi
fi

# gRPC address alignment: systemd services should match
go_service="${root_dir}/deploy/systemd/aether-go.service"
rust_service="${root_dir}/deploy/systemd/aether-rust.service"
if [[ -f "${go_service}" && -f "${rust_service}" ]]; then
  go_grpc=$(grep 'GRPC_ADDRESS=' "${go_service}" | sed 's/.*GRPC_ADDRESS=//' | head -1)
  rust_grpc=$(grep 'GRPC_ADDRESS=' "${rust_service}" | sed 's/.*GRPC_ADDRESS=//' | head -1)
  if [[ -n "${go_grpc}" && -n "${rust_grpc}" && "${go_grpc}" != "${rust_grpc}" ]]; then
    echo "Mismatch: gRPC address differs between aether-go.service (${go_grpc}) and aether-rust.service (${rust_grpc})"
    fail=1
  fi
fi

if [[ ${fail} -ne 0 ]]; then
  echo "Toolchain version drift detected"
  exit 1
fi

echo "Toolchain versions are aligned"
