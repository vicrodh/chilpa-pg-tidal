# Tidal for Chilpa

This third-party plugin is maintained by [vicrodh](https://github.com/vicrodh)
independently of Tidal. Chilpa's official catalog also lists it as a
third-party plugin validated by Chilpa.
The source and release downloads live in
[chilpa-pg-tidal](https://github.com/vicrodh/chilpa-pg-tidal).

## Install

Linux x86_64 and macOS Apple Silicon packages target Chilpa 0.1.0-beta.
The macOS package requires the updated Chilpa host with its native worker
sandbox; earlier macOS builds cannot launch process plugins. Add this catalog URL
in Settings > Plugins to install Tidal alongside the official plugins:

```text
https://raw.githubusercontent.com/vicrodh/chilpa-3rd-party-plugins/main/index.json
```

Alternatively, download the ZIP from
[Releases](https://github.com/vicrodh/chilpa-pg-tidal/releases) and use
Settings > Plugins > Install from ZIP. Review permissions before enabling it.
A Tidal account with a subscription is required for full playback.

## Features and account data

Chilpa's native process protocol supports discovery, library, search, album and
artist navigation, playlist editing and playback. The ZIP includes its runtime;
no separate Python installation or LMS server is required.

After installation and permission approval, enable Tidal and open its settings.
Sign in in the browser and paste the final Tidal return address into the masked field. A Tidal Oops page can still carry the
successful authorization code in its address. The password is entered only in the browser.

Choose Max, FLAC or AAC in plugin settings. Actual audio resolution is taken
from the stream, not inferred from the requested quality. Provider normalization
uses Chilpa's existing single gain stage. For clear FLAC DASH, the plugin serves the original
integer samples as a ranged virtual WAV; temporary decoded fragments remain only
in bounded memory. Seek and preloading use the same ranged reader as other sources.
Casting uses Chilpa's receiver proxy, not a provider Connect implementation.

`track.lyrics` returns lyrics for this source's own tracks: line-timed lyrics when Tidal has
them, plain text otherwise, with the track's length and ISRC. A track without
lyrics answers "none"; a limited or failed request stays an error, with the
wait Tidal asked for, so Chilpa asks again later.

Collection releases can be included in Albums from its filters or plugin settings.
Playlists use the host's integrated or separate sidebar preference. Disabling or
uninstalling uses Chilpa's plugin management, including account-data removal.

Music includes For You and Staff Picks, plus Explore, Hi-Res, My Mixes,
genre and mood pages. The plugin follows the personalized feed cursor and
preserves context headers and versioned View all links. A mixed recommendation module
stays one customizable shelf, preserving its artist, playlist and album order.
Previous section IDs retain existing visibility and size choices. Shelves
use the host's ordering and visibility controls. Artist and album links preserve source references. Playlist writes check ownership and version and use
occurrence positions, preserving duplicate songs.

## Limits to verify with an account

Connect and an offline music library are not implemented. DRM-protected
streams are not decrypted. The plugin sends measured `play_log` events
through Tidal's event endpoint, using durable
retries and checking each event acknowledgement. `reporting.status` exposes pending
and delivered counts. Resolving or preloading does not create a listen. Delivery
requires host checkpoints with `position_ms` and `observed_at_ms`; older hosts retain
local history only. Earlier history is not backfilled. Recently Played needs a
signed-in end-to-end check. Label navigation restricts catalog search to exact
label matches because there is no dedicated label listing.

The fixture tests cover contracts, authentication state, errors, pagination,
playlist writes and measured history. Account entitlement, live discovery content
and physical receiver playback require a signed-in listening test.

## Build

The published Linux package is built on Debian 12 (glibc 2.36) with the exact
dependency versions in `requirements-lock.txt`. Run these commands from
the repository root with Docker:

```sh
docker build -f tools/package-linux.Dockerfile -t chilpa-tidal-builder .
docker run --rm --user "$(id -u):$(id -g)" -e HOME=/tmp \
  -v "$PWD:/source" -w /source chilpa-tidal-builder \
  sh -c 'PYTHONPATH=src python -m unittest discover -s tests -v && python tools/build-native'
python tools/smoke-native dist/0.1.18-x86_64-unknown-linux-gnu
```

The installable ZIP includes source code, licenses and the dependency list.
The Linux process smoke uses Bubblewrap and a temporary profile. It does not
sign in, open a browser, play audio or modify an existing account.

Create a virtual environment, install `requirements-dev.txt`, then run
`python tools/build-native` from that environment on each target OS and
architecture. Set `CHILPA_FLAC_LIBRARY` to the absolute path of that target's
libFLAC shared library if the script cannot find it. The build bundles libFLAC
and its linked libraries; the listener does not install development dependencies.
The package contains `plugin.exe` on Windows or `plugin` on Unix. The macOS
package runs inside Chilpa's Seatbelt worker sandbox. Windows process hosting
is not implemented.

Run `PYTHONPATH=src python -m unittest discover -s tests -v` for the fixture suite.
The process accepts `--data-dir PATH` for an isolated test profile. Without
that argument it uses `CHILPA_PLUGIN_DATA_DIR` when set, or `/data` in the
Linux sandbox.
On macOS, run `python tools/smoke-native dist/<version>-aarch64-apple-darwin --direct`
to check the packaged JSON-RPC process with temporary data. Direct mode does
not validate a Chilpa host sandbox.

### macOS Apple Silicon

The Mac package uses portable CPython 3.13.16 from
[python-build-standalone 20261013](https://github.com/astral-sh/python-build-standalone/releases/tag/20261013)
and the locked dependencies. Set `CHILPA_PYTHON_NOTICES` to the license
directory from the matching full interpreter archive, including its
`PYTHON.json`, so the transitive runtime notices travel with the ZIP. Compile
libFLAC 1.5.0 from its official source
with `MACOSX_DEPLOYMENT_TARGET=14.0`, `--disable-ogg`, `--disable-programs`,
`--disable-examples`, `--disable-cpplibs`, `--disable-static` and
`--enable-shared`. Point `CHILPA_FLAC_LIBRARY` at that private build and
run `tools/build-native` with the portable Python virtual environment.

Strip local symbols from bundled libraries and sign each modified binary
before packaging. Preserve the PyInstaller executable's appended archive.
The ZIP contains Python's license and its runtime; it needs neither Homebrew
nor a separately installed Python. Packaging checks cover every Mach-O
dependency, architecture, minimum OS and signature. The exact ZIP passed the
updated host's installer and permission review on macOS, without a live
account or physical audio device. Signed-in playback remains a listening
check. The Mac runtime source remains the `v0.1.18` tag; the build tool on
`main` adds the Mac library stripping and interpreter license packaging.
