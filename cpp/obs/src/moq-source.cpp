// SPDX-License-Identifier: GPL-2.0-or-later
#include <obs-module.h>
#include <util/threading.h>
#include <util/platform.h>
#include <util/darray.h>
#include <util/dstr.h>
#include <util/util_uint64.h>

#include <moq/moq.hpp>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <deque>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <tuple>
#include <type_traits>
#include <unordered_map>
#include <utility>
#include <variant>
#include <vector>

#ifdef __APPLE__
#include <CoreFoundation/CoreFoundation.h>
#include <CoreVideo/CoreVideo.h>
#include <IOSurface/IOSurface.h>
#include <VideoToolbox/VideoToolbox.h>
#endif

#ifdef _WIN32
#define strncasecmp _strnicmp
#endif

#include "moq-implementation.h"
#include "moq-source-present.h"
#include "moq-source.h"
#include "moq-url.h"
#include "moq-worker.h"
#include "logger.h"

namespace {

struct moq_source;

// A track timestamp in the nanoseconds OBS stamps media with, floored when the scale does not
// divide a second evenly into nanoseconds.
static uint64_t moq_source_nanoseconds(const moq::Timestamp &timestamp)
{
	return util_mul_div64(timestamp.value, 1'000'000'000, timestamp.scale);
}

// How a decoded audio rendition reaches OBS: the channel count moq-audio remixes to, the
// OBS layout that places those channels, and the rate moq-audio resamples to.
struct AudioLayout {
	uint32_t channels = 0;
	enum speaker_layout speakers = SPEAKERS_UNKNOWN;
	uint32_t sample_rate = 0;
};

// Own the ranked snapshot while setup asynchronously tries each rendition once.
struct AudioSelection {
	std::vector<std::pair<std::string, moq::Audio>> candidates;
	size_t next = 0;
};

// Same shape as audio: a refusal opens the next rendition instead of stopping.
struct VideoSelection {
	std::vector<std::pair<std::string, moq::Video>> candidates;
	size_t next = 0;
};

// Codecs moq-video decodes, codecs it will decode later, and everything else.
enum class VideoSupport { Decode, Deferred, Unknown };

// One subscribed track: what it is, the consumer once subscribed, and the pending
// call that subscribes or reads its next frame. Dropping it cancels that call.
struct Track {
	Track(const char *kind, std::string id) : kind(kind), id(std::move(id)) {}

	// "Video" or "Audio", for the log.
	const char *kind;
	// The rendition's catalog ID.
	std::string id;
	// The publisher instance the track reads, so a newer one resubscribes even an unchanged rendition.
	uint64_t instance = 0;
	// The rendition the decoder was built from.
	std::optional<moq::Video> video_config;
	std::optional<moq::Audio> audio_config;
	AudioLayout audio_layout;
	uint64_t audio_frames = 0;
	// Ask moq-video to keep the decoder surface. macOS only; a refusal retries on the CPU.
	bool surface = false;
	// Set once the first picture of this track has been shown.
	bool started = false;

	// Video reads pictures moq-video decoded; audio reads PCM moq-audio decoded.
	std::shared_ptr<moq::VideoConsumer> video_consumer;
	std::shared_ptr<moq::AudioConsumer> audio_consumer;
	std::shared_ptr<VideoSelection> video_selection;
	std::shared_ptr<AudioSelection> audio_selection;
	std::optional<moq::Continuation> call;
};

// Everything one connect owns, from the client to the tracks. A reconnect, a
// settings change, a terminal failure, or destroy drops it whole, which cancels
// every pending call it holds.
struct Connection {
	uint64_t revision = 0;
	std::string url;
	std::string broadcast;

	std::shared_ptr<moq::Client> client;
	std::shared_ptr<moq::Session> session;
	std::shared_ptr<moq::Following> announced;

	// How many publisher instances `announced` has handed out; the newest is the one read.
	uint64_t instance = 0;
	std::shared_ptr<moq::BroadcastConsumer> consumer;
	std::shared_ptr<moq::MediaCatalogConsumer> catalog;

	// The connect, then the session's lifecycle revisions.
	std::optional<moq::Continuation> session_call;
	// Waiting for the next publisher instance.
	std::optional<moq::Continuation> available_call;
	// Subscribing to the instance's catalog, then its next update.
	std::optional<moq::Continuation> catalog_call;

	std::shared_ptr<Track> video;
	std::shared_ptr<Track> audio;
	std::shared_ptr<Track> pending_video;
	std::shared_ptr<Track> pending_audio;
};

struct moq_source {
	obs_source_t *source = nullptr;

	// Guards everything below. Every continuation takes it, so it also serializes
	// them against update and destroy on the OBS threads.
	std::mutex mutex;

	// Settings - current active connection settings
	std::string url;
	std::string broadcast;
	bool keylog = false;

	// The current connection, or null while disconnected. A continuation for any
	// other connection is stale and returns without touching anything.
	std::shared_ptr<Connection> connection;

	// The OBS mixer's rate, which audio is resampled to so OBS need not resample it
	// again. Zero when OBS has no audio output. Set once at create.
	uint32_t sample_rate = 0;

	// How many video decoders this source has opened. A replacement instance opens one
	// of its own while the previous instance is still playing.
	std::atomic<int> video_subscriptions{0};

	// Guards the presentation queue, its stats, and (on macOS) the texture. Take it
	// only while already holding `mutex`, or from the graphics thread on its own.
	// Never take `mutex` while holding this.
	std::mutex present_mutex;
	PresentQueue present;
	DelayStats video_delay;
	std::string video_decoder_name;
	std::string video_path;

	// Runs every continuation; destroy stops it before freeing the source.
	MoQWorker worker;

#ifdef __APPLE__
	struct AppleHeld {
		PresentItem item;
		std::shared_ptr<moq::VideoDecodedFrame> frame;
		CVPixelBufferRef converted = nullptr;
		std::chrono::steady_clock::time_point arrived{};
	};

	std::unordered_map<uint64_t, AppleHeld> apple_waiting;
	std::optional<AppleHeld> apple_showing;
	std::optional<AppleHeld> apple_retired;
	// Pictures the worker retired. The graphics thread destroys the texture, then releases these.
	std::vector<AppleHeld> apple_doomed;
	gs_texture_t *apple_texture = nullptr;
	VTPixelTransferSessionRef apple_transfer = nullptr;
	uint32_t display_width = 0;
	uint32_t display_height = 0;
	uint64_t media_base_pts = 0;
	uint64_t tick_base_ns = 0;
	uint64_t tick_ns = 0;
	bool media_clock = false;
#endif
};

} // namespace

// Forward declarations
static void moq_source_update(void *data, obs_data_t *settings);
static void moq_source_destroy(void *data);
static obs_properties_t *moq_source_properties(void *data);
static void moq_source_get_defaults(obs_data_t *settings);

// Helper functions
static void moq_source_reconnect(struct moq_source *ctx);
static void moq_source_disconnect_locked(struct moq_source *ctx);
static void moq_source_blank_video(struct moq_source *ctx);
static void moq_source_on_connect(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				  moq::expected<std::shared_ptr<moq::Session>> result);
static void moq_source_next_broadcast(struct moq_source *ctx, const std::shared_ptr<Connection> &conn);
static void moq_source_on_broadcast(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				    moq::expected<std::shared_ptr<moq::BroadcastConsumer>> result);
static void moq_source_on_catalogs(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				   moq::expected<std::shared_ptr<moq::MediaCatalogConsumer>> result);
static void moq_source_watch_state(struct moq_source *ctx, const std::shared_ptr<Connection> &conn);
static void moq_source_on_state(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				moq::expected<moq::ConnectionState> result);
static void moq_source_on_catalog(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				  moq::expected<std::optional<moq::Catalog>> result);
static void moq_source_next_catalog(struct moq_source *ctx, const std::shared_ptr<Connection> &conn);
static void moq_source_clear_video_locked(struct moq_source *ctx);
static void moq_source_subscribe_video(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				       const moq::Catalog &catalog);
static void moq_source_try_video(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				 const std::shared_ptr<VideoSelection> &selection);
static void moq_source_open_video(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				  const std::shared_ptr<Track> &track);
#ifdef __APPLE__
static void moq_source_apple_release_frames_locked(struct moq_source *ctx);
static void moq_source_apple_queue(struct moq_source *ctx, const std::shared_ptr<moq::VideoDecodedFrame> frame);
static void moq_source_apple_shutdown(struct moq_source *ctx);
static void moq_source_video_tick(void *data, float seconds);
static void moq_source_video_render(void *data, gs_effect_t *effect);
static uint32_t moq_source_get_width(void *data);
static uint32_t moq_source_get_height(void *data);
#endif
static void moq_source_clear_audio_locked(struct moq_source *ctx);
static void moq_source_subscribe_audio(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				       const moq::Catalog &catalog);
static void moq_source_try_audio(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				 const std::shared_ptr<AudioSelection> &selection);

// Wraps a continuation so it runs under ctx->mutex, and only while `conn` is still
// the source's connection. Dropping a connection cancels its calls, but a result
// that already completed may be queued on the worker by then.
template<typename Output, typename Callback>
static std::function<void(Output)> moq_source_current(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
						      Callback callback)
{
	return [ctx, weak = std::weak_ptr<Connection>(conn), callback = std::move(callback)](Output output) {
		std::lock_guard<std::mutex> lock(ctx->mutex);
		auto current = weak.lock();
		if (!current || current != ctx->connection)
			return;
		callback(ctx, current, std::move(output));
	};
}

// Wraps a continuation so it also runs only while `conn` reads the publisher instance it
// was issued for. Replacing the instance cancels its calls, but a result may be queued.
template<typename Output>
static std::function<void(Output)>
moq_source_instance_current(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
			    void (*callback)(struct moq_source *, const std::shared_ptr<Connection> &, Output))
{
	return moq_source_current<Output>(ctx, conn,
					  [instance = conn->instance, callback](struct moq_source *ctx,
										const std::shared_ptr<Connection> &conn,
										Output output) {
						  if (conn->instance == instance)
							  callback(ctx, conn, std::move(output));
					  });
}

static void *moq_source_create(obs_data_t *settings, obs_source_t *source)
{
	auto *ctx = new moq_source();
	ctx->source = source;
	obs_audio_info audio = {};
	if (obs_get_audio_info(&audio))
		ctx->sample_rate = audio.samples_per_sec;

	// Dimensions will be set dynamically from the stream.

	// Load settings from OBS - this will auto-connect if settings are valid
	// (moq_source_update detects settings changed from empty and reconnects)
	moq_source_update(ctx, settings);

	return ctx;
}

static void moq_source_destroy(void *data)
{
	auto *ctx = static_cast<struct moq_source *>(data);

	{
		// Dropping the connection cancels every call it had pending.
		std::lock_guard<std::mutex> lock(ctx->mutex);
		moq_source_disconnect_locked(ctx);
	}

	// Drops whatever is still queued and waits out a continuation in flight, which
	// may be parked on ctx->mutex; so this must not hold it. Nothing touches ctx
	// once it returns.
	ctx->worker.Stop();

#ifdef __APPLE__
	// The texture belongs to the graphics thread. Shutdown destroys it, then the frames.
	obs_enter_graphics();
	moq_source_apple_shutdown(ctx);
	obs_leave_graphics();
#endif

	delete ctx;
}

// Relay URLs can embed credentials (userinfo) or a query/path token; MoQRedactUrl
// strips those for logging (see moq-url.h).

static void moq_source_update(void *data, obs_data_t *settings)
{
	auto *ctx = static_cast<struct moq_source *>(data);

	const char *url_value = obs_data_get_string(settings, "url");
	const char *broadcast_value = obs_data_get_string(settings, "broadcast");
	const std::string url = url_value ? url_value : "";
	const std::string broadcast = broadcast_value ? broadcast_value : "";
	const bool keylog = obs_data_get_bool(settings, "quic_keylog");

	bool settings_changed;
	{
		std::lock_guard<std::mutex> lock(ctx->mutex);
		settings_changed = url != ctx->url || broadcast != ctx->broadcast || keylog != ctx->keylog;
		ctx->url = url;
		ctx->broadcast = broadcast;
		ctx->keylog = keylog;
	}

	// Check if new settings are valid for connection
	const bool valid = !url.empty() && !broadcast.empty();

	// If settings changed and are valid, reconnect
	if (settings_changed && valid) {
		LOG_INFO("Settings changed, reconnecting (url=%s, broadcast=%s)", MoQRedactUrl(url).c_str(),
			 broadcast.c_str());
		moq_source_reconnect(ctx);
	} else if (settings_changed && !valid) {
		LOG_INFO("Settings changed but invalid - disconnecting");
		{
			std::lock_guard<std::mutex> lock(ctx->mutex);
			moq_source_disconnect_locked(ctx);
		}
		moq_source_blank_video(ctx);
	}
}

static void moq_source_get_defaults(obs_data_t *settings)
{
	obs_data_set_default_string(settings, "url", "http://localhost:4443");
	obs_data_set_default_string(settings, "broadcast", "obs/test");
	obs_data_set_default_bool(settings, "quic_keylog", false);
}

static std::string moq_source_video_stats_locked(struct moq_source *ctx)
{
	std::string text = ctx->video_decoder_name.empty() ? "waiting" : ctx->video_decoder_name;
	if (!ctx->video_path.empty())
		text += " / " + ctx->video_path;
	if (!ctx->present.fallback().empty())
		text += " / " + ctx->present.fallback();
	if (auto p50 = ctx->video_delay.p50())
		text += " / p50 " + std::to_string(*p50 / 1000000) + " ms";
	if (auto p95 = ctx->video_delay.p95())
		text += " / p95 " + std::to_string(*p95 / 1000000) + " ms";
	return text;
}

static obs_properties_t *moq_source_properties(void *data)
{
	auto *ctx = static_cast<struct moq_source *>(data);

	obs_properties_t *props = obs_properties_create();

	obs_properties_add_text(props, "url", "URL", OBS_TEXT_DEFAULT);
	obs_properties_add_text(props, "broadcast", "Broadcast", OBS_TEXT_DEFAULT);
	// Writes TLS session secrets to the file SSLKEYLOGFILE names, to decrypt a packet
	// capture; without SSLKEYLOGFILE the dial fails.
	obs_properties_add_bool(props, "quic_keylog", "TLS key log");
	// OBS copies this label when the dialog opens. It names the decoder, the path
	// (CPU or GPU), a fallback reason, and the handoff delay.
	std::string stats = "waiting";
	if (ctx) {
		std::lock_guard<std::mutex> present(ctx->present_mutex);
		stats = moq_source_video_stats_locked(ctx);
	}
	obs_properties_add_text(props, "video_stats", stats.c_str(), OBS_TEXT_INFO);

	return props;
}

// Tear down the connection after a failure to reach playable media, leaving the
// source disconnected so the next update/reconnect starts clean.
//
// NOTE: Caller must hold ctx->mutex.
static void moq_source_fail_locked(struct moq_source *ctx)
{
	moq_source_disconnect_locked(ctx);
	moq_source_blank_video(ctx);
}

// The source dials with the defaults, plus the TLS key log when the user opts in.
static moq::ClientConfig moq_source_client_config(bool keylog)
{
	moq::ClientConfig config;
	config.quic.keylog = keylog;
	config.implementation = MoQImplementation();
	return config;
}

static void moq_source_reconnect(struct moq_source *ctx)
{
	std::lock_guard<std::mutex> lock(ctx->mutex);
	moq_source_disconnect_locked(ctx);

	// Blank video while reconnecting to avoid showing stale frames
	moq_source_blank_video(ctx);

	auto conn = std::make_shared<Connection>();
	conn->url = ctx->url;
	conn->broadcast = ctx->broadcast;
	// Neither origin side is wired, so the session's consume side carries the
	// remote's announcements.
	auto client = moq::Client::init(moq_source_client_config(ctx->keylog));
	if (!client) {
		// Already disconnected and blanked above.
		LOG_ERROR("Failed to create MoQ client: %s", client.error().to_string().c_str());
		return;
	}
	conn->client = *client;
	ctx->connection = conn;

	LOG_INFO("Connecting to MoQ server: %s", MoQRedactUrl(conn->url).c_str());

	conn->session_call = conn->client->connect(conn->url).then(
		ctx->worker.Executor(),
		moq_source_current<moq::expected<std::shared_ptr<moq::Session>>>(ctx, conn, moq_source_on_connect));
}

static void moq_source_on_connect(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				  moq::expected<std::shared_ptr<moq::Session>> result)
{
	if (!result) {
		LOG_ERROR("MoQ session error: %s", result.error().to_string().c_str());
		moq_source_fail_locked(ctx);
		return;
	}

	conn->session = *result;
	moq_source_on_state(ctx, conn, conn->session->state());
	if (ctx->connection != conn)
		return;

	// Wait for the broadcast to be announced. Announcements arrive over the session
	// after it connects, so resolving against only what is announced *now* would race
	// them and blank the source for a broadcast that is live.
	auto announced = conn->session->consume()->follow(conn->broadcast, nullptr);
	if (!announced) {
		LOG_ERROR("Failed to request broadcast '%s': %s", conn->broadcast.c_str(),
			  announced.error().to_string().c_str());
		moq_source_fail_locked(ctx);
		return;
	}
	conn->announced = *announced;
	LOG_INFO("Requesting broadcast: %s", conn->broadcast.c_str());

	moq_source_next_broadcast(ctx, conn);
}

// Waits for the next publisher instance: the first one announced, then each that takes over
// or returns after the broadcast went offline.
static void moq_source_next_broadcast(struct moq_source *ctx, const std::shared_ptr<Connection> &conn)
{
	conn->available_call = conn->announced->available().then(
		ctx->worker.Executor(), moq_source_current<moq::expected<std::shared_ptr<moq::BroadcastConsumer>>>(
						ctx, conn, moq_source_on_broadcast));
}

static void moq_source_on_broadcast(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				    moq::expected<std::shared_ptr<moq::BroadcastConsumer>> result)
{
	if (!result) {
		LOG_ERROR("Failed to resolve broadcast '%s': %s", conn->broadcast.c_str(),
			  result.error().to_string().c_str());
		moq_source_fail_locked(ctx);
		return;
	}

	// The offline instance's tracks end on their own once its route dies, holding the last
	// frame, so only wait for whatever serves the path next.
	if (!*result) {
		LOG_INFO("Broadcast went offline: %s", conn->broadcast.c_str());
		moq_source_next_broadcast(ctx, conn);
		return;
	}

	// Another instance took over, or the broadcast came back. The previous one's active tracks
	// keep playing until the new catalog's renditions commit, or until they end once its
	// route dies, holding the last frame.
	if (conn->instance++ > 0)
		LOG_INFO("Following a new publisher instance of broadcast: %s", conn->broadcast.c_str());
	conn->consumer = *result;
	conn->catalog.reset();
	conn->pending_video.reset();
	conn->pending_audio.reset();

	conn->catalog_call =
		moq::MediaCatalogConsumer::subscribe(conn->consumer)
			.then(ctx->worker.Executor(),
			      moq_source_instance_current<moq::expected<std::shared_ptr<moq::MediaCatalogConsumer>>>(
				      ctx, conn, moq_source_on_catalogs));
	moq_source_next_broadcast(ctx, conn);
}

static void moq_source_on_catalogs(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				   moq::expected<std::shared_ptr<moq::MediaCatalogConsumer>> result)
{
	if (!result) {
		// Stop the instance this one replaced too, and wait for the next one.
		LOG_ERROR("Failed to subscribe to catalog: %s", result.error().to_string().c_str());
		moq_source_clear_video_locked(ctx);
		moq_source_clear_audio_locked(ctx);
		moq_source_blank_video(ctx);
		return;
	}

	conn->catalog = *result;
	LOG_INFO("Consuming broadcast: %s", conn->broadcast.c_str());
	moq_source_next_catalog(ctx, conn);
}

// Follows the session's connect and reconnect transitions until it gives up.
static void moq_source_watch_state(struct moq_source *ctx, const std::shared_ptr<Connection> &conn)
{
	conn->session_call =
		conn->session->state_changed(conn->revision)
			.then(ctx->worker.Executor(),
			      moq_source_current<moq::expected<moq::ConnectionState>>(ctx, conn, moq_source_on_state));
}

static void moq_source_on_state(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				moq::expected<moq::ConnectionState> result)
{
	if (!result) {
		// Reconnecting gave up (or the relay refused us). Tear down every
		// subscription, not just the session, and blank to show the error.
		LOG_ERROR("MoQ session error: %s", result.error().to_string().c_str());
		moq_source_fail_locked(ctx);
		return;
	}

	conn->revision = result->revision;
	const auto &phase = result->phase.get_variant();
	std::visit(
		[](const auto &data) {
			if constexpr (!std::is_same_v<std::decay_t<decltype(data)>, moq::ConnectionPhase::kConnected>) {
				if (data.failure)
					LOG_WARNING("MoQ connection attempt failed: %s",
						    data.failure->to_string().c_str());
			}
		},
		phase);
	if (std::holds_alternative<moq::ConnectionPhase::kClosed>(phase)) {
		moq_source_fail_locked(ctx);
		return;
	}
	if (std::holds_alternative<moq::ConnectionPhase::kConnected>(phase))
		LOG_INFO("MoQ session connected (connect %llu)", (unsigned long long)result->connects);

	moq_source_watch_state(ctx, conn);
}

static void moq_source_next_catalog(struct moq_source *ctx, const std::shared_ptr<Connection> &conn)
{
	conn->catalog_call = conn->catalog->next().then(
		ctx->worker.Executor(), moq_source_instance_current<moq::expected<std::optional<moq::Catalog>>>(
						ctx, conn, moq_source_on_catalog));
}

static void moq_source_on_catalog(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				  moq::expected<std::optional<moq::Catalog>> result)
{
	if (!result) {
		LOG_ERROR("Catalog subscription error: %s", result.error().to_string().c_str());
		moq_source_blank_video(ctx);
		return;
	}
	if (!*result) {
		LOG_DEBUG("Catalog subscription closed cleanly");
		return;
	}

	LOG_INFO("Catalog update received");
	// Audio and video are independent catalog sections. Attempt both from this
	// update, even when either rendition is absent or unsupported.
	moq_source_subscribe_video(ctx, conn, **result);
	moq_source_subscribe_audio(ctx, conn, **result);
	moq_source_next_catalog(ctx, conn);
}

template<typename Rendition> using CatalogEntry = std::pair<const std::string, Rendition>;

// A catalog section's renditions best first, by the key `rank` gives each (highest first),
// with exact ties in name order: the order hang's `ranked()` gives.
template<typename Rendition, typename Rank>
static std::vector<const CatalogEntry<Rendition> *>
moq_source_ranked(const std::unordered_map<std::string, Rendition> &renditions, Rank rank)
{
	std::vector<const CatalogEntry<Rendition> *> ranked;
	ranked.reserve(renditions.size());
	for (const auto &entry : renditions)
		ranked.push_back(&entry);
	std::sort(ranked.begin(), ranked.end(), [&](const auto *a, const auto *b) {
		const auto x = rank(a->second);
		const auto y = rank(b->second);
		return x != y ? x > y : a->first < b->first;
	});
	return ranked;
}

// hang's `Video::ranked`: enabled, then preference, then coded area, then bitrate. An unknown
// size or bitrate ranks below a known one, and the area is 64-bit so a large picture never wraps.
static std::vector<const CatalogEntry<moq::Video> *>
moq_source_ranked_video(const std::unordered_map<std::string, moq::Video> &renditions)
{
	return moq_source_ranked(renditions, [](const moq::Video &config) {
		const uint64_t area = config.coded ? uint64_t{config.coded->width} * config.coded->height : 0;
		return std::make_tuple(config.enabled, config.preference, area, config.bitrate);
	});
}

// hang's `Audio::ranked`: enabled, then bitrate, then sample rate, then channel count.
static std::vector<const CatalogEntry<moq::Audio> *>
moq_source_ranked_audio(const std::unordered_map<std::string, moq::Audio> &renditions)
{
	return moq_source_ranked(renditions, [](const moq::Audio &config) {
		return std::make_tuple(config.enabled, config.bitrate, config.sample_rate, config.channel_count);
	});
}

// The ranked renditions to try in turn: the enabled ones, since a consumer must not select a
// disabled rendition.
template<typename Rendition>
static std::vector<const CatalogEntry<Rendition> *>
moq_source_candidates(std::vector<const CatalogEntry<Rendition> *> ranked)
{
	ranked.erase(std::remove_if(ranked.begin(), ranked.end(),
				    [](const auto *entry) { return !entry->second.enabled; }),
		     ranked.end());
	return ranked;
}

static void moq_source_on_video_subscribed(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
					   const std::shared_ptr<Track> &track,
					   moq::expected<std::shared_ptr<moq::VideoConsumer>> result);
static void moq_source_read_video(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				  const std::shared_ptr<Track> &track);
static void moq_source_on_video(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				const std::shared_ptr<Track> &track,
				moq::expected<std::optional<moq::VideoRead>> result);

// The track, while it is still the connection's current video or audio track.
static std::shared_ptr<Track> moq_source_track(const Connection &conn, const std::weak_ptr<Track> &weak)
{
	auto track = weak.lock();
	return track && (track == conn.video || track == conn.audio || track == conn.pending_video ||
			 track == conn.pending_audio)
		       ? track
		       : nullptr;
}

// Wraps a track's continuation so it runs only while `track` is still current.
template<typename Output>
static std::function<void(Output)>
moq_source_track_current(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
			 const std::shared_ptr<Track> &track,
			 void (*callback)(struct moq_source *, const std::shared_ptr<Connection> &,
					  const std::shared_ptr<Track> &, Output))
{
	return moq_source_current<Output>(ctx, conn,
					  [weak = std::weak_ptr<Track>(track),
					   callback](struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
						     Output output) {
						  if (auto track = moq_source_track(*conn, weak))
							  callback(ctx, conn, track, std::move(output));
					  });
}

// A failed candidate owns no active playback state and can be released immediately.
static void moq_source_discard_candidate(const std::shared_ptr<Connection> &conn, const std::shared_ptr<Track> &track)
{
	if (track == conn->pending_video)
		conn->pending_video.reset();
	if (track == conn->pending_audio)
		conn->pending_audio.reset();
}

// moq-video decodes H.264, HEVC, and AV1. VP8 and VP9 are refused here so a later
// rendition can play; handing them to the decoder would fail the same way, later.
static VideoSupport moq_source_video_support(const std::string &codec)
{
	auto starts = [&](const char *prefix) {
		const size_t length = std::strlen(prefix);
		return codec.size() >= length && strncasecmp(codec.c_str(), prefix, length) == 0;
	};
	if (starts("h264") || starts("avc"))
		return VideoSupport::Decode;
	if (starts("hevc") || starts("h265") || starts("hev1") || starts("hvc1"))
		return VideoSupport::Decode;
	if (starts("av01") || starts("av1"))
		return VideoSupport::Decode;
	if (starts("vp09") || starts("vp9") || starts("vp8"))
		return VideoSupport::Deferred;
	return VideoSupport::Unknown;
}

static std::optional<PresentColor> moq_source_present_color(const moq::VideoDecodedFrame &frame)
{
	const auto color = frame.color();
	if (!color)
		return std::nullopt;
	PresentColor out;
	switch (color->matrix) {
	case moq::VideoColorMatrix::kBt601:
		out.matrix = PresentMatrix::Bt601;
		break;
	case moq::VideoColorMatrix::kBt709:
		out.matrix = PresentMatrix::Bt709;
		break;
	case moq::VideoColorMatrix::kBt2020:
		out.matrix = PresentMatrix::Bt2020;
		break;
	default:
		return std::nullopt;
	}
	switch (color->range) {
	case moq::VideoColorRange::kLimited:
		out.range = PresentRange::Limited;
		break;
	case moq::VideoColorRange::kFull:
		out.range = PresentRange::Full;
		break;
	default:
		return std::nullopt;
	}
	switch (color->transfer) {
	case moq::VideoColorTransfer::kBt709:
		out.transfer = PresentTransfer::Bt709;
		break;
	case moq::VideoColorTransfer::kPq:
		out.transfer = PresentTransfer::Pq;
		break;
	case moq::VideoColorTransfer::kHlg:
		out.transfer = PresentTransfer::Hlg;
		break;
	default:
		return std::nullopt;
	}
	return out;
}

// `known` wins. Otherwise the height picks BT.601 or BT.709, both limited.
static void moq_source_apply_color(obs_source_frame &out, const std::optional<PresentColor> &known, uint32_t height)
{
	const PresentOutput mapped = present_output(known, height);
	enum video_colorspace space = VIDEO_CS_709;
	switch (mapped.space) {
	case PresentSpace::Bt601:
		space = VIDEO_CS_601;
		break;
	case PresentSpace::Bt709:
		space = VIDEO_CS_709;
		break;
	case PresentSpace::Pq:
		space = VIDEO_CS_2100_PQ;
		break;
	case PresentSpace::Hlg:
		space = VIDEO_CS_2100_HLG;
		break;
	}
	const enum video_range_type range = mapped.full_range ? VIDEO_RANGE_FULL : VIDEO_RANGE_PARTIAL;
	video_format_get_parameters(space, range, out.color_matrix, out.color_range_min, out.color_range_max);
	out.full_range = mapped.full_range;
	switch (mapped.transfer) {
	case PresentTransfer::Pq:
		out.trc = VIDEO_TRC_PQ;
		break;
	case PresentTransfer::Hlg:
		out.trc = VIDEO_TRC_HLG;
		break;
	case PresentTransfer::Bt709:
		out.trc = VIDEO_TRC_DEFAULT;
		break;
	}
}

// A new track's first picture retires whatever the previous rendition had queued.
static void moq_source_begin_track(struct moq_source *ctx, const std::shared_ptr<Track> &track)
{
	std::lock_guard<std::mutex> present(ctx->present_mutex);
	if (track->video_consumer)
		ctx->video_decoder_name = track->video_consumer->name();
	ctx->present.restart();
#ifdef __APPLE__
	moq_source_apple_release_frames_locked(ctx);
	ctx->display_width = 0;
	ctx->display_height = 0;
	ctx->media_clock = false;
#endif
}

#ifndef __APPLE__
// Windows and Linux have no decoder surface OBS can import, so the picture is I420 in
// the stream's color space. OBS copies it before this returns. The delay sample is the
// handoff, from this callback to that return: decode already finished inside next().
static bool moq_source_output_i420(struct moq_source *ctx, const moq::VideoDecodedFrame &frame)
{
	const auto arrived = std::chrono::steady_clock::now();
	auto pixels = frame.pixels(moq::VideoPixelFormat::kI420);
	const uint32_t width = frame.width();
	const uint32_t height = frame.height();
	if (!pixels || width == 0 || height == 0 || (width % 2) != 0 || (height % 2) != 0)
		return false;
	const size_t luma = static_cast<size_t>(width) * height;
	const size_t chroma = static_cast<size_t>(width / 2) * (height / 2);
	if (pixels->size() < luma + 2 * chroma)
		return false;

	obs_source_frame out = {};
	out.data[0] = pixels->data();
	out.data[1] = pixels->data() + luma;
	out.data[2] = pixels->data() + luma + chroma;
	out.linesize[0] = width;
	out.linesize[1] = width / 2;
	out.linesize[2] = width / 2;
	out.width = width;
	out.height = height;
	out.timestamp = moq_source_nanoseconds(frame.timestamp());
	out.format = VIDEO_FORMAT_I420;
	moq_source_apply_color(out, moq_source_present_color(frame), height);
	obs_source_output_video(ctx->source, &out);

	std::lock_guard<std::mutex> present(ctx->present_mutex);
	ctx->video_path = "CPU I420";
	const auto held = std::chrono::steady_clock::now() - arrived;
	ctx->video_delay.add(static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::nanoseconds>(held).count()));
	return true;
}
#endif

