# Releasing sift-it

A release is built, signed and notarized on a Mac, then published from it: `make release VERSION=X.Y.Z`. The repo
holds no credentials and no references to any. Signing and notarizing use what the Mac's keychain holds, as Apple
intends, and the GitHub release and the cask use your `gh` login. Each Mac you release from is set up once.

## Setting up a Mac (once)

### 1. A Developer ID Application certificate

`codesign` needs the certificate and its private key in the login keychain. If another Mac has them, move them over:

1. On that Mac, open Keychain Access → login → My Certificates.
2. Right-click "Developer ID Application: <your name> (<team ID>)" → Export, as a `.p12` with a password.
3. On this Mac, double-click the `.p12` (or `security import <file>.p12 -k ~/Library/Keychains/login.keychain-db -T /usr/bin/codesign`).
4. Delete the `.p12`.

If no Mac has one, make a new one: Xcode → Settings → Accounts → your team → Manage Certificates → + → Developer ID
Application. Only the team's Account Holder can, and Apple allows a few per team, so reuse one where you can.

Check it:

```bash
security find-identity -v -p codesigning
```

It should list `"Developer ID Application: …"`. If the certificate is in the keychain but missing here, the Developer
ID intermediate certificate is missing too: Xcode installs it, or get "Developer ID - G2" from
<https://www.apple.com/certificateauthority/>.

With several Developer ID Application certificates, name the one to use in `SIFT_IT_SIGN_IDENTITY` (its name or SHA-1).

### 2. A notarytool profile

notarytool keeps its login in the keychain as a named profile. sift-it uses the one called `notarytool`:

```bash
xcrun notarytool store-credentials notarytool --apple-id <your Apple ID> --team-id <team ID>
```

It asks for an app-specific password: make one at <https://account.apple.com> → Sign-In and Security →
App-Specific Passwords. The team ID is the one in the certificate's name. An App Store Connect API key works too
(`--key <AuthKey.p8> --key-id <id> --issuer <issuer>`). If these live in your password manager, pass them from it in
your own shell; they're stored in the keychain, not in the repo.

To use a profile with another name, set `SIFT_IT_NOTARY_PROFILE`.

### 3. gh

```bash
gh auth login
```

The login needs the `repo` scope and push access to jwa91/sift-it and jwa91/homebrew-tap.

### 4. Check

```bash
scripts/codesign.sh --check
```

`make release` runs the same check before it builds anything.

## Releasing

1. Set the version in `Cargo.toml`, move the changelog's Unreleased entries under it, and commit.
2. Tag the commit `vX.Y.Z` and push both: `git push origin main --follow-tags`.
3. `make release VERSION=X.Y.Z`.

It checks that the version matches `Cargo.toml`, that gh is logged in, that this Mac can sign and notarize, and that
the tag points at HEAD. Then `scripts/release.sh`:

- builds sift-it;
- signs it (hardened runtime) and notarizes it;
- makes `sift-it_X.Y.Z_darwin_arm64.tar.gz` and `checksums.txt`;
- creates the GitHub release;
- commits `Casks/sift-it.rb` to jwa91/homebrew-tap through the API.

`make snapshot` makes the same files in `dist/`, unsigned, and publishes nothing; CI's release workflow runs it.
