// SPDX-License-Identifier: GPL-2.0-or-later
#include "moq-event.h"
#include "logger.h"

#include <utility>

MoQEventController::MoQEventController(const MoQEventConfig &config)
	: config_(config),
	  event_active_(false),
	  next_event_id_(1),
	  event_start_pts_us_(0)
{
}

void MoQEventController::SetConfig(const MoQEventConfig &config)
{
	config_ = config;
}

MoQEventConfig MoQEventController::GetConfig() const
{
	return config_;
}

void MoQEventController::StartEvent()
{
	if (event_active_) {
		LOG_WARNING("Event already active");
		return;
	}

	event_active_ = true;
	event_start_time_ = std::chrono::steady_clock::now();
	event_start_pts_us_ = 0;
	next_event_id_ = 1;
	scheduled_markers_.clear();
	active_slates_.clear();
	pending_markers_.clear();

	LOG_INFO("Live event started");

	if (config_.enabled && config_.auto_start_slate && !config_.start_slate_source.empty()) {
		EventSlate start_slate(config_.start_slate_source, ScteSegmentationType::ProgramStart, 0, false);
		ScheduleSlate(start_slate);
		if (slate_callback_)
			slate_callback_(start_slate, true);
	}
}

void MoQEventController::EndEvent()
{
	if (!event_active_) {
		LOG_WARNING("No active event to end");
		return;
	}

	if (config_.enabled && config_.auto_end_slate && !config_.end_slate_source.empty()) {
		EventSlate end_slate(config_.end_slate_source, ScteSegmentationType::ProgramEnd, 0, false);
		ScheduleSlate(end_slate);
		if (slate_callback_)
			slate_callback_(end_slate, true);
	}

	event_active_ = false;
	LOG_INFO("Live event ended");
}

bool MoQEventController::IsEventActive() const
{
	return event_active_;
}

uint32_t MoQEventController::InsertAdBreak(uint32_t duration_sec)
{
	if (!config_.enabled) {
		LOG_WARNING("Event mode not enabled");
		return 0;
	}

	if (!event_active_) {
		LOG_WARNING("No active event for ad break insertion");
		return 0;
	}

	uint32_t event_id = next_event_id_++;
	uint32_t duration_us = duration_sec * 1000000;

	ScteMarker out_marker(event_start_pts_us_, event_id, true, duration_us,
			      ScteSegmentationType::ProviderAdvertisementStart);
	ScheduleMarker(out_marker);

	ScteMarker in_marker(event_start_pts_us_ + duration_us, event_id, false, 0,
			     ScteSegmentationType::ProviderAdvertisementEnd);
	ScheduleMarker(in_marker);

	LOG_INFO("Scheduled ad break: event_id=%u, duration=%us", event_id, duration_sec);
	return event_id;
}

uint32_t MoQEventController::InsertCustomSlate(const std::string &source_name, uint32_t duration_sec)
{
	if (!config_.enabled) {
		LOG_WARNING("Event mode not enabled");
		return 0;
	}

	if (!event_active_) {
		LOG_WARNING("No active event for slate insertion");
		return 0;
	}

	uint32_t event_id = next_event_id_++;

	EventSlate slate(source_name, ScteSegmentationType::BreakStart, duration_sec, true);
	ScheduleSlate(slate);

	if (slate_callback_)
		slate_callback_(slate, true);

	ScteMarker break_start(event_start_pts_us_, event_id, true, duration_sec * 1000000,
			       ScteSegmentationType::BreakStart);
	ScheduleMarker(break_start);

	LOG_INFO("Scheduled custom slate: event_id=%u, source=%s, duration=%us", event_id, source_name.c_str(),
		 duration_sec);
	return event_id;
}

void MoQEventController::CancelSlate(uint32_t event_id)
{
	auto marker_it = scheduled_markers_.find(event_id);
	if (marker_it != scheduled_markers_.end()) {
		scheduled_markers_.erase(marker_it);
	}

	auto slate_it = active_slates_.find(event_id);
	if (slate_it != active_slates_.end()) {
		if (slate_callback_)
			slate_callback_(slate_it->second, false);
		active_slates_.erase(slate_it);
	}

	LOG_INFO("Cancelled event/slate: event_id=%u", event_id);
}

std::vector<ScteMarker> MoQEventController::GetPendingMarkers(uint64_t current_pts_us)
{
	ProcessMarkers(current_pts_us);
	std::vector<ScteMarker> markers;
	markers.swap(pending_markers_);
	return markers;
}

std::vector<EventSlate> MoQEventController::GetActiveSlates() const
{
	std::vector<EventSlate> slates;
	for (const auto &[id, slate] : active_slates_) {
		slates.push_back(slate);
	}
	return slates;
}

void MoQEventController::SetScteCallback(std::function<void(const ScteMarker &)> callback)
{
	scte_callback_ = callback;
}

void MoQEventController::SetSlateCallback(std::function<void(const EventSlate &, bool active)> callback)
{
	slate_callback_ = callback;
}

void MoQEventController::ScheduleMarker(ScteMarker marker)
{
	scheduled_markers_.insert_or_assign(marker.event_id, std::move(marker));
}

void MoQEventController::ScheduleSlate(EventSlate slate)
{
	uint32_t event_id = next_event_id_++;
	active_slates_.insert_or_assign(event_id, std::move(slate));
}

void MoQEventController::ProcessMarkers(uint64_t current_pts_us)
{
	if (event_start_pts_us_ == 0) {
		event_start_pts_us_ = current_pts_us;
	}

	std::vector<uint32_t> to_remove;
	for (const auto &[event_id, marker] : scheduled_markers_) {
		if (marker.pts_us <= current_pts_us) {
			pending_markers_.push_back(marker);
			to_remove.push_back(event_id);

			if (scte_callback_)
				scte_callback_(marker);
		}
	}

	for (uint32_t event_id : to_remove) {
		scheduled_markers_.erase(event_id);
	}
}

void register_moq_event() {}