static void moq_source_on_video_subscribed(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
					   const std::shared_ptr<Track> &track,
					   moq::expected<std::shared_ptr<moq::VideoConsumer>> result)
{
	if (!result) {
		LOG_ERROR("Failed to subscribe to %s track: %s", track->kind, result.error().to_string().c_str());
		// A build with no platform surface refuses surface=true. Retry this rendition on the CPU
		// once, rather than skipping to a different rendition.
		if (track->surface && std::holds_alternative<moq::Error::kUnsupported>(result.error().get_variant())) {
			track->surface = false;
			moq_source_open_video(ctx, conn, track);
			return;
		}
		moq_source_discard_candidate(conn, track);
		if (std::holds_alternative<moq::Error::kUnsupportedConfig>(result.error().get_variant()))
			moq_source_try_video(ctx, conn, track->video_selection);
		return;
	}
	track->video_selection.reset();
	track->video_consumer = *result;
	LOG_INFO("Subscribed to %s track successfully", track->kind);
	moq_source_read_video(ctx, conn, track);
}

static void moq_source_read_video(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				  const std::shared_ptr<Track> &track)
{
	track->call = track->video_consumer->next().then(
		ctx->worker.Executor(), moq_source_track_current<moq::expected<std::optional<moq::VideoRead>>>(
						ctx, conn, track, moq_source_on_video));
}

