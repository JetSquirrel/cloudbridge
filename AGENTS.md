# CloudBridge — agent notes

## Local macOS release: build, sign, notarize, staple

The normal path is CI: push a `v*` tag, `.github/workflows/release.yml` builds
both platforms, signs and notarizes the macOS dmg, and publishes the GitHub
release. Reach for the local path below when CI is blocked rather than broken
— GitHub's macOS runner queue and Apple's notary queue have each stalled for
hours in practice, and neither is fixable from this repository.

`scripts/package-macos.sh` does the packaging itself and refuses to run
without credentials; the local path is the same script, run by hand, with no
job time limit.

### Prerequisites — already true on the maintainer's Mac

| What | Where |
|---|---|
| Signing identity | `Developer ID Application: Tian Deng (V5KP6ZYMDT)`, login keychain |
| Notary API key | `~/Downloads/AuthKey_492MX6SWPC.p8` |
| Key ID and Issuer ID | `scripts/notary.local` (gitignored, see below) |

Confirm the identity and its private key are present before anything else:

```bash
security find-identity -v -p codesigning | grep "Developer ID"
```

Exactly one identity must print. An `Apple Development` certificate cannot
sign for distribution. If the identity is missing, the certificate has to be
created in the Apple Developer portal — no script can do that step.

`scripts/notary.local` holds the three values the script needs:

```bash
export NOTARY_KEY=~/Downloads/AuthKey_492MX6SWPC.p8
export NOTARY_KEY_ID=492MX6SWPC
export NOTARY_ISSUER=<issuer UUID from App Store Connect -> Users and Access -> Integrations -> Keys>
```

