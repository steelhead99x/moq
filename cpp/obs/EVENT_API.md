# Live event mode

Opt-in controls on the MoQ dock. Simple broadcast stays the default.

Set Mode to Live Event, then Go Live. The dock copies the slate sources, the ad-break
duration, and `event_mode_enabled` onto the service settings the output reads at start.
Start Event, End Event, Insert Ad Break, and Insert Slate call `MoQOutput`, which owns a
`MoQEventController`.

On each video frame the output asks the controller for markers whose timestamp has arrived
and logs them. The markers are not SCTE-35 sections and they are not written onto the
broadcast. Slate source names are recorded for the operator. Switching the named OBS source
on and off is still the caller's job.

`MoQEventController` (`src/moq-event.h`) is the fork-specific piece: schedule, cancel, and
read pending markers. The dock and output wiring around it follows the current plugin.