// One decoded read. moq-video already reordered it and already flushed the pictures it was
// holding before a gap, an end, or the track closing, so those are not pictures to show.
// Closing a candidate that never showed a picture leaves the current rendition playing.
static void moq_source_on_video(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				const std::shared_ptr<Track> &track,
				moq::expected<std::optional<moq::VideoRead>> result)
{
	if (!result) {
		LOG_ERROR("%s track error: %s", track->kind, result.error().to_string().c_str());
		moq_source_discard_candidate(conn, track);
		return;
	}
	if (!*result) {
		LOG_DEBUG("%s track closed cleanly", track->kind);
		if (track == conn->pending_video && conn->video) {
			moq_source_discard_candidate(conn, track);
			return;
		}
		moq_source_discard_candidate(conn, track);
		return;
	}

	const auto *read = std::get_if<moq::VideoRead::kFrame>(&(*result)->get_variant());
	if (!read) {
		moq_source_read_video(ctx, conn, track);
		return;
	}

#ifndef __APPLE__
	if (!moq_source_output_i420(ctx, *read->frame)) {
		if (!track->started && track == conn->pending_video) {
			moq_source_discard_candidate(conn, track);
			return;
		}
		moq_source_read_video(ctx, conn, track);
		return;
	}
#endif
	const bool first = !track->started;
	if (track == conn->pending_video)
		conn->video = std::move(conn->pending_video);
	track->started = true;
	if (first)
		moq_source_begin_track(ctx, track);
#ifdef __APPLE__
	moq_source_apple_queue(ctx, read->frame);
#endif
	moq_source_read_video(ctx, conn, track);
}

