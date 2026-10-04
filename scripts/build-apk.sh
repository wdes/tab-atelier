#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# Build and sign the Tab Atelier Remote APK from the pinned ConnectBot tree.
#
#   scripts/build-apk.sh                signed release APK
#   scripts/build-apk.sh --debug        debug APK (upstream's debug key)
#
# Output: connectbot/app/build/outputs/apk/oss/<variant>/*.apk
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
upstream="$root/connectbot"
variant=release
task=assembleOssRelease

case "${1:-}" in
    --debug) variant=debug; task=assembleOssDebug ;;
    "") ;;
    *) echo "usage: $0 [--debug]" >&2; exit 2 ;;
esac

: "${ANDROID_HOME:=$HOME/Android/Sdk}"
export ANDROID_HOME
export ANDROID_SDK_ROOT="$ANDROID_HOME"
: "${JAVA_HOME:=/usr/lib/jvm/java-21-openjdk-amd64}"
export JAVA_HOME

# connectbot/ is an ordinary directory of this repository now — it was a submodule
# with our changes layered on as quilt patches. Our changes are commits, so there
# is nothing to reset or re-apply: what is checked out is what gets built.

args=()
if [[ "$variant" == release ]]; then
    # Neither this file nor the key is in this repository (it is public).
    # Locally both live with the other persistent identity material in
    # ~/.config/tab-atelier/; in CI both arrive as secrets. See README.
    config="${ANDROID_KEYSTORE_CONFIG:-$HOME/.config/tab-atelier/keystore.properties}"
    password="${ANDROID_KEYSTORE_PASSWORD:-}"
    alias="${ANDROID_KEYSTORE_ALIAS:-}"
    if [[ -r "$config" ]]; then
        [[ -n "$password" ]] || password="$(sed -n 's/^storePassword=//p' "$config")"
        [[ -n "$alias" ]] || alias="$(sed -n 's/^keyAlias=//p' "$config")"
    fi
    [[ -n "$alias" ]] || alias="ta-remote"
    if [[ -z "$password" ]]; then
        echo "No signing password: set ANDROID_KEYSTORE_PASSWORD, or put" >&2
        echo "storePassword=... in $config, which is outside this repository." >&2
        exit 1
    fi
    # The key itself is NEVER in this repository. It is the app's identity —
    # Google's ADI ownership is tied to this certificate and installed apps
    # upgrade in place on it — and this repository is public. Locally it lives
    # with the other persistent identity material, in ~/.config/tab-atelier/;
    # in CI it arrives as the ANDROID_KEYSTORE_BASE64 secret and is decoded to
    # a temporary file that never touches the checkout.
    keystore="${ANDROID_KEYSTORE_FILE:-$HOME/.config/tab-atelier/release.keystore}"
    if [[ -n "${ANDROID_KEYSTORE_BASE64:-}" ]]; then
        keystore="$(mktemp --suffix=.keystore)"
        trap 'rm -f "$keystore"' EXIT
        printf '%s' "$ANDROID_KEYSTORE_BASE64" | base64 -d > "$keystore"
    fi
    if [[ ! -s "$keystore" ]]; then
        echo "No signing keystore at $keystore." >&2
        echo "It is deliberately not in the repository — see NOTICE and README." >&2
        exit 1
    fi
    args+=(
        "-PkeystoreFile=$keystore"
        "-PkeystorePassword=$password"
        "-PkeystoreAlias=$alias"
    )
fi

# versionCode must climb: the installed app is 16777472 and Android refuses an
# upgrade that does not increase. Defaults live in the overlay patch.
[[ -n "${APP_VERSION_CODE:-}" ]] && args+=("-PappVersionCode=$APP_VERSION_CODE")
[[ -n "${APP_VERSION_NAME:-}" ]] && args+=("-PappVersionName=$APP_VERSION_NAME")

# The commit the APK was built from, shown on the About screen. CI passes the
# short SHA it already computed; a local build names the commit it is building,
# so the About screen of a hand-built APK is not a different kind of "unknown".
APP_BUILD_COMMIT="${APP_BUILD_COMMIT:-$(git -C "$root" rev-parse --short HEAD 2>/dev/null || true)}"
[[ -n "$APP_BUILD_COMMIT" ]] && args+=("-PbuildCommit=$APP_BUILD_COMMIT")
# The upstream half of the fork, for the About screen. connectbot/ was squashed in
# with `git subtree`, which records the upstream commit it came from in a
# `git-subtree-split` trailer — so the most recent commit carrying that trailer is
# the upstream base, and it stays correct across future subtree pulls with nothing
# to remember. (It was `git -C connectbot rev-parse HEAD` while connectbot was a
# submodule; that would now resolve to this repository's own HEAD, since
# connectbot/ is a plain directory here.)
APP_UPSTREAM_COMMIT="${APP_UPSTREAM_COMMIT:-$(git -C "$root" log -1 --grep='git-subtree-split:' --format='%(trailers:key=git-subtree-split,valueonly)' 2>/dev/null || true)}"
[[ -n "$APP_UPSTREAM_COMMIT" ]] && args+=("-PupstreamCommit=$APP_UPSTREAM_COMMIT")

cd "$upstream"
./gradlew --no-daemon -Dorg.gradle.jvmargs="-Xmx2g -XX:MaxMetaspaceSize=1g" \
    "$task" "${args[@]}"

apk_dir="app/build/outputs/apk/oss/$variant"
echo
echo "APK:"
ls -1 "$upstream/$apk_dir"/*.apk 2>/dev/null || echo "  (none found under $apk_dir)"
