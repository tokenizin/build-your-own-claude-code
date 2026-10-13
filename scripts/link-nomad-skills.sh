#!/usr/bin/env bash
#
# P0/P1: Symlink NomadWorks / OpenCode methodology skills into this repo's
# .claude/skills/ so Claude Code and playground/rust discover them.
#
# Source of truth stays in StarWorld (or CodeNomad) — this repo only holds links.
#
# Usage:
#   ./scripts/link-nomad-skills.sh
#   STARWORLD=/path/to/starworld ./scripts/link-nomad-skills.sh
#   ./scripts/link-nomad-skills.sh --dry-run
#   ./scripts/link-nomad-skills.sh --unlink   # remove P1 symlinks only

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="${REPO_ROOT}/.claude/skills"
STARWORLD="${STARWORLD:-/Users/alexshapiro/starworld}"

DRY_RUN=0
UNLINK=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    --unlink) UNLINK=1 ;;
    -h|--help)
      sed -n '2,16p' "$0"
      exit 0
      ;;
    *)
      echo "Unknown arg: $arg" >&2
      exit 1
      ;;
  esac
done

# P1 methodology + CodeNomad ops (from docs/NOMADWORKS_INTEGRATION.md)
P1_SKILLS=(
  task-management
  project-delivery-management
  project-state
  ecosystem-readiness
  qa-validation-pipeline
  context-reconcile
  codenomad-build-restart
  codenomad-architecture-guide
  sidecar-management
  set-agent-model
)

SOURCE_ROOTS=(
  "${STARWORLD}/.opencode/skills"
  "${STARWORLD}/CodeNomad/.opencode/skills"
  "${STARWORLD}/.cursor/skills"
)

resolve_skill() {
  local name="$1"
  local root
  for root in "${SOURCE_ROOTS[@]}"; do
    if [ -f "${root}/${name}/SKILL.md" ]; then
      # Prefer real path (resolve one level of symlink in source trees)
      (cd "${root}/${name}" && pwd -P)
      return 0
    fi
  done
  return 1
}

mkdir -p "$TARGET_DIR"

if [ "$UNLINK" -eq 1 ]; then
  for name in "${P1_SKILLS[@]}"; do
    link_path="${TARGET_DIR}/${name}"
    if [ -L "$link_path" ]; then
      if [ "$DRY_RUN" -eq 1 ]; then
        echo "would unlink $link_path"
      else
        rm "$link_path"
        echo "unlinked $name"
      fi
    fi
  done
  exit 0
fi

echo "STARWORLD=$STARWORLD"
echo "TARGET=$TARGET_DIR"
echo

linked=0
missing=0
for name in "${P1_SKILLS[@]}"; do
  link_path="${TARGET_DIR}/${name}"
  if ! src="$(resolve_skill "$name")"; then
    echo "MISSING  $name (not found under source roots)" >&2
    missing=$((missing + 1))
    continue
  fi

  if [ -e "$link_path" ] && [ ! -L "$link_path" ]; then
    echo "SKIP     $name (exists and is not a symlink): $link_path" >&2
    continue
  fi

  if [ "$DRY_RUN" -eq 1 ]; then
    echo "would link $name -> $src"
  else
    ln -sfn "$src" "$link_path"
    echo "linked   $name -> $src"
  fi
  linked=$((linked + 1))
done

echo
echo "done: linked=$linked missing=$missing"
if [ "$missing" -ne 0 ]; then
  exit 1
fi