// Stops a failed audio track, leaving a pending replacement to take over.
static void moq_source_stop_audio(const std::shared_ptr<Connection> &conn, const std::shared_ptr<Track> &track)
{
	moq_source_discard_candidate(conn, track);
	if (track == conn->audio)
		conn->audio.reset();
}

static void moq_source_read_audio(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				  const std::shared_ptr<Track> &track);

// Takes one decoded read under the source lock. A candidate's first frame commits it, and
// any failure stops the track. On a frame `audio` points into it, for output once the lock
// is released. A gap or encoder end needs nothing here: OBS buffers by timestamp, so the
// next frame resumes wherever its timestamp lands.
static bool moq_source_on_audio_read(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				     const std::shared_ptr<Track> &track,
				     const moq::expected<std::optional<moq::AudioRead>> &result,
				     obs_source_audio &audio)
{
	if (!result) {
		LOG_ERROR("Audio track error: %s", result.error().to_string().c_str());
		moq_source_stop_audio(conn, track);
		return false;
	}
	if (!*result) {
		LOG_DEBUG("Audio track closed cleanly");
		moq_source_discard_candidate(conn, track);
		return false;
	}

	const auto *read = std::get_if<moq::AudioRead::kFrame>(&(*result)->get_variant());
	if (!read) {
		moq_source_read_audio(ctx, conn, track);
		return false;
	}

	// moq-audio delivers whole planar float samples in the layout the track asked for.
	const auto &frame = read->frame;
	const auto &layout = track->audio_layout;
	const size_t plane = frame.data.size() / layout.channels;
	if (plane == 0 || plane % sizeof(float) != 0 || plane * layout.channels != frame.data.size()) {
		LOG_ERROR("Decoded audio frame of %zu bytes is not whole %u-channel samples", frame.data.size(),
			  layout.channels);
		moq_source_stop_audio(conn, track);
		return false;
	}
	for (uint32_t i = 0; i < layout.channels; i++)
		audio.data[i] = frame.data.data() + i * plane;
	audio.frames = static_cast<uint32_t>(plane / sizeof(float));
	audio.speakers = layout.speakers;
	audio.format = AUDIO_FORMAT_FLOAT_PLANAR;
	audio.samples_per_sec = layout.sample_rate;
	// The same broadcast timeline video stamps, in the nanoseconds OBS expects.
	audio.timestamp = moq_source_nanoseconds(frame.timestamp);

	if (track == conn->pending_audio)
		conn->audio = std::move(conn->pending_audio);
	if (++track->audio_frames == 1)
		LOG_INFO("First audio frame output: %u samples, %u Hz, %u ch", audio.frames, layout.sample_rate,
			 layout.channels);
	moq_source_read_audio(ctx, conn, track);
	return true;
}

