#!/usr/bin/env bash
# Signs and notarizes the sift-it binary with what this Mac's keychain holds, as Apple intends:
#   - a "Developer ID Application" certificate with its private key. With exactly one, that's the one; with several,
#     SIFT_IT_SIGN_IDENTITY names it (its name or SHA-1).
#   - a notarytool keychain profile: "notarytool", or the one SIFT_IT_NOTARY_PROFILE names.
# A Mac you release from needs both, once. The repo holds no credentials and no references to any.
#
#   scripts/codesign.sh <binary>    sign and notarize it
#   scripts/codesign.sh --check     check that this Mac can (make release runs it before building)
#
# Hardened runtime needs no entitlements: llama.cpp compiles its Metal shaders at run time from source embedded in the
# binary, which hardened runtime allows.
set -euo pipefail

profile="${SIFT_IT_NOTARY_PROFILE:-notarytool}"

# The certificate to sign with: its SHA-1, or what SIFT_IT_SIGN_IDENTITY says.
identity() {
  if [ -n "${SIFT_IT_SIGN_IDENTITY:-}" ]; then
    echo "$SIFT_IT_SIGN_IDENTITY"
    return
  fi
  local found count
  found="$(security find-identity -v -p codesigning | grep '"Developer ID Application' || true)"
  count="$(printf '%s' "$found" | grep -c . || true)"
  case "$count" in
    0)
      echo "codesign: no Developer ID Application certificate (with its private key) in this Mac's keychain" >&2
      return 1
      ;;
    1) printf '%s\n' "$found" | awk '{print $2}' ;;
    *)
      echo "codesign: several Developer ID Application certificates; name one in SIFT_IT_SIGN_IDENTITY:" >&2
      printf '%s\n' "$found" >&2
      return 1
      ;;
  esac
}

check() {
  command -v xcrun > /dev/null || {
    echo "codesign: xcrun missing; install Xcode's command line tools" >&2
    return 1
  }
  identity > /dev/null
  xcrun notarytool history --keychain-profile "$profile" > /dev/null 2>&1 || {
    echo "codesign: no working notarytool keychain profile \"$profile\"" \
      "(xcrun notarytool store-credentials $profile)" >&2
    return 1
  }
}

if [ "${1:-}" = "--check" ]; then
  check
  echo "codesign: this Mac can sign and notarize"
  exit 0
fi

binary="${1:?usage: scripts/codesign.sh <binary> | --check}"
check
codesign --force --options runtime --timestamp --identifier dev.jwa91.sift-it --sign "$(identity)" "$binary"
echo "codesign: signed $binary"

zip="$(mktemp -t sift-it-notarize.XXXXXX).zip"
ditto -c -k --sequesterRsrc "$binary" "$zip"
echo "notarytool: submitting $binary (profile $profile)"
xcrun notarytool submit "$zip" --keychain-profile "$profile" --wait
rm -f "$zip"
echo "notarytool: accepted $binary"
