#!/usr/bin/env bash
# Install or upgrade Corrall from a GitHub release, and migrate an existing
# TeamClaude install (this project before it was renamed, or the original
# Node.js implementation) over to the new name.
#
#   gh api repos/NodeSpy/corrall/contents/scripts/install.sh -H "Accept: application/vnd.github.raw" | bash
#   ./scripts/install.sh [--version vX.Y.Z] [--dry-run] [--rollback] [--no-service] [--no-npm]
#
# What it does, in order:
#   1. Downloads the release archive for this OS/arch plus SHA256SUMS, verifies
#      the checksum, and (when cosign / gh attestation are available) the
#      Sigstore signature and build provenance.
#   2. Backs up the config and state file: ~/.config/corrall.json, or
#      ~/.config/teamclaude.json when only that one exists.
#   3. Stops the systemd --user unit (corrall.service or teamclaude.service).
#   4. Migrates a TeamClaude install: renames teamclaude.json, its .state.json
#      and the teamclaude-*.pem MITM certificates to corrall.*, removes the
#      teamclaude.service unit, the ~/.local/bin/teamclaude binary and the npm
#      global @karpeleslab/teamclaude (remembering its version).
#   5. Installs the binary to ~/.local/bin/corrall and validates the config.
#   6. Writes corrall.service in place of a migrated unit, restarts it if it
#      was running, and waits for /corrall/health.
#   Any failure after step 3 rolls everything back automatically.
#
# The repository is private: downloads go through `gh` (GitHub CLI), which must
# be logged in with access to NodeSpy/corrall. Set GH_TOKEN to run headless.

set -euo pipefail

NAME="corrall"
LEGACY_NAME="teamclaude"
REPO="${CORRALL_REPO:-NodeSpy/corrall}"
INSTALL_DIR="${CORRALL_INSTALL_DIR:-$HOME/.local/bin}"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}"
CONFIG="${CORRALL_CONFIG:-$CONFIG_DIR/$NAME.json}"
STATE="${CONFIG%.json}.state.json"
LEGACY_CONFIG="${TEAMCLAUDE_CONFIG:-$CONFIG_DIR/$LEGACY_NAME.json}"
LEGACY_STATE="${LEGACY_CONFIG%.json}.state.json"
UNIT="$NAME.service"
LEGACY_UNIT="$LEGACY_NAME.service"
UNIT_DIR="$CONFIG_DIR/systemd/user"
NPM_PKG="@karpeleslab/teamclaude"
# MITM certificate files, `<name>-<suffix>`, beside the config file.
CERT_SUFFIXES="ca.pem leaf.pem leaf.key"
VERSION=""
DRY_RUN=0
DO_ROLLBACK=0
MANAGE_SERVICE=1
MANAGE_NPM=1
STAMP="$(date +%Y%m%d-%H%M%S)"
ROLLBACK_FILE="${CONFIG%.json}.rollback"

usage() { sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit 0; }
log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
run() { if [ "$DRY_RUN" = 1 ]; then printf '   (dry-run) %s\n' "$*"; else "$@"; fi; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --version=*) VERSION="${1#*=}"; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    --rollback) DO_ROLLBACK=1; shift ;;
    --no-service) MANAGE_SERVICE=0; shift ;;
    --no-npm) MANAGE_NPM=0; shift ;;
    -h|--help) usage ;;
    *) die "unknown argument: $1" ;;
  esac
done

need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required"; }
need gh; need tar; need python3; need curl
command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1 || die "sha256sum or shasum is required"

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi
}

target_triple() {
  local os arch
  os="$(uname -s)"; arch="$(uname -m)"
  case "$os" in
    Linux)  case "$arch" in x86_64|amd64) echo x86_64-unknown-linux-musl ;; aarch64|arm64) echo aarch64-unknown-linux-musl ;; *) die "unsupported arch $arch" ;; esac ;;
    Darwin) case "$arch" in x86_64) echo x86_64-apple-darwin ;; arm64) echo aarch64-apple-darwin ;; *) die "unsupported arch $arch" ;; esac ;;
    *) die "unsupported OS $os" ;;
  esac
}

# $1 = unit name
unit_present() { [ "$MANAGE_SERVICE" = 1 ] && command -v systemctl >/dev/null 2>&1 && systemctl --user list-unit-files "$1" 2>/dev/null | grep -q "$1"; }
unit_active() { unit_present "$1" && systemctl --user is-active --quiet "$1"; }
unit_enabled() { unit_present "$1" && systemctl --user is-enabled --quiet "$1" 2>/dev/null; }
npm_version() { [ "$MANAGE_NPM" = 1 ] && command -v npm >/dev/null 2>&1 && npm ls -g --depth=0 --json 2>/dev/null | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("dependencies",{}).get("'"$NPM_PKG"'",{}).get("version",""))' 2>/dev/null || true; }

