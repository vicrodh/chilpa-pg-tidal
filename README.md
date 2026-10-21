# Tidal for Chilpa

This third-party plugin is maintained by [vicrodh](https://github.com/vicrodh)
independently of Tidal. It is a single native executable that Chilpa runs as a
process plugin; it needs no Python runtime and no LMS server.

## Install

Download the ZIP for your platform from Releases and use Settings > Plugins >
Install from ZIP, or add the third-party catalog in Settings > Plugins:

```text
https://raw.githubusercontent.com/vicrodh/chilpa-3rd-party-plugins/main/index.json
```

Review the requested permissions before enabling it. A Tidal account with a
subscription is required for full playback. Version 0.2.0 replaces the earlier
Python package under the same plugin id and reads its private data as it is:
an account that was signed in stays signed in.

## Signing in

Open the plugin's settings. Two ways are offered:

- **In the browser (FLAC and Max).** Select Open Tidal sign-in, sign in and
  approve access. A Chilpa that has its sign-in window (Linux with WebKitGTK)
  opens the page there, stops at Tidal's return address and finishes the
  sign-in by itself. Otherwise the page opens in the browser, and Tidal then
  shows a page that may say Oops or stay blank; its address carries the
  sign-in code. Copy that whole address from the address
  bar, paste it in the settings page and select Finish sign-in. The page
  explains which address it needs when a different one is pasted, and a bare
  code is accepted too. Passwords stay in the browser.
- **With a code (AAC only).** Select Sign in with a code instead, open
  link.tidal.com with the code already filled in, approve access, and select Check
  sign-in. Nothing is copied, but Tidal limits this kind of sign-in to AAC
  320 kbps.

The browser sign-in uses PKCE with the same public application identity as
python-tidal; the code sign-in uses its device identity. Tokens are stored in
`account.json` in the plugin's private data.

## Features

Discovery (For You with its full feed, Staff Picks, New, Top, Explore, Hi-Res,
My Mixes, genres, moods and decades), the collection (albums, tracks, artists,
playlists with native folders), search, album, artist and label pages, mixes
and radios, playlist editing and favorites, lyrics (`track.lyrics`, line-timed
when available), "Open music link", and playback.

Choose Max, FLAC or AAC in the settings. The actual resolution comes from each
stream. For clear FLAC DASH, the plugin serves the original integer samples as
a ranged virtual WAV on 127.0.0.1; decoded fragments stay in bounded memory
(64 MiB per stream, the 8 most recent streams) and the next fragment is read
while the current one plays. Provider normalization uses Chilpa's single gain
stage. Casting uses Chilpa's receiver proxy, not a provider Connect
implementation. DRM-protected streams are not decrypted.

Saved tracks carry composers from album credits when available, and labels
and genres from the album page or the copyright line. Each listing waits at
most six seconds for that optional metadata, with at most eight albums and
eight requests shared by credits and genre pages; the answers are cached in
`credits.json` and `library-enrichment.json` (found for 90 days, empty for
seven). `library-enrichment.json` also holds a per-account diagnostic of the
last listing, without tokens, item ids or names.

Finished listens are sent as `play_log` events through Tidal's event endpoint
from a durable local queue (`history.json`), with retries, once the host
reports positions; `reporting.status` shows pending and delivered counts.

## Build

```sh
python3 tools/build-native
python3 tools/smoke-native dist/0.2.0-x86_64-unknown-linux-gnu
```

The published Linux package is built on Debian 12, so it needs glibc 2.36 or
newer, not the glibc of the build machine:

```sh
docker build -f tools/package-linux.Dockerfile -t chilpa-tidal-builder .
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp -e CARGO_HOME=/tmp/cargo \
  -e CARGO_TARGET_DIR=/tmp/target -v "$PWD:/source" chilpa-tidal-builder \
  sh -c 'cargo fetch --locked && python3 tools/build-native'
```

`build-native` builds the release executable for the host target and writes
`dist/<version>-<target>.zip`. `smoke-native` runs the packaged process in a
temporary profile inside Bubblewrap (or with `--direct`, as a plain process)
and checks the settings page and the unauthenticated errors; it does not sign
in, reach the network or play audio.

`cargo test` runs the unit tests and the FLAC decoder fixtures in
`tests/fixtures`, which `tools/make-flac-fixtures` regenerates with the
reference FLAC decoder. `tools/differential/` compares this plugin with the
earlier Python plugin against a local fake service: build with
`--features mock-endpoints` (never packaged), set `CHILPA_TIDAL_PYTHON` to an
interpreter with the Python plugin's requirements and
`CHILPA_TIDAL_PYTHON_SOURCE` to its `src` folder, then run
`python3 tools/differential/compare.py --rust <binary>`.
