# Tidal for Chilpa

I publish this third-party plugin as [vicrodh](https://github.com/vicrodh).
It is independent of Tidal and separate from Chilpa's official plugin catalog.
The source and release downloads live in
[chilpa-pg-tidal](https://github.com/vicrodh/chilpa-pg-tidal).

## Install

I publish Linux x86_64 packages for Chilpa 0.1.0-beta. I add this catalog URL
in Settings > Plugins to install Tidal alongside the official plugins:

```text
https://raw.githubusercontent.com/vicrodh/chilpa-3rd-party-plugins/main/index.json
```

Alternatively, I download the ZIP from
[Releases](https://github.com/vicrodh/chilpa-pg-tidal/releases) and use
Settings > Plugins > Install from ZIP. I review permissions before enabling it.
A Tidal account with a subscription is required for full playback.

## Features and account data

I use Chilpa's native process protocol for discovery, library, search, album and
artist navigation, playlist editing and playback. The ZIP contains its runtime;
I do not require a Python installation or an LMS server on the listener's system.

I install the ZIP from Settings → Plugins → Install from ZIP, review permissions,
enable Tidal and open its settings. I sign in in the browser and paste the final
Tidal return address into the masked field. A Tidal Oops page can still carry the
successful authorization code in its address. I never enter my password in Chilpa.

I choose Max, FLAC or AAC in plugin settings. Actual audio resolution is taken
from the stream, not inferred from the requested quality. Provider normalization
uses Chilpa's existing single gain stage. For clear FLAC DASH, I serve the original
integer samples as a ranged virtual WAV; temporary decoded fragments remain only
in bounded memory. Seek and preloading use the same ranged reader as other sources.
Casting uses Chilpa's receiver proxy, not a provider Connect implementation.

I answer `track.lyrics` for my own tracks: line-timed lyrics when Tidal has
them, plain text otherwise, with the track's length and ISRC. A track without
lyrics answers "none"; a limited or failed request stays an error, with the
wait Tidal asked for, so Chilpa asks again later.

Collection releases can be included in Albums from its filters or plugin settings.
Playlists use the host's integrated or separate sidebar preference. Disabling or
uninstalling uses Chilpa's plugin management, including account-data removal.

I expose Music with For You and Staff Picks, plus Explore, Hi-Res, My Mixes,
genre and mood pages. I follow the personalized feed cursor and preserve
context headers and versioned View all links. A mixed recommendation module
stays one customizable shelf, preserving its artist, playlist and album order.
I retain previous section IDs so existing visibility and size choices carry over. Shelves
use the host's ordering and visibility controls. I preserve source references
on artist and album links. Playlist writes check ownership and version and use
occurrence positions, preserving duplicate songs.

## Limits to verify with an account

I do not implement Connect or an offline music library. DRM-protected streams
are not decrypted. I send measured `play_log` events through Tidal's event endpoint, using durable
retries and checking each event acknowledgement. `reporting.status` exposes pending
and delivered counts. Resolving or preloading does not create a listen. Delivery
requires host checkpoints with `position_ms` and `observed_at_ms`; older hosts retain
local history only. I do not backfill earlier history. Recently Played needs a
signed-in end-to-end check. Label navigation restricts catalog search to exact
label matches because there is no dedicated label listing.

The fixture tests cover contracts, authentication state, errors, pagination,
playlist writes and measured history. Account entitlement, live discovery content
and physical receiver playback require a signed-in listening test.

## Build

I build the published Linux package on Debian 12 (glibc 2.36) with the exact
dependency versions in `requirements-lock.txt`. I run these commands from
the repository root with Docker:

```sh
docker build -f tools/package-linux.Dockerfile -t chilpa-tidal-builder .
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
  -v "$PWD:/source" -w /source chilpa-tidal-builder \
  sh -c 'PYTHONPATH=src python -m unittest discover -s tests -v && python tools/build-native'
python tools/smoke-native dist/0.1.18-x86_64-unknown-linux-gnu
```

I include source code, licenses and the dependency list in the installable ZIP.
The Linux process smoke uses Bubblewrap and a temporary profile. It does not
sign in, open a browser, play audio or modify an existing account.

I create a virtual environment, install `requirements-dev.txt`, then run
`python tools/build-native` from that environment on each target OS and
architecture. I set `CHILPA_FLAC_LIBRARY` to the absolute path of that target's
libFLAC shared library if the script cannot find it. The build bundles libFLAC
and its linked libraries; the listener does not install development dependencies.
The package contains `plugin.exe` on Windows or `plugin` on Unix. macOS and
Windows packages require matching Chilpa process-plugin sandbox and installer
support before they can be installed and run.

I run `PYTHONPATH=src python -m unittest discover -s tests -v` for the fixture suite.
The process accepts `--data-dir PATH` for an isolated test profile. Without
that argument it uses `CHILPA_PLUGIN_DATA_DIR` when set, or `/data` in the
Linux sandbox.
On macOS I run `python tools/smoke-native dist/<version>-aarch64-apple-darwin --direct`
to check the packaged JSON-RPC process with temporary data. Direct mode does
not validate a Chilpa host sandbox.
