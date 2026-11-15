// SPDX-License-Identifier: GPL-2.0-or-later
//
// The presentation queue, the GPU-fallback latch, and the color mapping, without
// OBS or a decoder. `just obs test` compiles this with the other unit tests.
#include "moq-source-present.h"

#include <cstdio>

namespace {
int g_failures = 0;
}

#define CHECK(cond)                                                                                        \
	do {                                                                                               \
		if (!(cond)) {                                                                             \
			fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);                   \
			g_failures++;                                                                      \
		}                                                                                          \
	} while (0)

static PresentItem item(uint64_t generation, uint64_t timestamp, uint32_t width = 64, uint32_t height = 64)
{
	PresentItem out;
	out.generation = generation;
	out.timestamp = timestamp;
	out.width = width;
	out.height = height;
	return out;
}

static void test_queue()
{
	PresentQueue queue;
	std::vector<PresentItem> dropped;
	CHECK(queue.push(item(0, 1), dropped) == 0);
	CHECK(dropped.empty());

	const uint64_t generation = queue.generation();
	CHECK(queue.push(item(generation, 30), dropped) != 0);
	auto early = queue.select(10);
	CHECK(!early.show);
	CHECK(queue.pending() == 1);

	CHECK(queue.push(item(generation, 20), dropped) != 0);
	CHECK(queue.push(item(generation, 40), dropped) != 0);
	auto due = queue.select(30);
	CHECK(due.changed);
	CHECK(due.show && due.show->timestamp == 30);
	CHECK(due.dropped.size() == 1 && due.dropped[0].timestamp == 20);
	CHECK(queue.pending() == 1);

	auto again = queue.select(30);
	CHECK(!again.changed);
	CHECK(again.show && again.show->timestamp == 30);
	CHECK(!again.retire);

	auto next = queue.select(40);
	CHECK(next.changed);
	CHECK(next.show && next.show->timestamp == 40);
	CHECK(next.retire && next.retire->timestamp == 30);
	CHECK(queue.pending() == 0);
}

static void test_bound_and_generation()
{
	PresentQueue queue;
	std::vector<PresentItem> dropped;
	const uint64_t generation = queue.generation();
	for (uint64_t timestamp = 1; timestamp <= 5; timestamp++)
		CHECK(queue.push(item(generation, timestamp), dropped) != 0);
	CHECK(dropped.size() == 1);
	CHECK(dropped[0].timestamp == 1);
	CHECK(queue.pending() == PresentQueue::kBound);

	queue.restart();
	CHECK(queue.generation() != generation);
	CHECK(queue.pending() == 0);
	CHECK(!queue.showing());
	dropped.clear();
	CHECK(queue.push(item(generation, 9), dropped) == 0);
}

static void test_gpu_fallback()
{
	PresentQueue queue;
	CHECK(!queue.gpu_blocked(64, 64));
	queue.fail_gpu("OBS texture import failed", 64, 64);
	CHECK(queue.gpu_blocked(64, 64));
	CHECK(!queue.gpu_blocked(128, 64));
	CHECK(queue.fallback() == "OBS texture import failed");

	std::vector<PresentItem> dropped;
	const uint64_t id = queue.push(item(queue.generation(), 1, 64, 64), dropped);
	CHECK(id != 0);
	auto shown = queue.select(1);
	CHECK(shown.show && shown.show->id == id);
	auto owned = queue.abandon();
	CHECK(owned.size() == 1 && owned[0].id == id);
	CHECK(!queue.showing());

	queue.restart();
	CHECK(!queue.gpu_blocked(64, 64));
	CHECK(queue.fallback().empty());
}

static void test_delay_and_color()
{
	DelayStats stats;
	CHECK(!stats.p50());
	stats.add(10);
	stats.add(30);
	stats.add(20);
	CHECK(stats.p50() == 20);
	CHECK(stats.p95() == 30);
	for (int i = 0; i < 70; i++)
		stats.add(100);
	CHECK(stats.size() == DelayStats::kCap);
	CHECK(stats.p50() == 100);

	const auto sd = present_output(std::nullopt, 480);
	CHECK(sd.space == PresentSpace::Bt601);
	CHECK(!sd.full_range);
	CHECK(sd.transfer == PresentTransfer::Bt709);

	const auto hd = present_output(std::nullopt, 720);
	CHECK(hd.space == PresentSpace::Bt709);

	PresentColor hdr;
	hdr.matrix = PresentMatrix::Bt2020;
	hdr.range = PresentRange::Full;
	hdr.transfer = PresentTransfer::Pq;
	const auto pq = present_output(hdr, 2160);
	CHECK(pq.space == PresentSpace::Pq);
	CHECK(pq.full_range);
	CHECK(pq.transfer == PresentTransfer::Pq);

	PresentColor hlg;
	hlg.matrix = PresentMatrix::Bt2020;
	hlg.range = PresentRange::Limited;
	hlg.transfer = PresentTransfer::Hlg;
	CHECK(present_output(hlg, 2160).space == PresentSpace::Hlg);

	PresentColor uhd_sdr;
	uhd_sdr.matrix = PresentMatrix::Bt2020;
	uhd_sdr.range = PresentRange::Limited;
	uhd_sdr.transfer = PresentTransfer::Bt709;
	CHECK(present_output(uhd_sdr, 2160).space == PresentSpace::Bt709);
}

int main()
{
	test_queue();
	test_bound_and_generation();
	test_gpu_fallback();
	test_delay_and_color();
	if (g_failures) {
		fprintf(stderr, "%d checks failed\n", g_failures);
		return 1;
	}
	printf("present queue: ok\n");
	return 0;
}
