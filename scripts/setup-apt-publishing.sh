#!/usr/bin/env bash
# One-shot setup for the apt repo publishing flow:
#
#   1. Generate (or import) a dedicated GPG key for signing the
#      Release file. NOT the maintainer's personal key — that'd be
#      a single point of failure if the GitHub Actions secret leaks.
#   2. Upload the private half + key id to repo secrets via `gh`.
#   3. Enable GitHub Pages on the `gh-pages` branch.
#   4. Print the DNS record the user must still add by hand.
#
# Idempotent: re-running detects already-configured pieces and
# only does the missing ones. The private key never lands in a
# named file on disk — it's piped directly into `gh secret set`.
#
# Usage: ./scripts/setup-apt-publishing.sh [--force-key]
#
# --force-key generates a fresh signing key even if one already
# lives in this user's GPG keyring. Use after a suspected leak.
set -euo pipefail

REPO_SLUG="wdes/tab-atelier"
# RFC 4880 §5.11 lets a User ID be any UTF-8 string; apt only
# cares about the cryptographic signature + fingerprint pinned via
# `[signed-by=…]`. We ship the public key from
# https://deb.tab-atelier.wdes.eu/tab-atelier.gpg so we don't need
# `gpg --search-keys` / `keys.openpgp.org` lookup — which means no
# email on the PRIMARY User ID. After key gen the script
# references the key by FINGERPRINT everywhere, which is what GPG
# itself prefers.
KEY_NAME="tab-atelier release signing"
# Second User ID matching the gh-pages committer identity. The
# `crazy-max/ghaction-import-gpg` action refuses to configure
# git-signing unless the committer email matches one of the key's
# UIDs; this UID exists purely to satisfy that check (apt and
# the keyserver flow ignore it).
BOT_NAME="Wdes Bot"
BOT_EMAIL="williamdes+wdes-bot@wdes.fr"
KEY_EXPIRE="5y"
DOMAIN="deb.tab-atelier.wdes.eu"
PAGES_BRANCH="gh-pages"

FORCE_KEY=0
for arg in "$@"; do
    case "$arg" in
        --force-key) FORCE_KEY=1 ;;
        -h|--help)
            sed -n '1,/^set -e/p' "$0" | grep '^#'
            exit 0
            ;;
        *)
            echo "unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

step() { printf '\n\033[1;36m→ %s\033[0m\n' "$*"; }
ok()   { printf '\033[32m  ✓ %s\033[0m\n' "$*"; }
warn() { printf '\033[33m  ⚠ %s\033[0m\n' "$*"; }
err()  { printf '\033[31m  ✗ %s\033[0m\n' "$*" >&2; }

# ───── prereqs ──────────────────────────────────────────────────────────

step "Checking prerequisites"
for cmd in gpg gh dig jq; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        err "$cmd not on PATH"
        exit 1
    fi
done
ok "gpg / gh / dig / jq found"

if ! gh auth status >/dev/null 2>&1; then
    err "gh is not authenticated. Run \`gh auth login\` first."
    exit 1
fi
GH_USER=$(gh api user --jq .login)
ok "gh authenticated as $GH_USER"

# Confirm we're pointing at the right repo. `gh repo view` against
# REPO_SLUG fails fast if the user lacks access.
if ! gh repo view "$REPO_SLUG" --json name >/dev/null 2>&1; then
    err "cannot reach $REPO_SLUG with the current gh auth. Set REPO_SLUG in the script and re-run."
    exit 1
fi
ok "gh has access to $REPO_SLUG"

# ───── GPG signing key ─────────────────────────────────────────────────

step "Signing key"

# Look the key up by its User ID string. With no email on the UID
# this is the only stable handle we have until the fingerprint is
# captured.
find_fpr_by_name() {
    gpg --list-secret-keys --with-colons "$KEY_NAME" 2>/dev/null \
        | awk -F: '/^fpr:/ { print $10; exit }'
}