// Reads the audio track one decoded frame at a time. Unlike the other continuations, this
// one hands the frame to OBS after releasing the source lock, so OBS never runs under it.
static void moq_source_read_audio(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				  const std::shared_ptr<Track> &track)
{
	track->call = track->audio_consumer->next().then(
		ctx->worker.Executor(),
		[ctx, weak_conn = std::weak_ptr<Connection>(conn),
		 weak_track = std::weak_ptr<Track>(track)](moq::expected<std::optional<moq::AudioRead>> result) {
			obs_source_audio audio = {};
			{
				std::lock_guard<std::mutex> lock(ctx->mutex);
				auto conn = weak_conn.lock();
				if (!conn || conn != ctx->connection)
					return;
				auto track = moq_source_track(*conn, weak_track);
				if (!track || !moq_source_on_audio_read(ctx, conn, track, result, audio))
					return;
			}
			// `result` owns the samples, and OBS copies them before returning. Destroy
			// waits out this continuation, so ctx is still alive.
			obs_source_output_audio(ctx->source, &audio);
		});
}

static void moq_source_on_audio_subscribed(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
					   const std::shared_ptr<Track> &track,
					   moq::expected<std::shared_ptr<moq::AudioConsumer>> result)
{
	if (!result) {
		LOG_ERROR("Failed to subscribe to Audio track: %s", result.error().to_string().c_str());
		moq_source_discard_candidate(conn, track);
		if (std::holds_alternative<moq::Error::kUnsupportedConfig>(result.error().get_variant()))
			moq_source_try_audio(ctx, conn, track->audio_selection);
		return;
	}
	track->audio_selection.reset();
	track->audio_consumer = *result;
	LOG_INFO("Subscribed to Audio track successfully");
	moq_source_read_audio(ctx, conn, track);
}

