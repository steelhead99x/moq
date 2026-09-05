# [S] Shared watch AudioContext

## Goal

A `@moq/watch` audio Decoder can render into an AudioContext the application
owns, so several remotes share one listener graph. The decoder never closes an
injected context. There is no spatial mixer API: apps connect `Decoder.out.root`
to their own PannerNode or GainNode.

## Plan

- Optional constructor knob `context` (`AudioContext` or a Signal of one). When
  unset, the decoder still creates and closes its own context.
- Load the render worklet once per context so two Decoders can share it.
- If an injected context's sample rate disagrees with the decoded PCM rate,
  warn and keep the injected context. Do not replace or close it.
- Tests: injected context is reused and survives `Decoder.close()`; the default
  path still owns and closes its context.

## Closes

- [#8](https://github.com/steelhead99x/moq/issues/8) - close this issue when the quest finishes

## Related

- [Room SDK](/quest/m2/room-sdk.md) - roster and identity; not required for the shared-context seam