FPR=$(find_fpr_by_name || true)
if [[ -n "$FPR" && $FORCE_KEY -eq 0 ]]; then
    ok "key '$KEY_NAME' already in keyring (fpr ${FPR:0:16}…)"
else
    if [[ -n "$FPR" && $FORCE_KEY -eq 1 ]]; then
        warn "--force-key: deleting existing '$KEY_NAME' key from local keyring (fpr ${FPR:0:16}…)"
        gpg --batch --yes --delete-secret-keys "$FPR" >/dev/null 2>&1 || true
        gpg --batch --yes --delete-keys "$FPR" >/dev/null 2>&1 || true
    fi
    echo "  Generating a fresh ed25519 signing key (no passphrase — it's a CI-side secret)…"
    # No `Name-Email` field → User ID is just `Name-Real`. Valid
    # per RFC 4880 §5.11 and apt verifies by fingerprint anyway.
    gpg --batch --gen-key <<EOF >/dev/null
%no-protection
Key-Type: EDDSA
Key-Curve: ed25519
Key-Usage: sign
Name-Real: $KEY_NAME
Expire-Date: $KEY_EXPIRE
EOF
    FPR=$(find_fpr_by_name)
    if [[ -z "$FPR" ]]; then
        err "key generation appeared to succeed but no fingerprint visible — aborting"
        exit 1
    fi
    ok "generated key (fpr ${FPR:0:16}…)"
fi

# Ensure the bot User ID exists on the key. crazy-max's
# ghaction-import-gpg enforces a committer-email/UID match before
# it'll configure git signing; this UID exists for that check.
# `--quick-add-uid` is idempotent at the protocol level but
# *adds duplicates* if invoked twice — so check first.
BOT_UID="$BOT_NAME <$BOT_EMAIL>"
if gpg --list-keys --with-colons "$FPR" 2>/dev/null \
        | awk -F: -v want="$BOT_UID" '/^uid:/ { if ($10 == want) found=1 } END { exit found ? 0 : 1 }'; then
    ok "key already carries UID '$BOT_UID'"
else
    gpg --batch --yes --quick-add-uid "$FPR" "$BOT_UID"
    ok "added UID '$BOT_UID' to key ${FPR:0:16}…"
fi

# Save the public key into the repo so users can curl it during
# install. The workflow re-exports the same key onto gh-pages on
# every publish, but committing it under `assets/` means the source
# repo is also a usable reference for the fingerprint.
PUB_PATH="assets/tab-atelier-release.gpg"
mkdir -p "$(dirname "$PUB_PATH")"
gpg --armor --export "$FPR" > "$PUB_PATH"
ok "public key exported to $PUB_PATH"

# Revocation certificate.
#
# GnuPG 2.1+ auto-creates one at `~/.gnupg/openpgp-revocs.d/<FPR>.rev`
# the moment a key is generated. That file is "use only as a last
# resort" by design — its presence on the same disk as the live
# secret key means a laptop compromise loses BOTH. So we copy it
# into a location the user is supposed to move off-machine, and
# loudly say so.
REVOKE_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/tab-atelier"
REVOKE_PATH="$REVOKE_DIR/apt-signing-revocation.asc"
AUTO_REVOKE="$HOME/.gnupg/openpgp-revocs.d/${FPR}.rev"
mkdir -p "$REVOKE_DIR"
if [[ -f "$AUTO_REVOKE" ]]; then
    cp "$AUTO_REVOKE" "$REVOKE_PATH"
    chmod 600 "$REVOKE_PATH"
    ok "revocation cert copied to $REVOKE_PATH (mode 600)"
else
    warn "no auto-generated revocation cert at $AUTO_REVOKE — falling back to interactive --gen-revoke"
    gpg --output "$REVOKE_PATH" --gen-revoke "$FPR"
    chmod 600 "$REVOKE_PATH"
    ok "revocation cert written to $REVOKE_PATH"
