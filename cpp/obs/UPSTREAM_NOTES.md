# Upstream Contribution Notes

This document tracks which components of the live event implementation are suitable for upstream contribution to `moq-dev/moq` versus those that should remain fork-specific.

## Components Suitable for Upstream

The following architectural components are vendor-neutral and provide general value for professional OBS + MoQ streaming:

### Core Event Architecture

**Files:**

- `src/moq-event.h`
- `src/moq-event.cpp`

**What:**

- `MoQEventController` class and API
- SCTE-35 marker structure definitions
- Event configuration structure
- Slate management framework
- Marker scheduling and PTS tracking

**Why Upstream:**

- Standard broadcast protocol support (SCTE-35)
- Generic slate reference system (uses OBS source names)
- No vendor-specific logic or branding
- Reusable by any professional streaming setup
- Well-documented API for external control

**Recommendation:** Propose as enhancement to moq-dev/moq after validation

### Mode Selection Framework

**Files:**

- `src/moq-dock.cpp` (mode combo and visibility logic)
- `src/moq-output.cpp` (event mode handling)

**What:**

- Simple broadcast vs. live event mode toggle
- Conditional feature enablement
- Settings persistence for mode

**Why Upstream:**

- Keeps simple use cases simple
- Professional features opt-in, not forced
- Clean separation of concerns
- No product assumptions

**Recommendation:** Include with event controller PR

### SCTE Integration Points

**Files:**

- `src/moq-output.cpp` (`ProcessEventMarkers` method)
- `src/moq-event.cpp` (marker insertion logic)

**What:**

- PTS-based marker insertion
- SCTE segmentation type enums
- Marker logging and callback system

**Why Upstream:**

- Industry-standard protocol
- Needed for professional broadcast workflows
- No moq-specific wire changes required (markers are application-level)
- Can be extended to write markers into MoQ track metadata

**Recommendation:** Core feature for upstream

## Fork-Specific Components

Components that should remain in `steelhead99x/moq` only:

### Colorbars Branding

**What:**

- Colorbars-specific default URLs
- Product-specific terminology in UI strings
- Hardcoded relay paths for colorbars.dev

**Why Fork-Only:**

- Product branding, not generic functionality
- May confuse users of other services
- Colorbars owns their own UX

**Location:** None currently (kept generic), but guard against adding

### Product Integration Details

**What:**

- Colorbars API authentication flows
- Product-specific config schemas
- Colorbars-specific event templates

**Why Fork-Only:**

- Service-specific implementation
- Not reusable by other deployments
- Couples plugin to external service

**Location:** Keep in Colorbars application, not plugin

## Recommended Contribution Strategy

### Phase 1: Core Event Controller (Immediate)

Submit PR to moq-dev/moq with:

- `moq-event.h/cpp`
- `MoQEventController` API
- SCTE-35 structures and enums
- Documentation of event mode concept

**Rationale:** This is the foundational piece, fully vendor-neutral

### Phase 2: UI Integration (After Phase 1)

Submit PR with:

- Mode selector in dock
- Event control panel
- Settings persistence
- CMake integration

**Rationale:** Shows complete user-facing workflow

### Phase 3: Advanced Features (Future)

Consider upstreaming:

- SCTE-35 binary encoding
- Track-level marker embedding
- WebSocket remote control API
- Timeline/rundown UI

**Rationale:** Natural evolution, maintains upstream value

## Maintenance Strategy

### Fork Maintenance

When upstream accepts changes:

1. Cherry-pick upstream improvements back to fork
2. Keep fork's Colorbars-specific features as additive layer
3. Avoid diverging core logic (merge conflicts)

### Testing Both Codebases

Both fork and upstream should pass:

- Simple broadcast mode (no event features)
- Event mode with SCTE insertion
- Slate reference validation
- Mode switching

Fork additionally tests:

- Colorbars integration endpoints
- Product-specific workflows

## API Stability Commitment

For both upstream and fork:

### Stable (Won't Break)

- Event config structure field names
- SCTE segmentation type enum values
- Mode selection boolean
- Slate source name strings

### Unstable (May Evolve)

- Internal controller implementation
- Callback registration API
- Marker binary encoding format
- Future track metadata schema

## Communication with Upstream

When proposing changes:

1. Reference this document in PR description
2. Emphasize vendor neutrality
3. Show example use case beyond Colorbars
4. Provide complete documentation
5. Include tests for mode switching

Avoid:

- Mentioning Colorbars in commit messages
- Product-specific configuration examples
- Assuming external service integration

## Conflict Resolution

If upstream rejects event features:

1. Maintain full implementation in fork
2. Document divergence clearly
3. Periodically attempt re-proposal with improvements
4. Keep core moq-dev/moq changes (relay, protocol) merged into fork

## Licensing Note

All code added here is GPL-2.0-or-later, matching `cpp/obs/LICENSE`. This applies to both:

- Code intended for upstream
- Fork-specific additions

No license change is required for contribution back to moq-dev/moq.

## Current Status

As of this implementation:

- ✅ Core event controller complete
- ✅ UI integration complete
- ✅ Mode selection complete
- ✅ Documentation complete
- ⏳ Upstream PR not yet submitted (validation phase)
- ⏳ Real SCTE binary encoding not implemented
- ⏳ Track-level marker embedding not implemented

## Validation Checklist

Before proposing upstream:

- \[x] Simple broadcast mode works without event features
- \[ ] Event mode inserts markers at correct PTS
- \[ ] Mode switching doesn't break existing streams
- \[ ] Settings persist across OBS restarts
- \[ ] Slate references resolve to valid OBS sources
- \[ ] Ad breaks schedule both out and in markers
- \[ ] Multiple events can run in succession
- \[ ] Event controls disabled when not streaming
- \[ ] No Colorbars-specific code in proposal

## Contact

For questions about upstream strategy:

- Fork maintainer: steelhead99x/moq
- Upstream: moq-dev/moq (issues/discussions)

This document should be updated as upstream contributions progress.
