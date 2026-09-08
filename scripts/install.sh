#!/usr/bin/env bash
# Install or upgrade TeamClaude (Rust) from a GitHub release, and swap over from
# the original Node.js implementation if it is present.
#
#   gh api repos/NodeSpy/teamclaude/contents/scripts/install.sh -H "Accept: application/vnd.github.raw" | bash
#   ./scripts/install.sh [--version vX.Y.Z] [--dry-run] [--rollback] [--no-service] [--no-npm]
#
# What it does, in order:
#   1. Downloads the release archive for this OS/arch plus SHA256SUMS, verifies
#      the checksum, and (when cosign / gh attestation are available) the
#      Sigstore signature and build provenance.
#   2. Backs up ~/.config/teamclaude.json and the state file.
#   3. Stops the systemd --user unit if one is running.
#   4. Removes the npm global @karpeleslab/teamclaude (remembering its version).
#   5. Installs the binary to ~/.local/bin/teamclaude and validates the config.
#   6. Restarts the unit and waits for /teamclaude/health.
#   Any failure after step 3 rolls everything back automatically.
#
# The repository is private: downloads go through `gh` (GitHub CLI), which must
# be logged in with access to NodeSpy/teamclaude. Set GH_TOKEN to run headless.

set -euo pipefail

REPO="${TEAMCLAUDE_REPO:-NodeSpy/teamclaude}"
INSTALL_DIR="${TEAMCLAUDE_INSTALL_DIR:-$HOME/.local/bin}"
CONFIG="${TEAMCLAUDE_CONFIG:-${XDG_CONFIG_HOME:-$HOME/.config}/teamclaude.json}"
STATE="${CONFIG%.json}.state.json"
UNIT="teamclaude.service"
NPM_PKG="@karpeleslab/teamclaude"
VERSION=""
DRY_RUN=0
DO_ROLLBACK=0
MANAGE_SERVICE=1
MANAGE_NPM=1
STAMP="$(date +%Y%m%d-%H%M%S)"
ROLLBACK_FILE="${CONFIG%.json}.rollback"

usage() { sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0; }
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

service_present() { [ "$MANAGE_SERVICE" = 1 ] && command -v systemctl >/dev/null 2>&1 && systemctl --user list-unit-files "$UNIT" 2>/dev/null | grep -q "$UNIT"; }
service_active() { service_present && systemctl --user is-active --quiet "$UNIT"; }
npm_version() { [ "$MANAGE_NPM" = 1 ] && command -v npm >/dev/null 2>&1 && npm ls -g --depth=0 --json 2>/dev/null | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("dependencies",{}).get("'"$NPM_PKG"'",{}).get("version",""))' 2>/dev/null || true; }

wait_healthy() {
  local port
  port="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("proxy",{}).get("port",3456))' "$CONFIG" 2>/dev/null || echo 3456)"
  for _ in $(seq 1 30); do
    if curl -fsS "http://127.0.0.1:${port}/teamclaude/health" >/dev/null 2>&1; then return 0; fi
    sleep 0.5
  done
  return 1
}

# ── rollback ──────────────────────────────────────────────────