fi
warn "MOVE $REVOKE_PATH OFF THIS MACHINE — encrypted USB / password manager / printout."
warn "If it stays here, a laptop theft loses the live key AND the emergency stop."
warn "To publish a revocation later:"
warn "    gpg --import $REVOKE_PATH"
warn "    gpg --armor --export $FPR > tab-atelier.gpg     # contains the revocation"
warn "Then replace tab-atelier.gpg on the gh-pages branch — apt clients re-fetch it on update."

# ───── repo secrets ────────────────────────────────────────────────────

step "Repo secrets on $REPO_SLUG"

# Pipe the private key straight into gh; never lands on disk.
gpg --armor --export-secret-keys "$FPR" \
    | gh secret set APT_SIGNING_KEY -R "$REPO_SLUG"
ok "APT_SIGNING_KEY set"

# Store the fingerprint (not the User ID) so the workflow can pass
# it verbatim to `gpg --local-user` — that's an unambiguous,
# email-independent reference.
printf '%s' "$FPR" | gh secret set APT_SIGNING_KEY_ID -R "$REPO_SLUG"
ok "APT_SIGNING_KEY_ID set to fingerprint ${FPR:0:16}…"

# ───── register key on the GH user account ─────────────────────────────
#
# `crazy-max/ghaction-import-gpg` configures git to sign commits
# with this key during the apt-publish workflow. For GitHub to
# render those commits with the green "Verified" badge it also
# needs to know about the key under the committer's account.
#
# Idempotent: skip if a key with the same fingerprint is already
# registered.

step "Register public key on $GH_USER's GitHub account"

# GitHub stores each GPG key as immutable — adding a UID locally
# means we have to DELETE the previously-uploaded copy and POST
# again. Look up our key id by fingerprint, then verify whether
# the bot UID is present; if not, drop + re-add.
#
# `gh api user/gpg_keys` requires the `admin:gpg_key` token
# scope. If the call fails we treat the response as empty and
# fall through to the "warn + suggest refresh" branch below.
GH_KEYS_JSON=$(gh api user/gpg_keys 2>/dev/null || echo "[]")
if ! echo "$GH_KEYS_JSON" | jq -e 'type == "array"' >/dev/null 2>&1; then
    GH_KEYS_JSON="[]"
fi
GH_KEY_ROW=$(echo "$GH_KEYS_JSON" | jq --arg fpr "$FPR" '.[] | select(.key_id == ($fpr | .[-16:]))')
GH_KEY_ID=$(printf '%s' "$GH_KEY_ROW" | jq -r '.id // empty')
GH_KEY_HAS_BOT_UID=$(printf '%s' "$GH_KEY_ROW" \
    | jq --arg email "$BOT_EMAIL" -r 'any(.emails[]?; .email == $email) // false')

upload_gh_key() {
    # gh api expects --input <file|->. Hand-roll the JSON because
    # `-f key=value` doesn't tolerate newlines in armored PGP blocks.
    jq -n --arg key "$(gpg --armor --export "$FPR")" '{armored_public_key: $key}' \
        | gh api user/gpg_keys --method POST --input - >/dev/null
}

if [[ -n "$GH_KEY_ID" && "$GH_KEY_HAS_BOT_UID" == "true" ]]; then
    ok "key ${FPR:0:16}… already registered on github.com/$GH_USER (with bot UID)"
elif [[ -n "$GH_KEY_ID" ]]; then
    echo "  Key is registered but missing the bot UID; deleting + re-adding…"
    if gh api -X DELETE "user/gpg_keys/$GH_KEY_ID" >/dev/null 2>&1 && upload_gh_key 2>/dev/null; then
        ok "refreshed key on github.com/$GH_USER (bot UID now present)"
    else
        warn "could not refresh key on github.com/$GH_USER — likely missing the 'admin:gpg_key' scope"
        warn "Run: gh auth refresh -s admin:gpg_key   then re-run this script."
    fi
else
    if upload_gh_key 2>/dev/null; then
        ok "uploaded public key to github.com/$GH_USER (commits signed by this key will show as Verified)"
    else
        warn "could not upload public key to github.com/$GH_USER — likely missing the 'admin:gpg_key' scope on the gh token"
        warn "Run: gh auth refresh -s admin:gpg_key   then re-run this script."
    fi