# $1 = control-plane prefix (corrall or teamclaude), $2 = config file to read the port from
wait_healthy() {
  local prefix="$1" cfgfile="$2" port
  port="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("proxy",{}).get("port",3456))' "$cfgfile" 2>/dev/null || echo 3456)"
  for _ in $(seq 1 30); do
    if curl -fsS "http://127.0.0.1:${port}/${prefix}/health" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  return 1
}

# Move $1 to $2 unless $2 already exists (never clobber on either direction).
move_if_free() {
  [ -e "$1" ] || return 0
  if [ -e "$2" ]; then warn "$2 already exists; leaving $1 in place"; return 0; fi
  run mv "$1" "$2"
}

# ── rollback ──────────────────────────────────────────────────

rollback() {
  [ -f "$ROLLBACK_FILE" ] || die "no rollback record at $ROLLBACK_FILE"
  # shellcheck disable=SC1090
  . "$ROLLBACK_FILE"
  log "Rolling back to the previous installation"
  unit_present "$UNIT" && { run systemctl --user stop "$UNIT" || true; }
  unit_present "$LEGACY_UNIT" && { run systemctl --user stop "$LEGACY_UNIT" || true; }

  if [ -n "${PREV_BINARY_BACKUP:-}" ] && [ -f "$PREV_BINARY_BACKUP" ]; then
    run cp -f "$PREV_BINARY_BACKUP" "$INSTALL_DIR/$NAME"
    run chmod 755 "$INSTALL_DIR/$NAME"
  else
    run rm -f "$INSTALL_DIR/$NAME"
  fi
  if [ -n "${LEGACY_BINARY_BACKUP:-}" ] && [ -f "$LEGACY_BINARY_BACKUP" ]; then
    run cp -f "$LEGACY_BINARY_BACKUP" "$INSTALL_DIR/$LEGACY_NAME"
    run chmod 755 "$INSTALL_DIR/$LEGACY_NAME"
  fi
  if [ -n "${PREV_NPM_VERSION:-}" ]; then
    log "Reinstalling $NPM_PKG@$PREV_NPM_VERSION"
    run npm install -g "$NPM_PKG@$PREV_NPM_VERSION"
  fi

  # The config may hold refresh tokens rotated by the new binary, which are the
  # valid ones. Move it back under the old name when it was migrated, and only
  # restore the backup when the current file is broken.
  local live_config="$CONFIG"
  if [ "${MIGRATED_CONFIG:-0}" = 1 ]; then
    log "Moving the config back to $LEGACY_CONFIG"
    move_if_free "$CONFIG" "$LEGACY_CONFIG"
    move_if_free "$STATE" "$LEGACY_STATE"
    local s
    for s in $CERT_SUFFIXES; do move_if_free "$(dirname "$CONFIG")/$NAME-$s" "$(dirname "$LEGACY_CONFIG")/$LEGACY_NAME-$s"; done
    run rm -f "${CONFIG%.json}.lock"
    live_config="$LEGACY_CONFIG"
  fi
  if ! python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$live_config" 2>/dev/null; then
    warn "current config is unreadable; restoring $CONFIG_BACKUP"
    run cp -f "$CONFIG_BACKUP" "$live_config"
  fi
  [ -n "${STATE_BACKUP:-}" ] && [ -f "$STATE_BACKUP" ] && run cp -f "$STATE_BACKUP" "${live_config%.json}.state.json"

  if [ "${MIGRATED_UNIT:-0}" = 1 ]; then
    log "Restoring $LEGACY_UNIT"
    run rm -f "$UNIT_DIR/$UNIT"
    [ -n "${LEGACY_UNIT_BACKUP:-}" ] && [ -f "$LEGACY_UNIT_BACKUP" ] && run cp -f "$LEGACY_UNIT_BACKUP" "$UNIT_DIR/$LEGACY_UNIT"
    run systemctl --user daemon-reload
    [ "${LEGACY_UNIT_ENABLED:-0}" = 1 ] && { run systemctl --user enable "$LEGACY_UNIT" || true; }
  fi
  if [ "${WAS_ACTIVE:-0}" = 1 ]; then
    local unit="${ACTIVE_UNIT:-$UNIT}" prefix="$NAME"
    [ "$unit" = "$LEGACY_UNIT" ] && prefix="$LEGACY_NAME"
    run systemctl --user daemon-reload
    run systemctl --user start "$unit"
    if [ "$DRY_RUN" = 0 ] && wait_healthy "$prefix" "$live_config"; then log "Previous installation is serving again"; else warn "service restarted but health check did not pass; inspect: journalctl --user -u $unit"; fi
  fi
  log "Rollback complete"
}