rollback() {
  [ -f "$ROLLBACK_FILE" ] || die "no rollback record at $ROLLBACK_FILE"
  # shellcheck disable=SC1090
  . "$ROLLBACK_FILE"
  log "Rolling back to the previous installation"
  if service_present; then run systemctl --user stop "$UNIT" || true; fi
  if [ -n "${PREV_BINARY_BACKUP:-}" ] && [ -f "$PREV_BINARY_BACKUP" ]; then
    run cp -f "$PREV_BINARY_BACKUP" "$INSTALL_DIR/teamclaude"
    run chmod 755 "$INSTALL_DIR/teamclaude"
  else
    run rm -f "$INSTALL_DIR/teamclaude"
  fi
  if [ -n "${PREV_NPM_VERSION:-}" ]; then
    log "Reinstalling $NPM_PKG@$PREV_NPM_VERSION"
    run npm install -g "$NPM_PKG@$PREV_NPM_VERSION"
  fi
  # The current config may hold refresh tokens rotated by the new binary, which
  # are the valid ones. Only restore the backup when the current file is broken.
  if ! python3 -c 'import json,sys; json.load(open(sys.argv[1]))' "$CONFIG" 2>/dev/null; then
    warn "current config is unreadable; restoring $CONFIG_BACKUP"
    run cp -f "$CONFIG_BACKUP" "$CONFIG"
  fi
  [ -n "${STATE_BACKUP:-}" ] && [ -f "$STATE_BACKUP" ] && run cp -f "$STATE_BACKUP" "$STATE"
  if [ "${WAS_ACTIVE:-0}" = 1 ]; then
    run systemctl --user daemon-reload
    run systemctl --user start "$UNIT"
    if [ "$DRY_RUN" = 0 ] && wait_healthy; then log "Previous implementation is serving again"; else warn "service restarted but health check did not pass; inspect: journalctl --user -u $UNIT"; fi
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
ARCHIVE="teamclaude-${VERSION}-${TARGET}.tar.gz"
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
NEW_BIN="$WORK/${ARCHIVE%.tar.gz}/teamclaude"
[ -x "$NEW_BIN" ] || die "archive did not contain the binary"
log "New binary: $("$NEW_BIN" --version)"

# ── swap over ─────────────────────────────────────────────────

WAS_ACTIVE=0; service_active && WAS_ACTIVE=1
PREV_NPM_VERSION="$(npm_version)"
CONFIG_BACKUP=""; STATE_BACKUP=""; PREV_BINARY_BACKUP=""

if [ -f "$CONFIG" ]; then
  CONFIG_BACKUP="${CONFIG}.bak-${STAMP}"
  log "Backing up config to $CONFIG_BACKUP"
  run cp -p "$CONFIG" "$CONFIG_BACKUP"; run chmod 600 "$CONFIG_BACKUP"
fi
if [ -f "$STATE" ]; then
  STATE_BACKUP="${STATE}.bak-${STAMP}"
  run cp -p "$STATE" "$STATE_BACKUP"; run chmod 600 "$STATE_BACKUP"
fi
if [ -f "$INSTALL_DIR/teamclaude" ] && [ -z "$PREV_NPM_VERSION" ]; then
  PREV_BINARY_BACKUP="${INSTALL_DIR}/teamclaude.bak-${STAMP}"
  run cp -p "$INSTALL_DIR/teamclaude" "$PREV_BINARY_BACKUP"
fi

if [ "$DRY_RUN" = 0 ]; then
  cat > "$ROLLBACK_FILE" <<REC
WAS_ACTIVE=$WAS_ACTIVE
PREV_NPM_VERSION=$PREV_NPM_VERSION
CONFIG_BACKUP=$CONFIG_BACKUP
STATE_BACKUP=$STATE_BACKUP
PREV_BINARY_BACKUP=$PREV_BINARY_BACKUP
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

# Never let two implementations share the config: refresh tokens rotate.
if [ "$WAS_ACTIVE" = 1 ]; then
  log "Stopping $UNIT"
  run systemctl --user stop "$UNIT"
fi
# A server that is not the unit we just stopped must not share the config.
unit_pid="$(service_present && systemctl --user show -p MainPID --value "$UNIT" 2>/dev/null || echo 0)"
own="$(printf '%s\n' "$$" "$BASHPID"; pgrep -P "$$" 2>/dev/null || true)"
strays="$(pgrep -f "teamclaude server" 2>/dev/null | grep -vx "${unit_pid:-0}" | grep -vxF -f <(printf '%s\n' "$own") || true)"
if [ -n "$strays" ] && [ "$DRY_RUN" = 0 ]; then
  if [ "$MANAGE_SERVICE" = 1 ]; then
    die "a teamclaude server is running outside systemd (pid $strays); stop it first so two instances never share the config"
  else
    warn "another teamclaude server is running (pid $strays); make sure it does not use $CONFIG"
  fi
fi

if [ -n "$PREV_NPM_VERSION" ]; then
  log "Removing npm global $NPM_PKG@$PREV_NPM_VERSION"
  run npm uninstall -g "$NPM_PKG"
fi

log "Installing to $INSTALL_DIR/teamclaude"
run mkdir -p "$INSTALL_DIR"
run install -m 755 "$NEW_BIN" "$INSTALL_DIR/teamclaude"
case ":$PATH:" in *":$INSTALL_DIR:"*) ;; *) warn "$INSTALL_DIR is not on your PATH" ;; esac

if [ -f "$CONFIG" ]; then
  log "Validating config"
  if [ "$DRY_RUN" = 0 ]; then TEAMCLAUDE_CONFIG="$CONFIG" "$INSTALL_DIR/teamclaude" config check >/dev/null; fi
fi

if [ "$WAS_ACTIVE" = 1 ]; then
  log "Starting $UNIT"
  run systemctl --user daemon-reload
  run systemctl --user start "$UNIT"
  if [ "$DRY_RUN" = 0 ]; then
    wait_healthy || { journalctl --user -u "$UNIT" -n 20 --no-pager >&2 || true; false; }
    log "Proxy is healthy"
    TEAMCLAUDE_CONFIG="$CONFIG" "$INSTALL_DIR/teamclaude" status || true
  fi
elif service_present; then
  log "Unit exists but was not running; leaving it stopped (systemctl --user start $UNIT)"
else
  log "No systemd unit found. Start with: teamclaude server   (or: teamclaude service install)"
fi

trap - ERR
log "Done. Rollback any time with: $0 --rollback"
if [ -n "$CONFIG_BACKUP" ]; then
  log "Next: exercise a real OAuth round trip:  teamclaude api /api/oauth/profile"
fi