// The channels moq-audio should remix a rendition to, and where OBS puts them. A count
// names moq-audio's default layout by the WAVE convention: 3 is 2.1, 4 quad, 5 5.0, 6 5.1,
// 7 6.1, 8 7.1. OBS has no quad, 5.0, or 6.1 (its 4.0 and 4.1 put a center where quad has
// a back pair), so those are upmixed to the smallest OBS layout holding every speaker.
// Any other count has no speaker positions to place.
static std::optional<AudioLayout> moq_source_audio_layout(uint32_t channels)
{
	switch (channels) {
	case 1:
		return AudioLayout{1, SPEAKERS_MONO};
	case 2:
		return AudioLayout{2, SPEAKERS_STEREO};
	case 3:
		return AudioLayout{3, SPEAKERS_2POINT1};
	case 4:
	case 5:
	case 6:
		return AudioLayout{6, SPEAKERS_5POINT1};
	case 7:
	case 8:
		return AudioLayout{8, SPEAKERS_7POINT1};
	default:
		return std::nullopt;
	}
}

static bool moq_source_same_container(const moq::Container &a, const moq::Container &b)
{
	const auto &x = a.get_variant();
	const auto &y = b.get_variant();
	if (x.index() != y.index())
		return false;
	const auto *cmaf = std::get_if<moq::Container::kCmaf>(&x);
	return !cmaf || cmaf->init == std::get<moq::Container::kCmaf>(y).init;
}

// Whether `track` already plays this rendition. Metadata such as the label or bitrate may
// change; the track, its broadcast and everything its decoder was built from may not.
static bool moq_source_same_video(const Track *track, const std::string &id, const moq::Video &config)
{
	if (!track)
		return false;
	const auto &old = *track->video_config;
	// The same track under a new catalog ID is the same rendition.
	if (old.track.value_or(track->id) != config.track.value_or(id))
		return false;
	const bool same_coded =
		old.coded.has_value() == config.coded.has_value() &&
		(!old.coded || (old.coded->width == config.coded->width && old.coded->height == config.coded->height));
	return old.broadcast == config.broadcast && old.instance == config.instance && old.codec == config.codec &&
	       old.description == config.description && same_coded &&
	       moq_source_same_container(old.container, config.container);
}

static bool moq_source_same_audio(const Track *track, const std::string &id, const moq::Audio &config)
{
	if (!track)
		return false;
	const auto &old = *track->audio_config;
	// The same track under a new catalog ID is the same rendition.
	if (old.track.value_or(track->id) != config.track.value_or(id))
		return false;
	return old.broadcast == config.broadcast && old.instance == config.instance && old.codec == config.codec &&
	       old.description == config.description && old.sample_rate == config.sample_rate &&
	       old.channel_count == config.channel_count && moq_source_same_container(old.container, config.container);
}

// Subscribe to the best video rendition moq-video can decode. The current track keeps
// playing until a replacement delivers, and for good when no rendition can replace it.
static void moq_source_subscribe_video(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				       const moq::Catalog &catalog)
{
	const auto candidates = moq_source_candidates(moq_source_ranked_video(catalog.video));
	if (candidates.empty()) {
		LOG_INFO("Catalog has no enabled video rendition; audio only");
		moq_source_clear_video_locked(ctx);
		moq_source_blank_video(ctx);
		return;
	}

	auto selection = std::make_shared<VideoSelection>();
	selection->candidates.reserve(candidates.size());
	for (const auto *rendition : candidates)
		selection->candidates.emplace_back(rendition->first, rendition->second);
	moq_source_try_video(ctx, conn, selection);
}

static void moq_source_open_video(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				  const std::shared_ptr<Track> &track)
{
	ctx->video_subscriptions.fetch_add(1, std::memory_order_relaxed);
	moq::VideoDecoderOutput output{};
	output.surface = track->surface;
	track->call = moq::VideoConsumer::subscribe(conn->consumer, track->id, *track->video_config, output)
			      .then(ctx->worker.Executor(),
				    moq_source_track_current<moq::expected<std::shared_ptr<moq::VideoConsumer>>>(
					    ctx, conn, track, moq_source_on_video_subscribed));
}

static void moq_source_try_video(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				 const std::shared_ptr<VideoSelection> &selection)
{
	while (selection->next < selection->candidates.size()) {
		const auto *rendition = &selection->candidates[selection->next++];
		const auto &id = rendition->first;
		const auto &config = rendition->second;
		if (moq_source_same_video(conn->video.get(), id, config) && conn->video->instance == conn->instance) {
			conn->pending_video.reset();
			return;
		}
		if (moq_source_same_video(conn->pending_video.get(), id, config)) {
			if (!conn->pending_video->video_consumer)
				conn->pending_video->video_selection = selection;
			return;
		}

		switch (moq_source_video_support(config.codec)) {
		case VideoSupport::Deferred:
			LOG_ERROR("%s is not decoded by moq-video yet", config.codec.c_str());
			continue;
		case VideoSupport::Unknown:
			LOG_ERROR("Unknown or unsupported codec: '%s'", config.codec.c_str());
			continue;
		case VideoSupport::Decode:
			break;
		}

		LOG_INFO("Subscribing to video track (%s)", config.codec.c_str());
		auto track = std::make_shared<Track>("Video", id);
		track->instance = conn->instance;
		track->video_config = config;
		track->video_selection = selection;
#ifdef __APPLE__
		track->surface = true;
#endif
		conn->pending_video = track;
		moq_source_open_video(ctx, conn, track);
		return;
	}
	conn->pending_video.reset();
}

// Subscribe to the best audio rendition OBS has a speaker layout for, decoded by moq-audio
// to OBS's rate. A broadcast with no audio rendition stays video-only (not an error), and
// the current track keeps playing when no rendition can replace it.
static void moq_source_subscribe_audio(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				       const moq::Catalog &catalog)
{
	const auto candidates = moq_source_candidates(moq_source_ranked_audio(catalog.audio));
	if (candidates.empty()) {
		LOG_INFO("Catalog has no enabled audio rendition; video only");
		moq_source_clear_audio_locked(ctx);
		return;
	}

	auto selection = std::make_shared<AudioSelection>();
	selection->candidates.reserve(candidates.size());
	for (const auto *rendition : candidates)
		selection->candidates.emplace_back(rendition->first, rendition->second);
	moq_source_try_audio(ctx, conn, selection);
}