if [ "$DO_ROLLBACK" = 1 ]; then rollback; exit 0; fi

# ── download + verify ─────────────────────────────────────────

TARGET="$(target_triple)"
if [ -z "$VERSION" ]; then
  VERSION="$(gh release view -R "$REPO" --json tagName --jq .tagName 2>/dev/null || true)"
  [ -n "$VERSION" ] || die "could not determine the latest release of $REPO (is gh logged in?)"
fi
ARCHIVE="$NAME-${VERSION}-${TARGET}.tar.gz"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT

log "Downloading $ARCHIVE from $REPO $VERSION"
gh release download "$VERSION" -R "$REPO" -D "$WORK" -p "$ARCHIVE" -p SHA256SUMS -p 'SHA256SUMS.sigstore.json' >/dev/null

log "Verifying checksum"
expected="$(grep " $ARCHIVE\$" "$WORK/SHA256SUMS" | awk '{print $1}')"
[ -n "$expected" ] || die "$ARCHIVE not listed in SHA256SUMS"
actual="$(sha256_file "$WORK/$ARCHIVE")"
[ "$expected" = "$actual" ] || die "checksum mismatch for $ARCHIVE"

if command -v cosign >/dev/null 2>&1 && [ -f "$WORK/SHA256SUMS.sigstore.json" ]; then
  log "Verifying Sigstore signature on SHA256SUMS"
  cosign verify-blob --bundle "$WORK/SHA256SUMS.sigstore.json" \
    --certificate-identity-regexp "github.com/${REPO}/" \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com "$WORK/SHA256SUMS" >/dev/null 2>&1 \
    || die "Sigstore verification failed"
else
  warn "cosign not installed; skipping signature verification (checksum verified)"
fi
# Provenance attestations exist only when the repository's plan allows them;
# their absence is not a failure, a present-but-invalid one is.
if gh attestation verify --help >/dev/null 2>&1; then
  out="$(gh attestation verify "$WORK/$ARCHIVE" -R "$REPO" 2>&1)" && log "Build provenance attestation verified" || {
    if printf '%s' "$out" | grep -qi "no attestations found\|not found\|404"; then
      warn "no provenance attestation published for this release (private repository); relying on checksum + Sigstore"
    else
      die "provenance attestation failed: $out"
    fi
  }
fi

tar -C "$WORK" -xzf "$WORK/$ARCHIVE"
NEW_BIN="$WORK/${ARCHIVE%.tar.gz}/$NAME"
[ -x "$NEW_BIN" ] || die "archive did not contain the binary"
log "New binary: $("$NEW_BIN" --version)"

# ── what is installed now ─────────────────────────────────────

WAS_ACTIVE=0; ACTIVE_UNIT=""
if unit_active "$UNIT"; then WAS_ACTIVE=1; ACTIVE_UNIT="$UNIT"
elif unit_active "$LEGACY_UNIT"; then WAS_ACTIVE=1; ACTIVE_UNIT="$LEGACY_UNIT"; fi
MIGRATE_UNIT=0; unit_present "$LEGACY_UNIT" && MIGRATE_UNIT=1
LEGACY_UNIT_ENABLED=0; unit_enabled "$LEGACY_UNIT" && LEGACY_UNIT_ENABLED=1
MIGRATE_CONFIG=0
if [ ! -e "$CONFIG" ] && [ -f "$LEGACY_CONFIG" ]; then
  MIGRATE_CONFIG=1
elif [ -e "$CONFIG" ] && [ -f "$LEGACY_CONFIG" ]; then
  warn "both $CONFIG and $LEGACY_CONFIG exist; using the former and leaving the latter untouched"
fi
PREV_NPM_VERSION="$(npm_version)"
SRC_CONFIG="$CONFIG"; [ "$MIGRATE_CONFIG" = 1 ] && SRC_CONFIG="$LEGACY_CONFIG"
SRC_STATE="${SRC_CONFIG%.json}.state.json"

if [ "$MIGRATE_CONFIG" = 1 ] || [ "$MIGRATE_UNIT" = 1 ] || [ -e "$INSTALL_DIR/$LEGACY_NAME" ] || [ -n "$PREV_NPM_VERSION" ]; then
  log "TeamClaude install found; it will be migrated to $NAME"
fi

# ── back up ───────────────────────────────────────────────────

