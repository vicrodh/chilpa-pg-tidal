# Tidal for Chilpa

This update targets **Chilpa 0.1.3** and its matching plugin contracts.
Release and catalog publication are pending that host release.

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

Music > Playlist recommendations lists the playlists in the collection. Open
one to hear suggested tracks without changing the original playlist. This
read-only view keeps the provider's order, removes duplicate suggestions and
videos, and can supply tracks to the host's ordinary queue or playlist actions.
Each opening reads at most five pages / 500 source rows within an eight-second
network budget, using the existing five-minute catalog cache. If more suggestions
remain, the description states how many are shown. No recommendations are
requested until that playlist is opened.

Catalog cards use `/users/{id}/favorites/ids` for track, album, artist and
playlist membership, shared across concurrent reads. One account snapshot
retains at most 100,000 IDs and is valid for 60 seconds, then replaced on the
next read. It is fetched on demand without a worker or polling. The optional request has a two-second budget. A failed or
malformed snapshot leaves membership unknown and waits 15 seconds before
retrying. Explicit refresh, account changes, sign-out and mutations invalidate
the snapshot; the normal paginated library listings still supply full metadata.

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

Finished listens with at least 30 seconds of confirmed playback are sent as
`play_log` events through Tidal's event endpoint from a durable local queue
(`history.json`). Events include the token's account, client and session
identity, the actual resolved product and the album, playlist, mix, artist or
favorites origin when available. Only measured intervals count; resolving,
preloading, pausing and seeking add no listening time. The body and UUID are
saved before sending and stay unchanged across retries and restarts. An HTTP
success removes an event only when its individual entry is acknowledged.
`reporting.status` shows pending and delivered counts and the 30,000 ms
threshold. Account tokens are never copied into the listening journal.
The event envelope and transport headers describe the Android application
identity used by the provider sign-in, following the reviewed SONE snapshot;
these protocol fields do not describe the listener's physical computer.
This is a private provider endpoint: an ingestion acknowledgement does not
prove that Recently Played or royalty accounting has updated. A signed-in
listening check remains necessary. Older hosts without measured positions
keep local history only.

Provider AI labels on tracks and albums are returned as `ai_generated: true`
or `false`; an absent or malformed label remains unknown. Album labels are
not inferred onto individual tracks. A visible AI badge requires host support
for this shared field; this plugin does not alter titles or artwork to emulate
one. Structured main-artist roles take precedence over guest ordering while
keeping the artist name paired with its navigation reference.

## Build

```sh
python3 tools/build-native
python3 tools/smoke-native dist/0.2.1-x86_64-unknown-linux-gnu
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

`cargo test` runs the unit tests, the folder test (the process writes only in
the `data_dir` that `initialize` names and refuses to start without one) and
the FLAC decoder fixtures in `tests/fixtures`, which
`tools/make-flac-fixtures` regenerates with the reference FLAC decoder.
`tools/differential/` compares this plugin with the earlier Python plugin
against a local fake service: build with
`--features mock-endpoints` (never packaged), set `CHILPA_TIDAL_PYTHON` to an
interpreter with the Python plugin's requirements and
`CHILPA_TIDAL_PYTHON_SOURCE` to its `src` folder, then run
`python3 tools/differential/compare.py --rust <binary>`.

`python3 tools/check-reporting <binary>` exercises metadata and durable event
delivery against the local fake service, including an incorrect HTTP 200
acknowledgement, token refresh, restart and duplicate events. Build that test
binary with `--features mock-endpoints`; never package that feature. The
fixture also covers shared favorite membership, invalidation, malformed
responses, recommendations pagination, duplicate/video filtering and errors.
The historical Python differential harness expects the earlier behavior; the AI
field, contextual references, reporting status and event envelope intentionally
differ in this version.

## Pull request packages

GitHub Actions runs tests and packages this branch on pull requests and pushes
to main. Download the ZIP, SHA-256 and archive-check report from the successful
run's artifacts. These are test artifacts retained for 30 days, not releases;
the workflow does not publish a release or update a plugin catalog.

CI checks do not replace installation of the exact ZIP in a compatible native
and Flatpak host. A build does not establish live account playback or physical
audio output. Release validation remains separate.

CI packages Linux x86_64 on Debian 12 and macOS Apple Silicon.
