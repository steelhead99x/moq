// SPDX-License-Identifier: GPL-2.0-or-later
#pragma once

#include <obs-module.h>
#include <chrono>
#include <cstdint>
#include <functional>
#include <map>
#include <memory>
#include <string>
#include <vector>

struct MoQEventConfig {
	bool enabled = false;
	bool auto_start_slate = true;
	bool auto_end_slate = true;
	uint32_t default_ad_duration_sec = 120;
	std::string start_slate_source;
	std::string end_slate_source;
};

enum class ScteCommandType : uint8_t {
	SpliceInsert = 0x05,
	TimeSignal = 0x06,
};

enum class ScteSegmentationType : uint8_t {
	NotIndicated = 0x00,
	ContentIdentification = 0x01,
	ProgramStart = 0x10,
	ProgramEnd = 0x11,
	ProgramEarlyTermination = 0x12,
	ProgramBreakaway = 0x13,
	ProgramResumption = 0x14,
	ProgramRunoverPlanned = 0x15,
	ProgramRunoverUnplanned = 0x16,
	ProgramOverlapStart = 0x17,
	ProgramBlackoutOverride = 0x18,
	ProviderAdvertisementStart = 0x30,
	ProviderAdvertisementEnd = 0x31,
	DistributorAdvertisementStart = 0x32,
	DistributorAdvertisementEnd = 0x33,
	ChapterStart = 0x40,
	BreakStart = 0x50,
	BreakEnd = 0x51,
};

struct ScteMarker {
	uint64_t pts_us;
	ScteCommandType command_type;
	uint32_t event_id;
	bool out_of_network;
	uint32_t duration_us;
	ScteSegmentationType segmentation_type;
	std::string descriptor;

	ScteMarker(uint64_t pts, uint32_t event, bool out, uint32_t dur, ScteSegmentationType seg)
		: pts_us(pts),
		  command_type(ScteCommandType::SpliceInsert),
		  event_id(event),
		  out_of_network(out),
		  duration_us(dur),
		  segmentation_type(seg)
	{
	}
};

struct EventSlate {
	std::string source_name;
	ScteSegmentationType trigger_type;
	uint32_t duration_sec;
	bool auto_return;

	EventSlate(const std::string &src, ScteSegmentationType trig, uint32_t dur, bool auto_ret)
		: source_name(src), trigger_type(trig), duration_sec(dur), auto_return(auto_ret)
	{
	}
};

class MoQEventController {
public:
	explicit MoQEventController(const MoQEventConfig &config);
	~MoQEventController() = default;

	void SetConfig(const MoQEventConfig &config);
	MoQEventConfig GetConfig() const;

	void StartEvent();
	void EndEvent();
	bool IsEventActive() const;

	uint32_t InsertAdBreak(uint32_t duration_sec);
	uint32_t InsertCustomSlate(const std::string &source_name, uint32_t duration_sec);

	void CancelSlate(uint32_t event_id);

	std::vector<ScteMarker> GetPendingMarkers(uint64_t current_pts_us);
	std::vector<EventSlate> GetActiveSlates() const;

	void SetScteCallback(std::function<void(const ScteMarker &)> callback);
	void SetSlateCallback(std::function<void(const EventSlate &, bool active)> callback);

private:
	void ScheduleMarker(ScteMarker marker);
	void ScheduleSlate(EventSlate slate);
	void ProcessMarkers(uint64_t current_pts_us);

	MoQEventConfig config_;
	bool event_active_;
	uint32_t next_event_id_;

	std::map<uint32_t, ScteMarker> scheduled_markers_;
	std::map<uint32_t, EventSlate> active_slates_;
	std::vector<ScteMarker> pending_markers_;

	std::function<void(const ScteMarker &)> scte_callback_;
	std::function<void(const EventSlate &, bool active)> slate_callback_;

	std::chrono::steady_clock::time_point event_start_time_;
	uint64_t event_start_pts_us_;
};

void register_moq_event();