CONFIG_BACKUP=""; STATE_BACKUP=""; PREV_BINARY_BACKUP=""; LEGACY_BINARY_BACKUP=""; LEGACY_UNIT_BACKUP=""
mkdir -p "$(dirname "$CONFIG")"
if [ -f "$SRC_CONFIG" ]; then
  CONFIG_BACKUP="${SRC_CONFIG}.bak-${STAMP}"
  log "Backing up config to $CONFIG_BACKUP"
  run cp -p "$SRC_CONFIG" "$CONFIG_BACKUP"; run chmod 600 "$CONFIG_BACKUP"
fi
if [ -f "$SRC_STATE" ]; then
  STATE_BACKUP="${SRC_STATE}.bak-${STAMP}"
  run cp -p "$SRC_STATE" "$STATE_BACKUP"; run chmod 600 "$STATE_BACKUP"
fi
if [ -f "$INSTALL_DIR/$NAME" ]; then
  PREV_BINARY_BACKUP="${INSTALL_DIR}/$NAME.bak-${STAMP}"
  run cp -p "$INSTALL_DIR/$NAME" "$PREV_BINARY_BACKUP"
fi
if [ -f "$INSTALL_DIR/$LEGACY_NAME" ] && [ ! -L "$INSTALL_DIR/$LEGACY_NAME" ]; then
  LEGACY_BINARY_BACKUP="${INSTALL_DIR}/$LEGACY_NAME.bak-${STAMP}"
  run cp -p "$INSTALL_DIR/$LEGACY_NAME" "$LEGACY_BINARY_BACKUP"
fi
if [ "$MIGRATE_UNIT" = 1 ] && [ -f "$UNIT_DIR/$LEGACY_UNIT" ]; then
  LEGACY_UNIT_BACKUP="${CONFIG%.json}.$LEGACY_UNIT.bak-${STAMP}"
  run cp -p "$UNIT_DIR/$LEGACY_UNIT" "$LEGACY_UNIT_BACKUP"
fi

if [ "$DRY_RUN" = 0 ]; then
  cat > "$ROLLBACK_FILE" <<REC
WAS_ACTIVE=$WAS_ACTIVE
ACTIVE_UNIT=$ACTIVE_UNIT
MIGRATED_UNIT=$MIGRATE_UNIT
MIGRATED_CONFIG=$MIGRATE_CONFIG
LEGACY_UNIT_ENABLED=$LEGACY_UNIT_ENABLED
LEGACY_UNIT_BACKUP=$LEGACY_UNIT_BACKUP
PREV_NPM_VERSION=$PREV_NPM_VERSION
CONFIG_BACKUP=$CONFIG_BACKUP
STATE_BACKUP=$STATE_BACKUP
PREV_BINARY_BACKUP=$PREV_BINARY_BACKUP
LEGACY_BINARY_BACKUP=$LEGACY_BINARY_BACKUP
REC
  chmod 600 "$ROLLBACK_FILE"
else
  printf '   (dry-run) write rollback record to %s\n' "$ROLLBACK_FILE"
fi

on_failure() {
  local rc=$?
  trap - ERR
  warn "swap failed (exit $rc); rolling back"
  rollback || true
  rm -rf "$WORK"
  exit "$rc"
}
trap on_failure ERR

# ── swap over ─────────────────────────────────────────────────

# Never let two implementations share the config: refresh tokens rotate.
if [ "$WAS_ACTIVE" = 1 ]; then
  log "Stopping $ACTIVE_UNIT"
  run systemctl --user stop "$ACTIVE_UNIT"
fi
# A server that is not the unit we just stopped must not share the config.
unit_pids="$( { unit_present "$UNIT" && systemctl --user show -p MainPID --value "$UNIT" 2>/dev/null; unit_present "$LEGACY_UNIT" && systemctl --user show -p MainPID --value "$LEGACY_UNIT" 2>/dev/null; echo 0; } | tr ' ' '\n')"
own="$(printf '%s\n' "$$" "$BASHPID"; pgrep -P "$$" 2>/dev/null || true)"
strays="$(pgrep -f "(^|/)($NAME|$LEGACY_NAME) server" 2>/dev/null | grep -vxF -f <(printf '%s\n' "$unit_pids") | grep -vxF -f <(printf '%s\n' "$own") || true)"
if [ -n "$strays" ] && [ "$DRY_RUN" = 0 ]; then
  if [ "$MANAGE_SERVICE" = 1 ]; then
    die "a $NAME/$LEGACY_NAME server is running outside systemd (pid $strays); stop it first so two instances never share the config"
  else
    warn "another $NAME/$LEGACY_NAME server is running (pid $strays); make sure it does not use $CONFIG"
  fi
