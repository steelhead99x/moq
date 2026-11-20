# obs-moq

An OBS Studio plugin for publishing to and subscribing from MoQ relays.

It loads into a stock OBS Studio install (no OBS source build required) and links
the moq C++ package, built from the in-tree [`cpp/moq`](../moq) over
[`rs/moq-ffi`](../../rs/moq-ffi).

## Live events

The dock's Mode selector adds an opt-in live event panel: start and end, ad breaks, and
slate source names. Due markers are logged as video frames pass. They are not muxed into
the broadcast. See [EVENT_API.md](EVENT_API.md). Simple broadcast is the default.

Build instructions for each platform live in [`doc/bin/obs.md`](../../doc/bin/obs.md).
In short, from the repo root:

```bash
# Linux: the dev shell provides libobs/Qt6/ffmpeg
nix develop
just obs build

# macOS / Windows: needs Xcode / Visual Studio 2022; obs-deps download via buildspec.json
just obs setup
just obs build
```

Licensed under GPL-2.0-or-later (see [LICENSE](LICENSE)), separate from the rest of the
repository, because it links OBS.

Release dry runs use `just obs package --target TARGET --moq-archive FILE` to
link the matching local C++ release archive. Tag builds use
`--moq-release VERSION` to download the published archive. The two inputs are
mutually exclusive; neither changes the plugin version in `VERSION`.