fi

# ───── GitHub Pages ───────────────────────────────────────────────────

step "GitHub Pages on $REPO_SLUG"

# `GET /repos/.../pages` returns 404 if Pages isn't enabled yet.
if gh api "repos/$REPO_SLUG/pages" >/dev/null 2>&1; then
    # Already enabled — PUT to make sure the source branch is right.
    gh api -X PUT "repos/$REPO_SLUG/pages" \
        -f "build_type=legacy" \
        -f "source[branch]=$PAGES_BRANCH" \
        -f "source[path]=/" \
        >/dev/null
    ok "Pages already enabled, source pinned to $PAGES_BRANCH:/"
else
    # The gh-pages branch may not exist yet; that's fine — POST
    # creates the Pages site config now, the apt-publish workflow
    # will create the branch on its first run.
    if ! gh api "repos/$REPO_SLUG/branches/$PAGES_BRANCH" >/dev/null 2>&1; then
        warn "$PAGES_BRANCH branch doesn't exist yet — the first apt-publish CI run will create it."
        warn "Re-run this script AFTER that first run to finish the Pages setup."
    else
        gh api -X POST "repos/$REPO_SLUG/pages" \
            -f "build_type=legacy" \
            -f "source[branch]=$PAGES_BRANCH" \
            -f "source[path]=/" \
            >/dev/null
        ok "Pages enabled, source = $PAGES_BRANCH:/"
    fi
fi

# ───── custom domain ──────────────────────────────────────────────────

step "Custom domain $DOMAIN"

CURRENT_DOMAIN=$(gh api "repos/$REPO_SLUG/pages" --jq .cname 2>/dev/null || true)
if [[ "$CURRENT_DOMAIN" == "$DOMAIN" ]]; then
    ok "GitHub Pages already configured for $DOMAIN"
else
    # Pages picks the CNAME up from the file in gh-pages, but
    # setting it explicitly via the API enforces HTTPS even before
    # the branch has the file.
    if gh api "repos/$REPO_SLUG/pages" >/dev/null 2>&1; then
        gh api -X PUT "repos/$REPO_SLUG/pages" -f "cname=$DOMAIN" >/dev/null || \
            warn "could not PUT cname yet (likely because the gh-pages branch is empty)"
        ok "API-side cname set to $DOMAIN (will fully apply after the first publish)"
    fi
fi

# Live DNS sanity check.
RESOLVED=$(dig +short CNAME "$DOMAIN" 2>/dev/null | head -1 | sed 's/\.$//')
EXPECTED="$GH_USER.github.io"
if [[ "$RESOLVED" == "$EXPECTED" ]]; then
    ok "DNS: $DOMAIN → $EXPECTED"
else
    warn "DNS: $DOMAIN resolves to ${RESOLVED:-<no record>}; expected $EXPECTED"
    echo "      Add a CNAME record at your DNS provider:"
    echo "          $DOMAIN.   CNAME   $EXPECTED."
fi

# ───── done ───────────────────────────────────────────────────────────

step "Done"
cat <<EOF
The apt-publish workflow is now ready. Trigger it the usual way:

    git push origin main      # produces a NIGHTLY build
    git tag v0.4.1 && git push origin v0.4.1
                              # produces a STABLE build

Once gh-pages is populated, install on Debian/Ubuntu with:

    curl -fsSL https://$DOMAIN/tab-atelier.gpg \\
        | sudo tee /usr/share/keyrings/tab-atelier.gpg > /dev/null
    echo "deb [signed-by=/usr/share/keyrings/tab-atelier.gpg] https://$DOMAIN stable main" \\
        | sudo tee /etc/apt/sources.list.d/tab-atelier.list > /dev/null
    sudo apt update
    sudo apt install tab-atelier

Replace 'stable' with 'nightly' to track main.

Public signing key kept at $PUB_PATH for reference;
the same key is re-exported onto gh-pages by every publish run.
EOF