fi

if [ -n "$PREV_NPM_VERSION" ]; then
  log "Removing npm global $NPM_PKG@$PREV_NPM_VERSION"
  run npm uninstall -g "$NPM_PKG"
fi

if [ "$MIGRATE_CONFIG" = 1 ]; then
  log "Renaming $LEGACY_CONFIG to $CONFIG"
  run mv "$LEGACY_CONFIG" "$CONFIG"
  move_if_free "$LEGACY_STATE" "$STATE"
  # Keep the MITM CA the user already trusts: the files carry the identity,
  # the name on disk does not.
  for s in $CERT_SUFFIXES; do move_if_free "$(dirname "$LEGACY_CONFIG")/$LEGACY_NAME-$s" "$(dirname "$CONFIG")/$NAME-$s"; done
  run rm -f "${LEGACY_CONFIG%.json}.lock"
fi

if [ -e "$INSTALL_DIR/$LEGACY_NAME" ]; then
  log "Removing $INSTALL_DIR/$LEGACY_NAME"
  run rm -f "$INSTALL_DIR/$LEGACY_NAME" "$INSTALL_DIR/$LEGACY_NAME.prev"
fi

log "Installing to $INSTALL_DIR/$NAME"
run mkdir -p "$INSTALL_DIR"
run install -m 755 "$NEW_BIN" "$INSTALL_DIR/$NAME"
case ":$PATH:" in *":$INSTALL_DIR:"*) ;; *) warn "$INSTALL_DIR is not on your PATH" ;; esac

if [ -f "$CONFIG" ] || { [ "$DRY_RUN" = 1 ] && [ "$MIGRATE_CONFIG" = 1 ]; }; then
  log "Validating config"
  if [ "$DRY_RUN" = 0 ]; then CORRALL_CONFIG="$CONFIG" "$INSTALL_DIR/$NAME" config check >/dev/null; fi
fi

if [ "$MIGRATE_UNIT" = 1 ]; then
  log "Replacing $LEGACY_UNIT with $UNIT"
  run systemctl --user disable "$LEGACY_UNIT" || true
  run rm -f "$UNIT_DIR/$LEGACY_UNIT"
  run mkdir -p "$UNIT_DIR"
  if [ "$DRY_RUN" = 0 ]; then
    CORRALL_CONFIG="$CONFIG" "$INSTALL_DIR/$NAME" service print > "$UNIT_DIR/$UNIT"
  else
    printf '   (dry-run) write %s from: %s service print\n' "$UNIT_DIR/$UNIT" "$NAME"
  fi
  run systemctl --user daemon-reload
  [ "$LEGACY_UNIT_ENABLED" = 1 ] && run systemctl --user enable "$UNIT"
fi

if [ "$WAS_ACTIVE" = 1 ]; then
  log "Starting $UNIT"
  run systemctl --user daemon-reload
  run systemctl --user start "$UNIT"
  if [ "$DRY_RUN" = 0 ]; then
    wait_healthy "$NAME" "$CONFIG" || { journalctl --user -u "$UNIT" -n 20 --no-pager >&2 || true; false; }
    log "Proxy is healthy"
    CORRALL_CONFIG="$CONFIG" "$INSTALL_DIR/$NAME" status || true
  fi
elif unit_present "$UNIT"; then
  log "Unit exists but was not running; leaving it stopped (systemctl --user start $UNIT)"
else
  log "No systemd unit found. Start with: $NAME server   (or: $NAME service install)"
fi

trap - ERR
log "Done. Rollback any time with: $0 --rollback"
if [ "$MIGRATE_CONFIG" = 1 ] || [ "$MIGRATE_UNIT" = 1 ] || [ -n "$LEGACY_BINARY_BACKUP" ] || [ -n "$PREV_NPM_VERSION" ]; then
  cat <<NOTE

Migrated from TeamClaude. Still yours to update:
  - shell wrappers:   eval "\$(teamclaude env)"  ->  eval "\$($NAME env)"
  - environment:      TEAMCLAUDE_*  ->  CORRALL_*   (TC_ACCT / TC_POOL are unchanged)
  - control plane:    /teamclaude/*  ->  /$NAME/*   (dashboards, health checks)
  - Prometheus:       teamclaude_*  ->  ${NAME}_*  metric names
  - Codex:            the model_provider entry in ~/.codex/config.toml, if you named it teamclaude
NOTE
fi
if [ -n "$CONFIG_BACKUP" ]; then
  log "Next: exercise a real OAuth round trip:  $NAME api /api/oauth/profile"
fi
