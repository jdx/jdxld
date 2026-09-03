# Releasing jdxld

Releases use release-plz. Do not edit versions, changelogs, or tags by hand.

## Release flow

1. Changes merged to `main` cause `release-plz.yml` to open or update a release PR.
2. The release PR bumps the shared workspace version and updates `CHANGELOG.md`.
3. Merging that PR creates `v{version}` and a draft GitHub release. The workspace is in
   release-plz git-only mode, so no crates are published to crates.io.
4. `release.yml` builds `jdxld-aarch64-apple-darwin.tar.gz` from the tag with only the Mach-O
   feature enabled, signs and notarizes the binary, and attaches the archive and `SHA256SUMS`.
5. The release-plz workflow publishes the draft only after the asset workflow succeeds.

Asset names do not contain the version, which keeps the
`releases/latest/download/jdxld-aarch64-apple-darwin.tar.gz` URL stable for mr-boxington.

## Repository setup

Configure these Actions secrets before merging the first release PR:

- `RELEASE_PLZ_TOKEN`: a PAT with contents and pull-request write access. The default token cannot
  trigger CI for the generated release PR.
- `CERTIFICATES_P12` and `CERTIFICATES_P12_PASS`: the base64-encoded Developer ID Application
  certificate used by the other jdx.dev CLI releases and its password.
- `APPLE_API_KEY_P8`, `APPLE_API_KEY_ID`, and `APPLE_API_ISSUER_ID`: the base64-encoded App Store
  Connect team API key and identifiers used by `notarytool`.

Enable immutable releases in the repository settings before the first public release. The
workflow keeps the release drafted while assets are replaceable and publishes it only when the
archive is complete.

## Testing and recovery

Run the `release` workflow manually with `dry_run` enabled and `ref` set to a branch or commit. It
builds and smoke-tests the production archive, then retains the archive and checksums as workflow
artifacts without using signing secrets or changing a GitHub release.

If an automated release fails after creating its tag, fix the workflow and manually run `release`
with that tag and `dry_run` disabled. The workflow recreates a missing draft, replaces assets only
while the release is still a draft, and publishes it after a successful upload.
