# Tauri updater and release signing

This document explains how CPU Manager ships signed updater artifacts and a
checksum file alongside every GitHub release.

## Overview

`tauri-plugin-updater` is registered on the Rust side (`src-tauri/src/lib.rs`)
and the `updater:default` permission is granted in
`src-tauri/capabilities/default.json`. The updater endpoint and public key are
configured in `src-tauri/tauri.conf.json` under `plugins.updater`.

The updater client (when invoked from the frontend) fetches
`latest.json` from the configured endpoint, verifies each entry's minisign
signature against the embedded public key, then downloads and applies the
matching `.nsis.zip`. The frontend does not currently call the updater API;
the release pipeline still emits the signed artifacts so a future in-app
update button can opt in without rebuilding the release tooling.

## Key generation

The repository ships a public key at `src-tauri/tauri.pubkey`. The matching
private key (`src-tauri/tauri.key`) is **never** committed - it is listed in
`src-tauri/.gitignore`.

To rotate the keypair (e.g. before the first public release, or after a key
is suspected of being leaked):

```powershell
# From the repository root, with the dev toolchain installed.
npx tauri signer generate -p "your-strong-password" --ci --force -w src-tauri/tauri.key
```

A non-empty password is **required** because GitHub Actions does not allow
empty secrets, and `tauri build` cannot decrypt the keystore when the
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` environment variable is empty or unset
(it silently skips signing instead of erroring, so the build looks
successful while no updater artifacts are produced). Store the same
password in the `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` GitHub Actions secret.

After regenerating, replace `src-tauri/tauri.pubkey` with the contents of
the newly generated `tauri.key.pub`, update the `plugins.updater.pubkey`
field in `src-tauri/tauri.conf.json` to match, and copy the corresponding
`tauri.key` into the `TAURI_SIGNING_PRIVATE_KEY` GitHub Actions secret.

## CI secrets

| Secret | Required | Purpose |
|--------|----------|---------|
| `TAURI_SIGNING_PRIVATE_KEY` | Recommended | The full text of `tauri.key`. The build job passes it to `tauri build`, which signs the updater artifacts. |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | **Required** | Password the keystore was generated with. GitHub does not allow empty secrets, so a non-empty password is mandatory. |

When `TAURI_SIGNING_PRIVATE_KEY` is unset, the build still succeeds and the
NSIS installer is published, but no updater artifacts (`.nsis.zip`, `.sig`,
`latest.json`) are produced. The release job detects the missing artifacts
and publishes only the installers.

When the private key is set but the password is wrong or missing, the
Tauri bundler cannot decrypt the keystore and silently skips signing
without failing the build - the symptom looks identical to "secret not
configured". The "No updater artifacts" line in the build log is the
canonical signal. Always set both secrets.

To set the secrets:

1. Run `Get-Content src-tauri/tauri.key` locally and copy the output.
2. On GitHub, go to **Settings &rarr; Secrets and variables &rarr; Actions
   &rarr; New repository secret**.
3. Name it `TAURI_SIGNING_PRIVATE_KEY` and paste the value. (For multi-line
   values, GitHub accepts the literal newlines; the YAML job exposes it as
   `secrets.TAURI_SIGNING_PRIVATE_KEY` unchanged.)
4. Repeat with `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` and the keystore
   password.

## Release artifacts

After the existing build matrix finishes, the release job produces the
following files in the GitHub release:

| File | Description |
|------|-------------|
| `CPU-Manager_<tag>_windows-x64-setup.exe` | NSIS installer (x86_64), optionally Authenticode-signed by SignPath. |
| `CPU-Manager_<tag>_windows-arm64-setup.exe` | NSIS installer (aarch64), optionally Authenticode-signed by SignPath. |
| `CPU-Manager_<tag>_windows-x64.nsis.zip` | Zipped installer used by the in-app updater (x86_64). |
| `CPU-Manager_<tag>_windows-x64.nsis.zip.sig` | Minisign signature of the x86_64 zip. |
| `CPU-Manager_<tag>_windows-arm64.nsis.zip` | Zipped installer used by the in-app updater (aarch64). |
| `CPU-Manager_<tag>_windows-arm64.nsis.zip.sig` | Minisign signature of the aarch64 zip. |
| `latest.json` | Combined update manifest with one entry per Windows architecture. |
| `SHA256SUMS.txt` | SHA-256 digests of every file above, `sha256sum -c` compatible (LF line endings, no BOM). |

The `latest.json` payload looks like:

```json
{
  "version": "0.1.0",
  "pub_date": "2026-09-11T11:21:39.000Z",
  "platforms": {
    "windows-x86_64": {
      "signature": "<minisign signature>",
      "url": "CPU-Manager_v0.1.0_windows-x64.nsis.zip"
    },
    "windows-aarch64": {
      "signature": "<minisign signature>",
      "url": "CPU-Manager_v0.1.0_windows-arm64.nsis.zip"
    }
  }
}
```

The URL is relative to the endpoint URL (`plugins.updater.endpoints` in
`tauri.conf.json`). With the current configuration the client resolves the
zip URLs against
`https://github.com/yixinin/cpum/releases/latest/download/`, which GitHub
serves without authentication.

## Local builds

`build.bat` mirrors the CI pipeline. If `src-tauri/tauri.key` is present on
the build machine, the resulting NSIS bundle directory contains the signed
`.nsis.zip`, the `.sig` files, and `latest.json`. The script then copies
them to a top-level `release/` directory alongside the installer and writes
`SHA256SUMS.txt` using the same format the release job uses.

To verify the local checksum file:

```powershell
# Windows (built-in certutil)
certutil -hashfile release\CPU-Manager_local_windows-x64-setup.exe SHA256

# Cross-platform (Git for Windows, WSL, macOS, Linux)
cd release && sha256sum -c SHA256SUMS.txt
```

## Verifying an existing release

After downloading `SHA256SUMS.txt` and the artifacts of a GitHub release:

```bash
sha256sum -c SHA256SUMS.txt
```

To inspect a single `.sig` file manually:

```bash
# `minisign` is the reference implementation Tauri uses. On Windows it is
# available via `winget install jedisct1.minisign`.
minisign -V -P RWR9f2sLThYL+G8XW2/Q5SDT1GF+jUTHTgZAtSY55Gh0aWSp4ldk30O \
  -m CPU-Manager_v0.1.0_windows-x64.nsis.zip \
  -x CPU-Manager_v0.1.0_windows-x64.nsis.zip.sig
```

The `-P` argument is the public key string that lives in
`src-tauri/tauri.pubkey` (strip the `untrusted comment:` line first).

## Troubleshooting

- **No updater artifacts in the release.** The build job did not have access
  to `TAURI_SIGNING_PRIVATE_KEY`. Add the secret, push a new tag.
- **`latest.json` reports an older version than the release.** The manifest
  is uploaded to `releases/latest/download/latest.json` only after the
  release is published. Wait a few minutes after publishing for GitHub's
  `latest` redirect to settle, then have the client re-check.
- **Client reports `Invalid signature`.** The client is using a binary built
  with a different `tauri.pubkey` than the one in this repository. Rebuild
  the desktop binary from this checkout and republish.
- **SignPath step fails but the updater pipeline still completes.** SignPath
  signs the `.exe`; Tauri separately signs the updater zips. The two are
  independent, so a SignPath outage does not block updater shipping.
