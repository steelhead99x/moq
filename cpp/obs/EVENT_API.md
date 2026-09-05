# OBS MoQ Plugin: Live Event API

This document describes the event control API and configuration contract for obs-moq live event support, designed to be driven by external control applications like Colorbars Live Events.

## Overview

The obs-moq plugin supports two operational modes:

1. **Simple Broadcast**: Direct connect and stream, minimal configuration
2. **Live Event**: Full production control with SCTE-35 markers, slate management, and ad break insertion

## Mode Selection

The mode is selected via the MoQ dock UI or through OBS service settings:

```cpp
obs_data_set_bool(settings, "event_mode_enabled", true);  // Enable event mode
```

## Event Configuration

Event settings are passed through the OBS service settings to the MoQ output:

```cpp
// Event mode toggle
obs_data_set_bool(settings, "event_mode_enabled", true);

// Auto-slate configuration
obs_data_set_bool(settings, "event_auto_start_slate", true);
obs_data_set_bool(settings, "event_auto_end_slate", true);

// Slate source names (OBS source references)
obs_data_set_string(settings, "event_start_slate_source", "StartSlate");
obs_data_set_string(settings, "event_end_slate_source", "EndSlate");

// Default ad break duration (seconds)
obs_data_set_int(settings, "event_ad_duration", 120);
```

## SCTE-35 Marker Structure

SCTE-35 markers are inserted into the stream at specified presentation timestamps (PTS) to signal commercial breaks and program boundaries:

```cpp
struct ScteMarker {
    uint64_t pts_us;                    // Presentation timestamp (microseconds)
    ScteCommandType command_type;        // SpliceInsert or TimeSignal
    uint32_t event_id;                   // Unique event identifier
    bool out_of_network;                 // true = splice out, false = splice in
    uint32_t duration_us;                // Break duration (microseconds)
    ScteSegmentationType segmentation_type;  // Segmentation type descriptor
    std::string descriptor;              // Optional descriptor data
};
```

### SCTE Segmentation Types

Key segmentation types supported:

- `ProgramStart` (0x10): Event/program start
- `ProgramEnd` (0x11): Event/program end
- `ProviderAdvertisementStart` (0x30): Ad break start
- `ProviderAdvertisementEnd` (0x31): Ad break end
- `BreakStart` (0x50): General break start
- `BreakEnd` (0x51): General break end

## Event Control Flow

### Starting an Event

When event mode is enabled and streaming starts with auto-start slate enabled:

1. Output creates `MoQEventController` with config
2. Controller schedules start slate if configured
3. Stream begins with slate active
4. SCTE `ProgramStart` marker inserted at event start PTS

### Ad Break Insertion

Ad breaks can be triggered programmatically:

```cpp
// Insert 120-second ad break
uint32_t event_id = controller->InsertAdBreak(120);

// This schedules two markers:
// 1. SCTE out (ProviderAdvertisementStart) at current PTS
// 2. SCTE in (ProviderAdvertisementEnd) at PTS + duration
```

### Custom Slate Insertion

Insert a custom slate mid-event:

```cpp
// Insert custom slate from OBS source "EventPromo" for 30 seconds
uint32_t event_id = controller->InsertCustomSlate("EventPromo", 30);
```

### Ending an Event

When the stream stops or event ends:

1. Controller schedules end slate if configured
2. SCTE `ProgramEnd` marker inserted
3. Stream transitions to end slate or terminates

## Integration Points for External Control

### WebSocket Control (Recommended)

OBS WebSocket can be used to trigger event controls remotely. Control applications should:

1. Connect to OBS WebSocket API
2. Configure event settings via service properties
3. Trigger scene switches for slates
4. Monitor output status for event state

### Configuration File

Event settings are persisted in the OBS profile's dock.json:

```json
{
  "url": "https://relay.example.com/anon",
  "path": "my-broadcast",
  "mode": 1,
  "event_auto_start_slate": true,
  "event_auto_end_slate": true,
  "event_start_slate": "StartSlate",
  "event_end_slate": "EndSlate",
  "event_ad_duration": 120,
  "advanced": { ... }
}
```

### Direct OBS Integration

Applications with OBS plugin access can call the event controller directly:

```cpp
// Get output instance
obs_output_t *output = obs_get_output_by_name("moq_dock_output");
if (!output) return;

// Access MoQOutput private data (requires plugin API)
// Event operations:
// - controller->StartEvent()
// - controller->InsertAdBreak(duration_sec)
// - controller->InsertCustomSlate(source, duration_sec)
// - controller->EndEvent()
```

## Slate Management

Slates reference OBS source names. The controller does not directly switch scenes; it:

1. Tracks active slates and their timing
2. Provides slate state via callbacks
3. Logs slate events for external monitoring
4. Expects the application/operator to perform scene switches

This separation keeps the plugin vendor-neutral and allows control applications to implement their own UX for slate transitions.

## Callbacks and Event Notification

The event controller supports callbacks for external monitoring:

```cpp
// SCTE marker callback
controller->SetScteCallback([](const ScteMarker &marker) {
    LOG_INFO("SCTE marker: event_id=%u, type=%u", 
             marker.event_id, 
             (uint8_t)marker.segmentation_type);
});

// Slate activation callback
controller->SetSlateCallback([](const EventSlate &slate, bool active) {
    if (active) {
        LOG_INFO("Slate active: source=%s, duration=%us",
                 slate.source_name.c_str(),
                 slate.duration_sec);
    } else {
        LOG_INFO("Slate ended: source=%s",
                 slate.source_name.c_str());
    }
});
```

## Simple Broadcast Mode

When event mode is disabled (mode = 0):

- No SCTE markers are inserted
- No slate management is performed
- Stream operates as a standard MoQ broadcast
- Event controls are hidden in the UI

This preserves the simple "connect and go live" workflow for basic streaming use cases.

## Upstream vs Fork-Specific Notes

### Suitable for Upstream (moq-dev/moq)

- Core event controller architecture
- SCTE-35 marker structure and timing
- Mode selection framework (simple/event toggle)
- Configuration persistence
- Generic slate reference system
- WebSocket integration points

These components are vendor-neutral and provide value to any OBS + MoQ user.

### Fork-Specific (steelhead99x/moq)

- Colorbars-specific branding or UI elements
- Product-specific default configurations
- Hardcoded relay URLs or paths
- Colorbars API integration details

Keep product-specific UX in the external control application (Colorbars) rather than embedding it in the plugin.

## Example Usage

### Colorbars Control Flow

1. User configures event in Colorbars UI
2. Colorbars writes OBS profile settings with event config
3. User starts stream from OBS dock (or Colorbars triggers via WebSocket)
4. Plugin inserts start slate and SCTE marker
5. Colorbars monitors event state
6. Colorbars sends ad break commands as scheduled
7. Plugin inserts SCTE out/in markers
8. Colorbars handles scene switching for slates
9. Event ends, plugin inserts end slate and final marker

This design keeps the plugin focused on SCTE/marker insertion while allowing Colorbars to own the production UX.

## Future Enhancements

Potential additions for both upstream and fork:

- Real-time SCTE-35 binary encoding (currently logs markers)
- Track-level marker insertion (embed in MoQ broadcast metadata)
- Rundown/timeline UI in OBS dock
- Remote control protocol (beyond WebSocket)
- Multiple slate tracks per event
- Dynamic ad duration adjustment
- SCTE descriptor field population

These would extend the API while maintaining backward compatibility with simple broadcast mode.
