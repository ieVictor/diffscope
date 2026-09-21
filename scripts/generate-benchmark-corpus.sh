#!/usr/bin/env bash
set -euo pipefail

root=${1:-target/benchmark-corpus}
rm -rf "$root"
mkdir -p "$root"
root=$(cd "$root" && pwd)

create_tier() {
  local name=$1
  local typescript_files=$2
  local markdown_files=$3
  local binary_files=$4
  local repo="$root/$name"

  mkdir -p "$repo/src" "$repo/docs" "$repo/assets"
  git -C "$repo" init --quiet
  git -C "$repo" config user.email diffscope-benchmark@example.invalid
  git -C "$repo" config user.name 'DiffScope Benchmark'

  for ((index = 1; index <= typescript_files; index++)); do
    cat >"$repo/src/file-${index}.ts" <<EOF
export function calculate${index}(value: number): number {
  if (value > ${index}) {
    return value + ${index};
  }
  return value;
}
EOF
  done
  for ((index = 1; index <= markdown_files; index++)); do
    printf '# Document %d\n\nBase benchmark content.\n' "$index" >"$repo/docs/file-${index}.md"
  done
  for ((index = 1; index <= binary_files; index++)); do
    printf '\0base-%04d\377' "$index" >"$repo/assets/file-${index}.bin"
  done

  git -C "$repo" add .
  GIT_AUTHOR_DATE='2025-01-01T00:00:00Z' GIT_COMMITTER_DATE='2025-01-01T00:00:00Z' \
    git -C "$repo" commit --quiet -m base

  for ((index = 1; index <= typescript_files; index++)); do
    cat >"$repo/src/file-${index}.ts" <<EOF
export function calculate${index}(value: number): number {
  if (value > ${index} && value % 2 === 0) {
    return value + ${index} + 1;
  }
  return value - 1;
}

export const format${index} = (value: number): string => String(value);
EOF
  done
  for ((index = 1; index <= markdown_files; index++)); do
    printf '# Document %d\n\nUpdated benchmark content with another line.\n' "$index" >"$repo/docs/file-${index}.md"
  done
  for ((index = 1; index <= binary_files; index++)); do
    printf '\0target-%04d\376' "$index" >"$repo/assets/file-${index}.bin"
  done

  git -C "$repo" add .
  GIT_AUTHOR_DATE='2025-01-02T00:00:00Z' GIT_COMMITTER_DATE='2025-01-02T00:00:00Z' \
    git -C "$repo" commit --quiet -m target
}

# Documented tiers: small <20, medium 20-200, and large >200 changed files.
create_tier small 7 2 1
create_tier medium 60 15 5
create_tier large 180 45 15

printf 'Generated benchmark repositories under %s\n' "$root"
