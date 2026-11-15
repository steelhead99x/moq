// SPDX-License-Identifier: GPL-2.0-or-later
#pragma once

#include <algorithm>
#include <array>
#include <cstddef>
#include <cstdint>
#include <deque>
#include <optional>
#include <string>
#include <utility>
#include <vector>

// One decoded picture waiting to be shown. `generation` is the source's current
// identity: a reconnect, a rendition change, or a blank retires everything older.
struct PresentItem {
	uint64_t generation = 0;
	uint64_t timestamp = 0;
	uint32_t width = 0;
	uint32_t height = 0;
	uint64_t id = 0;
};

// What `PresentQueue::select` decided for this media time.
struct PresentSelection {
	// The picture to draw, when one is due.
	std::optional<PresentItem> show;
	// True when `show` replaced the picture that was already up.
	bool changed = false;
	// The picture that was up, released after this draw so the GPU can finish it.
	std::optional<PresentItem> retire;
	// Pictures that will never be shown: late, over the bound, or the wrong generation.
	std::vector<PresentItem> dropped;
};

// Bounded queue of decoded pictures. `select` shows the newest one whose
// timestamp is at or before the media clock, and holds the previous one back
// for one decision so a GPU import can finish sampling it.
class PresentQueue {
public:
	// How many not-yet-shown pictures to keep. Older ones are dropped on push.
	static constexpr size_t kBound = 4;

	uint64_t generation() const { return generation_; }

	const std::string &fallback() const { return fallback_; }

	// A new source identity: drop the queue and allow another GPU probe.
	void restart()
	{
		generation_++;
		pending_.clear();
		showing_.reset();
		blocked_.reset();
		fallback_.clear();
	}

	// Queue `item` when it belongs to the current generation. Assigns `id`.
	// Returns 0 when the generation does not match. Pictures past `kBound` are
	// appended to `dropped`, oldest first.
	uint64_t push(PresentItem item, std::vector<PresentItem> &dropped)
	{
		if (item.generation != generation_)
			return 0;
		item.id = ++next_id_;
		pending_.push_back(item);
		while (pending_.size() > kBound) {
			dropped.push_back(pending_.front());
			pending_.pop_front();
		}
		return item.id;
	}

	// Choose what to draw at `media_now`. A picture timestamped after that stays queued.
	PresentSelection select(uint64_t media_now)
	{
		PresentSelection out;
		std::optional<PresentItem> best;
		std::deque<PresentItem> future;
		for (auto &item : pending_) {
			if (item.generation != generation_) {
				out.dropped.push_back(item);
				continue;
			}
			if (item.timestamp > media_now) {
				future.push_back(item);
				continue;
			}
			if (!best || item.timestamp >= best->timestamp) {
				if (best)
					out.dropped.push_back(*best);
				best = item;
			} else {
				out.dropped.push_back(item);
			}
		}
		pending_ = std::move(future);

		const bool showing_ok = showing_ && showing_->generation == generation_ &&
					showing_->timestamp <= media_now;
		if (showing_ && showing_->generation != generation_) {
			out.dropped.push_back(*showing_);
			showing_.reset();
		}
		if (!best) {
			if (showing_ok)
				out.show = showing_;
			return out;
		}
		if (showing_ok && showing_->timestamp > best->timestamp) {
			out.dropped.push_back(*best);
			out.show = showing_;
			return out;
		}
		if (showing_ && showing_->id != best->id) {
			if (showing_->generation == generation_)
				out.retire = showing_;
			else
				out.dropped.push_back(*showing_);
		}
		showing_ = best;
		out.show = best;
		out.changed = true;
		return out;
	}

	// Remember that this size failed to import, until the size changes or `restart`.
	void fail_gpu(std::string reason, uint32_t width, uint32_t height)
	{
		fallback_ = std::move(reason);
		blocked_ = std::make_pair(width, height);
	}

	bool gpu_blocked(uint32_t width, uint32_t height) const
	{
		return blocked_ && blocked_->first == width && blocked_->second == height;
	}

