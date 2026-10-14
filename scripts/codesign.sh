#!/usr/bin/env bash
# Sign + notarize the sift binary. Called by goreleaser as a `builds.hooks.post` step with the build target
# (a Rust triple: aarch64-apple-darwin) and the binary path.
#
# Requires, for a signed build:
#   - MACOS_SIGN_IDENTITY (from .env.template, resolved through 1Password at release time)
#   - `xcrun notarytool` (Xcode CLT) and a keychain profile named "notarytool", set up once via:
#       xcrun notarytool store-credentials "notarytool" \
#         --apple-id <id> --team-id <team> --password <app-specific-pwd>
#
# Hardened runtime needs no entitlements: llama.cpp compiles its Metal shaders at run time from source embedded in the
# binary, which hardened runtime allows.
set -euo pipefail

target="${1:?usage: codesign.sh <target> <binary-path>}"
binary="${2:?usage: codesign.sh <target> <binary-path>}"
profile="${NOTARYTOOL_PROFILE:-notarytool}"

case "$target" in
  *darwin*)
    if [ -z "${MACOS_SIGN_IDENTITY:-}" ]; then
      echo "codesign: MACOS_SIGN_IDENTITY unset — skipping signing/notarization"
      exit 0
    fi

    command -v xcrun >/dev/null || {
      echo "codesign: xcrun missing; install Xcode Command Line Tools" >&2
      exit 1
    }
    xcrun notarytool --help >/dev/null 2>&1 || {
      echo "codesign: xcrun notarytool unavailable" >&2
      exit 1
    }

    # Fail fast if the configured keychain profile is unavailable.
    xcrun notarytool history --keychain-profile "$profile" >/dev/null

    codesign --force --options runtime --timestamp \
      --identifier dev.jwa91.sift \
      --sign "$MACOS_SIGN_IDENTITY" "$binary"
    echo "codesign: signed $binary"

    zip="$(mktemp -t sift-notarize.XXXXXX).zip"
    ditto -c -k --sequesterRsrc "$binary" "$zip"
    echo "notarytool: submitting $binary via $zip (profile: $profile)"
    xcrun notarytool submit "$zip" --keychain-profile "$profile" --wait
    rm -f "$zip"
    echo "notarytool: accepted $binary"
    ;;
  *)
    : # not macOS, nothing to do
    ;;
esac