static void moq_source_try_audio(struct moq_source *ctx, const std::shared_ptr<Connection> &conn,
				 const std::shared_ptr<AudioSelection> &selection)
{
	while (selection->next < selection->candidates.size()) {
		const auto *rendition = &selection->candidates[selection->next++];
		const auto &id = rendition->first;
		const auto &config = rendition->second;
		if (moq_source_same_audio(conn->audio.get(), id, config) && conn->audio->instance == conn->instance) {
			conn->pending_audio.reset();
			return;
		}
		if (moq_source_same_audio(conn->pending_audio.get(), id, config)) {
			// Keep the in-flight setup, but fall through against the newest catalog if it refuses.
			if (!conn->pending_audio->audio_consumer)
				conn->pending_audio->audio_selection = selection;
			return;
		}

		// OBS's speaker layout is independent of the decoder's capabilities.
		auto layout = moq_source_audio_layout(config.channel_count);
		if (!layout) {
			LOG_ERROR("Unsupported audio channel count %u in rendition %s: OBS has no layout for it",
				  config.channel_count, id.c_str());
			continue;
		}
		if (!ctx->sample_rate) {
			LOG_ERROR("OBS has no audio output to play into");
			break;
		}
		layout->sample_rate = ctx->sample_rate;

		LOG_INFO("Subscribing to audio track (%s, %u Hz, %u ch)", config.codec.c_str(), config.sample_rate,
			 config.channel_count);
		auto track = std::make_shared<Track>("Audio", id);
		track->instance = conn->instance;
		track->audio_config = config;
		track->audio_layout = *layout;
		track->audio_selection = selection;
		conn->pending_audio = track;

		moq::AudioDecoderOutput output{};
		output.format = moq::AudioSampleFormat::kF32Planar;
		output.sample_rate = layout->sample_rate;
		output.channels = layout->channels;
		// Follows the rendition's broadcast and track itself, like moq_source_open_video.
		track->call =
			moq::AudioConsumer::subscribe(conn->consumer, id, config, output)
				.then(ctx->worker.Executor(),
				      moq_source_track_current<moq::expected<std::shared_ptr<moq::AudioConsumer>>>(
					      ctx, conn, track, moq_source_on_audio_subscribed));
		return;
	}
	conn->pending_audio.reset();
}

// NOTE: Caller must hold ctx->mutex.
static void moq_source_clear_video_locked(struct moq_source *ctx)
{
	if (ctx->connection) {
		ctx->connection->pending_video.reset();
		ctx->connection->video.reset();
	}
}

// NOTE: Caller must hold ctx->mutex.
static void moq_source_disconnect_locked(struct moq_source *ctx)
{
	moq_source_clear_video_locked(ctx);
	moq_source_clear_audio_locked(ctx);

	// Dropping the connection cancels whatever it was waiting on, including a wait
	// for a broadcast that is never announced. A subscriber has nothing to drain, so
	// the session closes at once.
	if (ctx->connection && ctx->connection->session)
		ctx->connection->session->cancel(0);
	ctx->connection.reset();
}

// Blanks the video preview and retires any picture still queued for the previous rendition.
static void moq_source_blank_video(struct moq_source *ctx)
{
	// Passing NULL to obs_source_output_video clears the async frame. On macOS the
	// custom render draws from the queue instead, so that queue is retired too.
	obs_source_output_video(ctx->source, NULL);
	std::lock_guard<std::mutex> present(ctx->present_mutex);
	ctx->present.restart();
#ifdef __APPLE__
	moq_source_apple_release_frames_locked(ctx);
	ctx->display_width = 0;
	ctx->display_height = 0;
	ctx->media_clock = false;
#endif
	LOG_DEBUG("Video preview blanked");
}

// NOTE: caller holds ctx->mutex.
static void moq_source_clear_audio_locked(struct moq_source *ctx)
{
	if (ctx->connection) {
		ctx->connection->pending_audio.reset();
		ctx->connection->audio.reset();
	}
}

#ifdef __APPLE__
static void moq_source_apple_release_held(moq_source::AppleHeld &held)
{
	if (held.converted)
		CVPixelBufferRelease(held.converted);
	held.converted = nullptr;
	held.frame.reset();
}

// Present mutex is held. Safe from any thread: the graphics thread is the one that
// destroys the texture and then releases what this moved aside.
static void moq_source_apple_release_frames_locked(struct moq_source *ctx)
{
	for (auto &entry : ctx->apple_waiting)
		ctx->apple_doomed.push_back(std::move(entry.second));
	ctx->apple_waiting.clear();
	if (ctx->apple_showing) {
		ctx->apple_doomed.push_back(std::move(*ctx->apple_showing));
		ctx->apple_showing.reset();
	}
	if (ctx->apple_retired) {
		ctx->apple_doomed.push_back(std::move(*ctx->apple_retired));
		ctx->apple_retired.reset();
	}
}

static void moq_source_apple_shutdown(struct moq_source *ctx)
{
	std::lock_guard<std::mutex> present(ctx->present_mutex);
	if (ctx->apple_texture)
		gs_texture_destroy(ctx->apple_texture);
	ctx->apple_texture = nullptr;
	if (ctx->apple_transfer)
		CFRelease(ctx->apple_transfer);
	ctx->apple_transfer = nullptr;
	moq_source_apple_release_frames_locked(ctx);
	for (auto &held : ctx->apple_doomed)
		moq_source_apple_release_held(held);
	ctx->apple_doomed.clear();
}

static uint64_t moq_source_media_now_locked(const struct moq_source *ctx)
{
	if (!ctx->media_clock || ctx->tick_ns < ctx->tick_base_ns)
		return ctx->media_clock ? ctx->media_base_pts : 0;
	return ctx->media_base_pts + (ctx->tick_ns - ctx->tick_base_ns);
}

static void moq_source_apple_queue(struct moq_source *ctx, std::shared_ptr<moq::VideoDecodedFrame> frame)
{
	std::lock_guard<std::mutex> present(ctx->present_mutex);
	PresentItem item;
	item.generation = ctx->present.generation();
	item.timestamp = moq_source_nanoseconds(frame->timestamp());
	item.width = frame->width();
	item.height = frame->height();
	std::vector<PresentItem> dropped;
	const uint64_t id = ctx->present.push(item, dropped);
	for (const auto &late : dropped) {
		auto it = ctx->apple_waiting.find(late.id);
		if (it == ctx->apple_waiting.end())
			continue;
		moq_source_apple_release_held(it->second);
		ctx->apple_waiting.erase(it);
	}
	if (!id)
		return;
	if (!ctx->media_clock) {
		ctx->media_base_pts = item.timestamp;
		ctx->tick_base_ns = ctx->tick_ns;
		ctx->media_clock = true;
	}
	ctx->display_width = item.width;
	ctx->display_height = item.height;
	moq_source::AppleHeld held;
	held.item = item;
	held.item.id = id;
	held.frame = std::move(frame);
	held.arrived = std::chrono::steady_clock::now();
	ctx->apple_waiting.emplace(id, std::move(held));
}

static void moq_source_apple_fail_locked(struct moq_source *ctx, const char *reason, uint32_t width, uint32_t height)
{
	if (ctx->apple_texture) {
		gs_texture_destroy(ctx->apple_texture);
		ctx->apple_texture = nullptr;
	}
	if (!ctx->present.gpu_blocked(width, height))
		LOG_INFO("Video delivery fell back: %s", reason);
	ctx->present.fail_gpu(reason, width, height);
	ctx->video_path = "CPU RGBA";
}

static bool moq_source_apple_upload_locked(struct moq_source *ctx, moq_source::AppleHeld &held)
{
	if (!held.frame)
		return false;
	auto pixels = held.frame->pixels(moq::VideoPixelFormat::kRgba);
	const uint32_t width = held.item.width;
	const uint32_t height = held.item.height;
	if (!pixels || pixels->size() < static_cast<size_t>(width) * height * 4)
		return false;
	const uint8_t *plane = pixels->data();
	if (ctx->apple_texture)
		gs_texture_destroy(ctx->apple_texture);
	ctx->apple_texture = gs_texture_create(width, height, GS_RGBA, 1, &plane, 0);
	held.frame.reset();
	ctx->video_path = "CPU RGBA";
	return ctx->apple_texture != nullptr;
}