The Key ID is not a secret (it is in the key's filename); the `.p8` is, and
stays where it is.

### Steps

1. **Check disk space.** A release build of this project needs roughly 10 GB
   beyond what the machine already uses:

   ```bash
   df -h /System/Volumes/Data   # want > 15Gi available
   ```

   The usual culprits are this and sibling Rust projects' `target/`
   directories. On a full disk the failure is not a clean "out of space" —
   `libduckdb-sys` dies inside `ar cq` with no explanation.

2. **Build from a worktree of the tag, never from the working tree.** The
   maintainer usually has uncommitted work, and it must not end up in a
   release:

   ```bash
   git worktree add /tmp/cloudbridge-vX.Y.Z vX.Y.Z
   cd /tmp/cloudbridge-vX.Y.Z && cargo build --release
   ```

3. **Package with the script from `main`**, invoked by path so a tag that
   predates a packaging fix still gets the fix:

   ```bash
   source /Users/admin/side-proj/cloudbridge/scripts/notary.local
   cd /tmp/cloudbridge-vX.Y.Z
   /Users/admin/side-proj/cloudbridge/scripts/package-macos.sh \
     target/release/cloudbridge cloudbridge-macos-arm64.dmg X.Y.Z
   ```

   The script signs the app and the dmg, submits the dmg for notarization,
   waits, staples, then runs `stapler validate` and two `spctl` assessments.
   It must be run from the repository root — it reads `assets/` and `themes/`
   from the working directory.

   macOS will raise a GUI keychain prompt the first time `codesign` uses the
   key. Someone has to click **Always Allow**; an agent cannot.

4. **Expect to wait, and know that waiting is not failure.** Apple's notary
   service has taken anywhere from minutes to over 12 hours per submission
   for this team. Two facts follow:

   - The 5-hour `--timeout` in the script is a client-side wait, not a
     cancellation. When it expires the submission keeps processing on Apple's
     side, and the ticket can still be collected later.
   - **A ticket binds to the exact bytes submitted.** Do not rebuild after a
     timeout — a rebuild re-signs with a fresh timestamp, producing different
     bytes and no ticket. Staple the file that was submitted:

     ```bash
     xcrun notarytool history --key "$NOTARY_KEY" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER"
     xcrun notarytool info <submission-id> --key "$NOTARY_KEY" --key-id "$NOTARY_KEY_ID" --issuer "$NOTARY_ISSUER"
     # once the dmg's submission reads Accepted:
     xcrun stapler staple cloudbridge-macos-arm64.dmg
     ```

5. **Verify the artifact as a user's Mac would**, not just as the script
   does:

   ```bash
   xcrun stapler validate cloudbridge-macos-arm64.dmg
   spctl --assess --type open --context context:primary-signature --verbose=2 cloudbridge-macos-arm64.dmg
   MNT=$(hdiutil attach -nobrowse -readonly cloudbridge-macos-arm64.dmg | grep -o '/Volumes/.*' | head -1)
   spctl --assess --type execute --verbose=2 "$MNT/CloudBridge.app"
   hdiutil detach "$MNT" -quiet
   ```

   All four must report `accepted` with `source=Notarized Developer ID`.

6. **Publish.** Pushing a tag runs CI, which builds and publishes the
   release; creating a tag through the GitHub release UI also triggers that
   workflow, so a hand-uploaded asset gets overwritten by CI's. Either push
   the tag and let CI own the release, or upload by hand and accept that CI
   will refresh the assets.

### Failure modes seen in practice

| Symptom | Cause |
|---|---|
| `...: rejected  source=no usable signature` on the dmg, app accepted | The dmg itself was never signed. Signing the app inside is not enough; `spctl`'s open assessment reads the dmg's own signature |
| `error: -25294` importing the `.cer` | The keychain dropdown in the import dialog was left on "Local Items"; pick "login" |
| `.p12` greyed out in the export dialog | Only the certificate was selected. Expand it and select certificate **and** private key |
| `security export: The contents of this item cannot be retrieved` | `security export` without a filter exports every identity and aborts if any one fails. Export the single identity from the GUI instead |
| `ar: ... libduckdb.a` failing with no message | Disk full |
| Notarization `In Progress` for hours | Apple's queue, not a configuration problem. Verified by `notarytool history` showing sibling submissions Accepted |

## The web demo (wasm32)

The same crate builds a browser version:

```bash
./scripts/build-web.sh              # debug; add --release for the shipped one
python3 -m http.server 8000 --directory www
```

`src/lib.rs` chooses a data backend per target with `cfg`: DuckDB, the provider
APIs and the OS keyring on the desktop; an in-memory ledger seeded with demo
data in the browser (`src/web/`). Everything above that — `src/ui/`, `app.rs`,
`alerts.rs`, `model.rs` — is shared and compiles for both, so **a change to a
page must keep building for wasm**, not just for the desktop.

`.github/workflows/ci.yml` includes advisory wasm and MSRV checks. The wasm
job skips when the web build script, bridge manifest or fonts directory is
absent from the checkout. Once the web sources are tracked and CI is verified,
remove its `continue-on-error` to enforce the shared-code build requirement.
The MSRV job reads `package.rust-version` from `Cargo.toml` (currently 1.95).
The locked `gpui-pre` dependency uses `std::hint::cold_path`, stabilized in
Rust 1.95; `cargo +1.94.0 check --locked` fails on this API on macOS ARM64.
Keep the job non-blocking until the Ubuntu CI check is verified; local macOS
validation does not establish compatibility on other platforms. Revalidate
the minimum when updating `Cargo.lock`. The wasm build still needs nightly.

Things that will bite you:

- **Nightly is required for the wasm build only.** `gpui-pre-web` pulls Zed's
  `wasm_thread`, which uses a `stdarch_wasm_atomic_wait` feature stable does not
  have. The desktop stays on stable.
- **`wasm-bindgen` CLI must match the `wasm-bindgen` crate version** — read it
  out of `Cargo.lock` and `cargo install wasm-bindgen-cli --version <that>`.
- **The browser has no system fonts.** `gpui-pre-web` starts with an empty font
  database, and GPUI resolves `.SystemUIFont` to IBM Plex Sans there; without
  that family registered the text system panics rather than falling back. The
  four fonts are in `www/fonts/` (OFL) and registered in `src/wasm_entry.rs`;
  they are `include_bytes!`d, so they must exist to compile.
- **Icons are fetched, not embedded.** gpui-kit's wasm asset source requests
  `<endpoint>/assets/icons/<name>.svg` on demand, so the build script copies the
  catalog out of the resolved `gpui-kit-assets` crate into `www/assets/`
  (gitignored).
- **`smol::unblock` is forwarded by `web/smol-bridge`.** Cargo refuses one
  dependency name with two sources, so the desktop's `smol` is reached through
  that crate instead of beside it. The pages call `smol::unblock` unchanged; on
  wasm it runs the closure inline, because there is no second thread.
- **`cargo build --release` at the root still means the desktop application.**
  `[workspace] default-members = ["."]` keeps it that way; the wasm build is
  `--lib --target wasm32-unknown-unknown`. A release wasm build is heavy and the
  disk advice above applies to it too.