	void clear_gpu_block()
	{
		blocked_.reset();
		fallback_.clear();
	}

	// Device loss: hand back every picture the queue still owns, including the one on screen.
	std::vector<PresentItem> abandon()
	{
		std::vector<PresentItem> out(pending_.begin(), pending_.end());
		pending_.clear();
		if (showing_) {
			out.push_back(*showing_);
			showing_.reset();
		}
		return out;
	}

	size_t pending() const { return pending_.size(); }

	const std::optional<PresentItem> &showing() const { return showing_; }

private:
	uint64_t generation_ = 1;
	uint64_t next_id_ = 0;
	std::deque<PresentItem> pending_;
	std::optional<PresentItem> showing_;
	std::optional<std::pair<uint32_t, uint32_t>> blocked_;
	std::string fallback_;
};

// A short ring of decode-to-presentation samples, in nanoseconds.
class DelayStats {
public:
	static constexpr size_t kCap = 64;

	void add(uint64_t ns)
	{
		if (filled_ < kCap) {
			samples_[filled_++] = ns;
			return;
		}
		samples_[next_ % kCap] = ns;
		next_++;
	}

	size_t size() const { return filled_; }

	std::optional<uint64_t> percentile(int percent) const
	{
		if (!filled_)
			return std::nullopt;
		std::array<uint64_t, kCap> ordered{};
		std::copy_n(samples_.begin(), filled_, ordered.begin());
		std::sort(ordered.begin(), ordered.begin() + static_cast<std::ptrdiff_t>(filled_));
		// The smallest index whose rank is at least `percent` of the sample.
		size_t index = filled_ * static_cast<size_t>(percent) / 100;
		if (index >= filled_)
			index = filled_ - 1;
		return ordered[index];
	}

	std::optional<uint64_t> p50() const { return percentile(50); }

	std::optional<uint64_t> p95() const { return percentile(95); }

private:
	std::array<uint64_t, kCap> samples_{};
	size_t filled_ = 0;
	size_t next_ = 0;
};

// The color a decoded picture named, when the stream named one.
enum class PresentMatrix { Bt601, Bt709, Bt2020 };
enum class PresentRange { Limited, Full };
enum class PresentTransfer { Bt709, Pq, Hlg };

struct PresentColor {
	PresentMatrix matrix = PresentMatrix::Bt709;
	PresentRange range = PresentRange::Limited;
	PresentTransfer transfer = PresentTransfer::Bt709;
};

// The OBS colorspace this picture should be drawn in.
enum class PresentSpace { Bt601, Bt709, Pq, Hlg };

struct PresentOutput {
	PresentSpace space = PresentSpace::Bt709;
	bool full_range = false;
	PresentTransfer transfer = PresentTransfer::Bt709;
};

// BT.601 limited up to 576 lines, BT.709 limited above that.
inline PresentColor present_infer(uint32_t height)
{
	PresentColor color;
	color.matrix = height <= 576 ? PresentMatrix::Bt601 : PresentMatrix::Bt709;
	color.range = PresentRange::Limited;
	color.transfer = PresentTransfer::Bt709;
	return color;
}

// `known` wins. An absent description uses `present_infer`. BT.2020 SDR has no
// OBS colorspace of its own, so it is drawn as BT.709; PQ and HLG keep their curves.
inline PresentOutput present_output(const std::optional<PresentColor> &known, uint32_t height)
{
	const PresentColor color = known.value_or(present_infer(height));
	PresentOutput out;
	out.full_range = color.range == PresentRange::Full;
	out.transfer = color.transfer;
	if (color.transfer == PresentTransfer::Pq)
		out.space = PresentSpace::Pq;
	else if (color.transfer == PresentTransfer::Hlg)
		out.space = PresentSpace::Hlg;
	else if (color.matrix == PresentMatrix::Bt601)
		out.space = PresentSpace::Bt601;
	else
		out.space = PresentSpace::Bt709;
	return out;
}
