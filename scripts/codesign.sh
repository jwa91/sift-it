#!/usr/bin/env bash
# Sign + notarize the sift-it binary. Called by scripts/release.sh with the build target (a Rust triple:
# aarch64-apple-darwin) and the binary path.
#
# Everything comes from .env.template, resolved through 1Password at release time:
#   MACOS_SIGN_IDENTITY             "Developer ID Application: …": the certificate to sign with
#   MACOS_CERTIFICATE_P12_BASE64    optional: that certificate and its key, as base64 text; with it, signing needs nothing
#   MACOS_CERTIFICATE_PASSWORD      in this Mac's keychain (it goes into a temporary keychain, deleted afterwards)
#   NOTARY_APPLE_ID, NOTARY_TEAM_ID, NOTARY_PASSWORD
#                                   the notarization login (an app-specific password); without them, the keychain
#                                   profile "notarytool" (xcrun notarytool store-credentials) is used
#
# Hardened runtime needs no entitlements: llama.cpp compiles its Metal shaders at run time from source embedded in the
# binary, which hardened runtime allows.
set -euo pipefail

target="${1:?usage: codesign.sh <target> <binary-path>}"
binary="${2:?usage: codesign.sh <target> <binary-path>}"
profile="${NOTARYTOOL_PROFILE:-notarytool}"

case "$target" in
  *darwin*) ;;
  *) exit 0 ;; # not macOS, nothing to do
esac

if [ -z "${MACOS_SIGN_IDENTITY:-}" ]; then
  echo "codesign: MACOS_SIGN_IDENTITY unset — skipping signing/notarization"
  exit 0
fi
command -v xcrun >/dev/null || {
  echo "codesign: xcrun missing; install Xcode Command Line Tools" >&2
  exit 1
}

# The certificate: from 1Password into a temporary keychain, or already in this Mac's keychains.
keychain_args=()
if [ -n "${MACOS_CERTIFICATE_P12_BASE64:-}" ]; then
  scratch="$(mktemp -d -t sift-it-signing)"
  keychain="$scratch/signing.keychain-db"
  keychain_password="$(uuidgen)"
  original_list="$(security list-keychains -d user | tr -d '"' | xargs)"
  cleanup() {
    # shellcheck disable=SC2086
    security list-keychains -d user -s $original_list
    security delete-keychain "$keychain" 2>/dev/null || true
    rm -rf "$scratch"
  }
  trap cleanup EXIT
  printf '%s' "$MACOS_CERTIFICATE_P12_BASE64" | base64 --decode > "$scratch/certificate.p12"
  security create-keychain -p "$keychain_password" "$keychain"
  security set-keychain-settings -lut 3600 "$keychain"
  security unlock-keychain -p "$keychain_password" "$keychain"
  security import "$scratch/certificate.p12" -k "$keychain" -P "${MACOS_CERTIFICATE_PASSWORD:-}" -T /usr/bin/codesign
  security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_password" "$keychain" > /dev/null
  rm -f "$scratch/certificate.p12"
  # codesign looks up the identity in the search list: the temporary keychain joins it until we're done.
  # shellcheck disable=SC2086
  security list-keychains -d user -s "$keychain" $original_list
  keychain_args=(--keychain "$keychain")
elif ! security find-identity -v -p codesigning | grep -qF "$MACOS_SIGN_IDENTITY"; then
  echo "codesign: \"$MACOS_SIGN_IDENTITY\" isn't in this Mac's keychain." >&2
  echo "  Import its .p12 (from a Mac that has it, or Xcode → Settings → Accounts → Manage Certificates)," >&2
  echo "  or add MACOS_CERTIFICATE_P12_BASE64 and MACOS_CERTIFICATE_PASSWORD to .env.template." >&2
  exit 1
fi

# The notarization login: from 1Password, or the keychain profile.
if [ -n "${NOTARY_PASSWORD:-}" ]; then
  notary=(--apple-id "${NOTARY_APPLE_ID:?NOTARY_APPLE_ID unset}" --team-id "${NOTARY_TEAM_ID:?NOTARY_TEAM_ID unset}" \
    --password "$NOTARY_PASSWORD")
  notary_source="1Password"
else
  notary=(--keychain-profile "$profile")
  notary_source="keychain profile $profile"
  # Fail fast if the profile is missing, before anything is signed.
  xcrun notarytool history "${notary[@]}" >/dev/null
fi

codesign --force --options runtime --timestamp \
  --identifier dev.jwa91.sift-it \
  ${keychain_args[@]+"${keychain_args[@]}"} \
  --sign "$MACOS_SIGN_IDENTITY" "$binary"
echo "codesign: signed $binary"

zip="$(mktemp -t sift-it-notarize.XXXXXX).zip"
ditto -c -k --sequesterRsrc "$binary" "$zip"
echo "notarytool: submitting $binary via $zip ($notary_source)"
xcrun notarytool submit "$zip" "${notary[@]}" --wait
rm -f "$zip"
echo "notarytool: accepted $binary"