// Copy a non-BGRA decoder buffer into an IOSurface-backed BGRA buffer. The source
// frame can be released once this returns; `dst` is owned by the caller.
static CVPixelBufferRef moq_source_apple_transfer_locked(struct moq_source *ctx, CVPixelBufferRef src, uint32_t width,
							 uint32_t height)
{
	if (!ctx->apple_transfer && VTPixelTransferSessionCreate(kCFAllocatorDefault, &ctx->apple_transfer) != noErr)
		return nullptr;
	CFMutableDictionaryRef io_props = CFDictionaryCreateMutable(
		kCFAllocatorDefault, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
	const void *keys[] = {kCVPixelBufferIOSurfacePropertiesKey};
	const void *values[] = {io_props};
	CFDictionaryRef attrs = CFDictionaryCreate(kCFAllocatorDefault, keys, values, 1, &kCFTypeDictionaryKeyCallBacks,
						   &kCFTypeDictionaryValueCallBacks);
	CVPixelBufferRef dst = nullptr;
	const CVReturn created =
		CVPixelBufferCreate(kCFAllocatorDefault, width, height, kCVPixelFormatType_32BGRA, attrs, &dst);
	CFRelease(attrs);
	CFRelease(io_props);
	if (created != kCVReturnSuccess || !dst)
		return nullptr;
	if (VTPixelTransferSessionTransferImage(ctx->apple_transfer, src, dst) != noErr) {
		CVPixelBufferRelease(dst);
		return nullptr;
	}
	return dst;
}

static bool moq_source_apple_bind_locked(struct moq_source *ctx, moq_source::AppleHeld &held)
{
	const uint32_t width = held.item.width;
	const uint32_t height = held.item.height;
	if (ctx->present.gpu_blocked(width, height))
		return moq_source_apple_upload_locked(ctx, held);

	CVPixelBufferRef buffer = nullptr;
	bool borrowed = false;
	if (held.frame) {
		if (const auto surface = held.frame->surface()) {
			if (const auto *pixel = std::get_if<moq::VideoSurface::kPixelBuffer>(&surface->get_variant())) {
				auto *src = reinterpret_cast<CVPixelBufferRef>(static_cast<uintptr_t>(pixel->pointer));
				if (src && CVPixelBufferGetPixelFormatType(src) == kCVPixelFormatType_32BGRA) {
					buffer = src;
					borrowed = true;
				} else if (src) {
					buffer = moq_source_apple_transfer_locked(ctx, src, width, height);
					if (buffer)
						held.converted = buffer;
				}
			}
		}
	} else {
		buffer = held.converted;
	}
	if (!buffer) {
		moq_source_apple_fail_locked(ctx, "no decoder surface", width, height);
		return moq_source_apple_upload_locked(ctx, held);
	}
	IOSurfaceRef io = CVPixelBufferGetIOSurface(buffer);
	if (!io) {
		moq_source_apple_fail_locked(ctx, "IOSurface missing", width, height);
		return moq_source_apple_upload_locked(ctx, held);
	}
	if (ctx->apple_texture && !gs_texture_rebind_iosurface(ctx->apple_texture, io)) {
		gs_texture_destroy(ctx->apple_texture);
		ctx->apple_texture = nullptr;
	}
	if (!ctx->apple_texture)
		ctx->apple_texture = gs_texture_create_from_iosurface(io);
	if (!ctx->apple_texture) {
		moq_source_apple_fail_locked(ctx, "OBS texture import failed", width, height);
		return moq_source_apple_upload_locked(ctx, held);
	}
	ctx->video_path = "GPU IOSurface";
	// The transfer owns its BGRA buffer now, so the decoder surface can go back.
	if (!borrowed)
		held.frame.reset();
	return true;
}

// Graphics thread, present mutex held. Releases the picture that stopped being sampled
// on the previous tick, then shows the newest picture due at the media clock.
static void moq_source_apple_sample_locked(struct moq_source *ctx)
{
	if (!ctx->apple_doomed.empty()) {
		if (ctx->apple_texture) {
			gs_texture_destroy(ctx->apple_texture);
			ctx->apple_texture = nullptr;
		}
		for (auto &held : ctx->apple_doomed)
			moq_source_apple_release_held(held);
		ctx->apple_doomed.clear();
	}
	if (ctx->apple_retired) {
		moq_source_apple_release_held(*ctx->apple_retired);
		ctx->apple_retired.reset();
	}

	const auto selection = ctx->present.select(moq_source_media_now_locked(ctx));
	for (const auto &late : selection.dropped) {
		auto it = ctx->apple_waiting.find(late.id);
		if (it == ctx->apple_waiting.end())
			continue;
		moq_source_apple_release_held(it->second);
		ctx->apple_waiting.erase(it);
	}
	if (selection.retire && ctx->apple_showing && ctx->apple_showing->item.id == selection.retire->id) {
		ctx->apple_retired = std::move(*ctx->apple_showing);
		ctx->apple_showing.reset();
	}
	if (!selection.changed || !selection.show)
		return;
	auto it = ctx->apple_waiting.find(selection.show->id);
	if (it == ctx->apple_waiting.end())
		return;
	ctx->apple_showing = std::move(it->second);
	ctx->apple_waiting.erase(it);
	moq_source_apple_bind_locked(ctx, *ctx->apple_showing);
	const auto held = std::chrono::steady_clock::now() - ctx->apple_showing->arrived;
	ctx->video_delay.add(static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::nanoseconds>(held).count()));
	ctx->display_width = ctx->apple_showing->item.width;
	ctx->display_height = ctx->apple_showing->item.height;
}

static void moq_source_video_tick(void *data, float seconds)
{
	auto *ctx = static_cast<struct moq_source *>(data);
	std::lock_guard<std::mutex> present(ctx->present_mutex);
	if (seconds > 0.f)
		ctx->tick_ns += static_cast<uint64_t>(static_cast<double>(seconds) * 1000000000.0);
	moq_source_apple_sample_locked(ctx);
}

static void moq_source_video_render(void *data, gs_effect_t *effect)
{
	UNUSED_PARAMETER(effect);
	auto *ctx = static_cast<struct moq_source *>(data);
	std::lock_guard<std::mutex> present(ctx->present_mutex);
	if (!ctx->apple_showing || ctx->apple_showing->item.generation != ctx->present.generation() ||
	    !ctx->apple_texture)
		return;
	gs_effect_t *draw = obs_get_base_effect(OBS_EFFECT_DEFAULT);
	gs_eparam_t *image = draw ? gs_effect_get_param_by_name(draw, "image") : nullptr;
	if (!image)
		return;
	gs_effect_set_texture(image, ctx->apple_texture);
	const uint32_t width = ctx->apple_showing->item.width;
	const uint32_t height = ctx->apple_showing->item.height;
	while (gs_effect_loop(draw, "Draw"))
		gs_draw_sprite(ctx->apple_texture, 0, width, height);
}

static uint32_t moq_source_get_width(void *data)
{
	auto *ctx = static_cast<struct moq_source *>(data);
	std::lock_guard<std::mutex> present(ctx->present_mutex);
	return ctx->display_width;
}

static uint32_t moq_source_get_height(void *data)
{
	auto *ctx = static_cast<struct moq_source *>(data);
	std::lock_guard<std::mutex> present(ctx->present_mutex);
	return ctx->display_height;
}
#endif

// Registration function
void register_moq_source()
{
	struct obs_source_info info = {};
	info.id = "moq_source";
	info.type = OBS_SOURCE_TYPE_INPUT;
	info.output_flags = OBS_SOURCE_ASYNC_VIDEO | OBS_SOURCE_AUDIO | OBS_SOURCE_DO_NOT_DUPLICATE;
#ifdef __APPLE__
	// A custom render replaces the async draw, so macOS draws both the IOSurface and
	// the CPU fallback itself. Other platforms leave this unset and stay on async I420.
	info.output_flags |= OBS_SOURCE_CUSTOM_DRAW;
	info.video_tick = moq_source_video_tick;
	info.video_render = moq_source_video_render;
	info.get_width = moq_source_get_width;
	info.get_height = moq_source_get_height;
#endif
	info.get_name = [](void *) -> const char * {
		return "Moq Source (MoQ)";
	};
	info.create = moq_source_create;
	info.destroy = moq_source_destroy;
	info.update = moq_source_update;
	info.get_defaults = moq_source_get_defaults;
	info.get_properties = moq_source_properties;

	obs_register_source(&info);
}
