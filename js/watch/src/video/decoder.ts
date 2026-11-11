import * as Catalog from "@moq/hang/catalog";
import * as Container from "@moq/hang/container";
import * as Util from "@moq/hang/util";
import type * as Moq from "@moq/net";
import { Time } from "@moq/net";
import {
	type Computed,
	type Dispose,
	Effect,
	type Getter,
	getter,
	type Inputs,
	type Readonlys,
	readonlys,
	Signal,
} from "@moq/signals";
import { base64ToBytes } from "../base64";

import type { Sync } from "../sync";
import {
	type DecoderConfig,
	decoderConfigKey,
	type PlaybackIdentity,
	playbackIdentity,
	supportCacheKey,
} from "./config";
import { caughtUp } from "./playhead";
import { rotateVideoDimensions } from "./presentation";
import type { Source } from "./source";

// The amount of time to wait before considering the video to be buffering.
const BUFFERING = Time.Milli(500);

export type DecoderInput = {
	/** Whether to download the video track. Wired from the renderer's output by the parent. */
	enabled: Getter<boolean>;

	/**
	 * Whether to pace playback off the clock, holding each decoded frame until its presentation
	 * time. Defaults to true.
	 *
	 * False paints on decode instead, trading smooth motion for the lowest latency the display can
	 * offer. The clock stays wired either way and keeps tracking receipts and rewinds, so anything
	 * else reading it (captions, the UI) still works and re-enabling this resumes cleanly.
	 */
	paced: Getter<boolean>;

	/**
	 * When true, parse A/53 `cc_data` from each encoded sample (Annex-B or
	 * length-prefixed AVCC/HVCC) before it is sent to WebCodecs and publish the
	 * result on {@link Decoder.out.ccData}. Off by default so existing players
	 * pay no parse cost.
	 */
	emitSei?: Getter<boolean>;
};

/** Cumulative video statistics since the decoder started. */
export interface Stats {
	/** Number of decoded frames. */
	frameCount: number;

	/** Number of encoded bytes received. */
	bytesReceived: number;
}

/** One CEA-608/708 pair parsed from A/53 cc_data (ATSC a/53, GA94 SEI). */
export interface CcDataPair {
	valid: boolean;
	type: number;
	data1: number;
	data2: number;
}

/** Coded-video caption data emitted when {@link DecoderInput.emitSei} is on. */
export interface CcDataEvent {
	/** Presentation timestamp of the encoded sample, in microseconds. */
	timestamp: Time.Micro;
	pairs: CcDataPair[];
	payloadBytes: number;
}

type DecoderOutput = {
	// The current frame to render.
	frame: Signal<VideoFrame | undefined>;

	// The timestamp of the current frame.
	timestamp: Signal<Time.Milli | undefined>;

	// The display size of the video in pixels, ideally sourced from the catalog.
	display: Signal<{ width: number; height: number } | undefined>;

	stalled: Signal<boolean>;
	stats: Signal<Stats | undefined>;

	// Combined buffered ranges (network jitter + decode buffer)
	buffered: Signal<Container.BufferedRanges>;

	/** Latest A/53 cc_data from the coded video, when emitSei is on. */
	ccData: Signal<CcDataEvent | undefined>;
};

/** Downloads video from a track and decodes it into {@link VideoFrame}s with WebCodecs. */
export class Decoder {
	readonly in: Readonlys<DecoderInput>;
	readonly source: Source;
	readonly sync: Sync;

	readonly #out: DecoderOutput = {
		frame: new Signal<VideoFrame | undefined>(undefined),
		timestamp: new Signal<Time.Milli | undefined>(undefined),
		display: new Signal<{ width: number; height: number } | undefined>(undefined),
		stalled: new Signal<boolean>(false),
		stats: new Signal<Stats | undefined>(undefined),
		buffered: new Signal<Container.BufferedRanges>([]),
		ccData: new Signal<CcDataEvent | undefined>(undefined),
	};
	readonly out = readonlys(this.#out);

	// The current track running, held so we can cancel it when the new track is ready.
	#active = new Signal<DecoderTrack | undefined>(undefined);
	readonly #identity: Computed<PlaybackIdentity | undefined>;

	#signals = new Effect();

	#clearCurrentFrame(): void {
		this.#out.frame.update((prev) => {
			prev?.close();
			return undefined;
		});
		this.#out.timestamp.set(undefined);
	}

	constructor(source: Source, sync: Sync, props?: Inputs<DecoderInput>) {
		this.in = {
			enabled: getter(props?.enabled ?? false),
			paced: getter(props?.paced ?? true),
			emitSei: getter(props?.emitSei ?? false),
		};

		this.source = source;
		this.sync = sync;
		this.#identity = this.#signals.computed((effect) => {
			const config = effect.get(this.source.out.config);
			return config ? playbackIdentity(config) : undefined;
		});

		this.#signals.run(this.#runPending.bind(this));
		this.#signals.run(this.#runActive.bind(this));
		this.#signals.run(this.#runDisplay.bind(this));
		this.#signals.run(this.#runBuffering.bind(this));
	}

	#runPending(effect: Effect): void {
		const values = effect.getAll([
			this.in.enabled,
			this.source.in.broadcast,
			this.source.out.track,
			this.#identity,
		]);
		if (!values) {
			// Close the active track when disabled (e.g. paused or not visible).
			// The pending cleanup won't do this because it was already promoted to #active.
			this.#active.set(undefined);
			return;
		}
		const [_, broadcast, track, identity] = values;

		// Honor a per-rendition `broadcast` override: subscribe on the resolved source
		// broadcast instead of the catalog's own broadcast.
		const active: Moq.Broadcast.Consumer | undefined = broadcast.relativeBroadcast(effect, identity.broadcast);
		if (!active) {
			// Going offline should clear the last rendered frame.
			this.#active.set(undefined);
			this.#clearCurrentFrame();
			this.#out.buffered.set([]);
			return;
		}

		// Start a new pending effect.
		let pending: DecoderTrack | undefined = new DecoderTrack({
			sync: this.sync,
			paced: this.in.paced,
			emitSei: this.in.emitSei ?? getter(false),
			broadcast: active,
			track,
			config: identity.decoder,
			stats: this.#out.stats,
		});

		effect.cleanup(() => pending?.close());

		effect.run((effect) => {
			if (!pending) return;

			const current = effect.get(this.#active);
			if (current) {
				// A zero timestamp is a rendered frame, not a missing one.
				const pendingTimestamp = effect.get(pending.timestamp);
				if (pendingTimestamp === undefined) return;

				// Hold off until the new rendition has caught up to the picture it's replacing.
				if (!caughtUp({ playhead: pendingTimestamp, active: effect.get(current.timestamp) })) return;
			}

			// Upgrade the pending track to active.
			// #runActive will be in charge of it now.
			this.#active.set(pending);
			pending = undefined;

			// This effect is done; close it to avoid a useless re-run.
			effect.close();
		});
	}

	#runActive(effect: Effect): void {
		const active = effect.get(this.#active);
		if (!active) {
			// Clear stale data when disabled (e.g. paused or not visible).
			this.#out.buffered.set([]);
			return;
		}

		effect.cleanup(() => active.close());

		// Clone the frame so we own it independently of the DecoderTrack.
		// proxy() would share the same reference, allowing the source to close our frame.
		effect.run((inner) => {
			const frame = inner.get(active.frame);
			this.#out.frame.update((prev) => {
				prev?.close();
				return frame?.clone();
			});
		});
		effect.proxy(this.#out.timestamp, active.timestamp);
		effect.proxy(this.#out.buffered, active.buffered);
		effect.proxy(this.#out.ccData, active.ccData);
	}

	#runDisplay(effect: Effect): void {
		const catalog = effect.get(this.source.out.catalog);
		if (!catalog) return;

		const display = catalog.display;
		if (display) {
			effect.set(this.#out.display, { width: display.width, height: display.height });
			return;
		}

		const frame = effect.get(this.#out.frame);
		if (!frame) return;

		effect.set(
			this.#out.display,
			rotateVideoDimensions({ width: frame.displayWidth, height: frame.displayHeight }, catalog.rotation),
		);
	}

	#runBuffering(effect: Effect): void {
		const enabled = effect.get(this.in.enabled);
		if (!enabled) return;

		const frame = effect.get(this.#out.frame);
		if (!frame) {
			this.#out.stalled.set(true);
			return;
		}

		this.#out.stalled.set(false);

		effect.timer(() => {
			this.#out.stalled.set(true);
		}, BUFFERING);
	}

	close() {
		this.#clearCurrentFrame();

		this.#signals.close();
	}

	// Whether the WebCodecs video decoder can play this config.
	static supported = supported;
}

interface DecoderTrackProps {
	sync: Sync;
	paced: Getter<boolean>;
	emitSei: Getter<boolean>;
	broadcast: Moq.Broadcast.Consumer;
	track: string;
	config: DecoderConfig;

	stats: Signal<Stats | undefined>;
}

class DecoderTrack {
	sync: Sync;
	paced: Getter<boolean>;
	emitSei: Getter<boolean>;
	broadcast: Moq.Broadcast.Consumer;
	track: string;
	config: DecoderConfig;
	stats: Signal<Stats | undefined>;
	ccData = new Signal<CcDataEvent | undefined>(undefined);

	timestamp = new Signal<Time.Milli | undefined>(undefined);
	frame = new Signal<VideoFrame | undefined>(undefined);

	// Network jitter + decode buffer.
	buffered = new Signal<Container.BufferedRanges>([]);

	// Decoded frames waiting to be rendered.
	#buffered = new Signal<Container.BufferedRanges>([]);

	// The container's reorder/skip budget, mirroring the sync buffer. Zero when unpaced,
	// matching the consumer's own default: don't wait, skip to the live edge.
	#latency = new Signal<Time.Milli>(Time.Milli.zero);

	// The last discontinuity count seen from the container consumer; doubles as a generation
	// so in-flight decodes from before a rewind can be dropped on output.
	#discontinuity = 0;

	#signals = new Effect();

	constructor(props: DecoderTrackProps) {
		this.sync = props.sync;
		this.paced = props.paced;
		this.emitSei = props.emitSei;
		this.broadcast = props.broadcast;
		this.track = props.track;
		this.config = props.config;
		this.stats = props.stats;

		this.#signals.run((effect) => {
			const paced = effect.get(this.paced);
			this.#latency.set(paced ? effect.get(this.sync.out.buffer) : Time.Milli.zero);
		});

		this.#signals.run(this.#run.bind(this));
	}

	#run(effect: Effect): void {
		const sub = this.broadcast.track(this.track).subscribe({ priority: Catalog.PRIORITY.video });
		effect.cleanup(() => sub.close());

		const decoder = new VideoDecoder({
			output: async (frame: VideoFrame) => {
				try {
					// The generation this frame was decoded in. If a rewind bumps it while we wait
					// below, this frame belongs to the reneged timeline and must be dropped.
					const generation = this.#discontinuity;

					const timestamp = Time.Milli.fromMicro(frame.timestamp as Time.Micro);
					if (timestamp < (this.timestamp.peek() ?? 0)) {
						// Late frame, don't render it.
						return;
					}

					if (this.paced.peek()) {
						// `received()` runs at submit, so a frame is anchored by the time it decodes.
						// Reaching here unanchored means a rewind reset the clock after this frame's
						// received(), so it predates the current timeline. Drop it: painting it would
						// set `timestamp` from the old timeline and late-reject the whole rewind.
						if (this.sync.out.reference.peek() === undefined) return;

						if (this.frame.peek() === undefined) {
							// Render something while we wait for the sync to catch up.
							this.frame.set(frame.clone());
						}

						if (!(await this.#park(timestamp, effect))) return;
						if (generation !== this.#discontinuity) return; // a rewind happened while waiting

						if (timestamp < (this.timestamp.peek() ?? 0)) {
							// Late frame, don't render it.
							// NOTE: This can happen when the ref is updated, such as on playback start.
							return;
						}
					}

					this.timestamp.set(timestamp);

					// Trim the decode buffer as frames are rendered
					this.#trimBuffered(timestamp);

					this.frame.update((prev) => {
						prev?.close();
						return frame.clone(); // avoid closing the frame here
					});
				} finally {
					frame.close();
				}
			},
			// TODO bubble up error
			error: (error) => {
				console.error(error);
				effect.close();
			},
		});
		effect.cleanup(() => {
			if (decoder.state !== "closed") decoder.close();
		});

		// Input processing - depends on container type
		if (this.config.container.kind === "cmaf") {
			this.#runCmaf(effect, sub, decoder);
		} else {
			this.#runLegacy(effect, sub, decoder);
		}
	}

	#runLegacy(effect: Effect, sub: Moq.Track.Subscriber, decoder: VideoDecoder): void {
		const format =
			this.config.container.kind === "loc" ? new Container.Loc.Format() : new Container.Legacy.Format();
		// Create consumer that reorders groups/frames up to the provided latency.
		const consumer = new Container.Consumer(sub, {
			format,
			latency: this.#latency,
		});
		effect.cleanup(() => consumer.close());

		// Combine network jitter buffer with decode buffer
		effect.run((inner) => {
			const network = inner.get(consumer.buffered);
			const decode = inner.get(this.#buffered);
			this.buffered.update(() => Container.mergeBufferedRanges(network, decode));
		});

		decoder.configure({
			codec: this.config.codec,
			description: this.config.description ? Util.Hex.toBytes(this.config.description) : undefined,
			displayAspectWidth: this.config.displayAspectWidth,
			displayAspectHeight: this.config.displayAspectHeight,
			optimizeForLatency: this.config.optimizeForLatency,
			// @ts-expect-error Only supported by Chrome, so the renderer has to flip manually.
			flip: false,
		});

		let previous: Time.Micro | undefined;

		effect.spawn(async () => {
			for (;;) {
				const next = await consumer.next();
				if (!next) break;

				// Publisher rewound: flush queued/in-flight video and re-anchor before decoding.
				if (this.#onDiscontinuity(next.discontinuity)) previous = undefined;

				const { frame } = next;
				if (!frame) continue; // The group is done

				// Mark that we received this frame right now.
				const timestamp = Time.Milli.fromMicro(frame.timestamp as Time.Micro);
				this.sync.received(timestamp, "video");

				this.#maybeEmitSei(frame.payload, frame.timestamp as Time.Micro);

				const chunk = new EncodedVideoChunk({
					type: frame.keyframe ? "key" : "delta",
					data: frame.payload,
					timestamp: frame.timestamp,
				});

				// Track both frame count and bytes received for stats in the UI
				this.stats.update((current) => ({
					frameCount: (current?.frameCount ?? 0) + 1,
					bytesReceived: (current?.bytesReceived ?? 0) + frame.payload.byteLength,
				}));

				// Track decode buffer: frames sent to decoder but not yet rendered. Only bridge from
				// the previous frame when the consumer says nothing is missing in between. Group ids
				// can't answer that: they aren't required to be sequential (some encoders derive them
				// from DTS), so adjacency neither proves continuity nor rules out a gap the consumer
				// skipped, and reporting a skipped span as decoded overstates the buffer.
				if (previous !== undefined && next.continuous) {
					this.#addBuffered(Time.Milli.fromMicro(previous), Time.Milli.fromMicro(frame.timestamp));
				}

				previous = frame.timestamp;

				decoder.decode(chunk);
			}
		});
	}

	#runCmaf(effect: Effect, sub: Moq.Track.Subscriber, decoder: VideoDecoder): void {
		const container = this.config.container;
		if (container.kind !== "cmaf") return;

		const initSegment = base64ToBytes(container.init);
		const init = Container.Cmaf.decodeInitSegment(initSegment);
		const description = this.config.description ? Util.Hex.toBytes(this.config.description) : init.description;

		const consumer = new Container.Consumer(sub, {
			format: new Container.Cmaf.Format(init),
			latency: this.#latency,
		});
		effect.cleanup(() => consumer.close());

		// Combine network jitter buffer with decode buffer
		effect.run((inner) => {
			const network = inner.get(consumer.buffered);
			const decode = inner.get(this.#buffered);
			this.buffered.update(() => Container.mergeBufferedRanges(network, decode));
		});

		// Configure decoder with description from catalog
		decoder.configure({
			codec: this.config.codec,
			description,
			displayAspectWidth: this.config.displayAspectWidth,
			displayAspectHeight: this.config.displayAspectHeight,
			optimizeForLatency: this.config.optimizeForLatency,
			// @ts-expect-error Only supported by Chrome, so the renderer has to flip manually.
			flip: false,
		});

		let previous: Time.Micro | undefined;

		effect.spawn(async () => {
			for (;;) {
				const next = await consumer.next();
				if (!next) break;

				// Publisher rewound: flush queued/in-flight video and re-anchor before decoding.
				if (this.#onDiscontinuity(next.discontinuity)) previous = undefined;

				const { frame } = next;
				if (!frame) continue;

				// Mark that we received this frame right now.
				const timestamp = Time.Milli.fromMicro(frame.timestamp);
				this.sync.received(timestamp, "video");

				// Track stats
				this.stats.update((current) => ({
					frameCount: (current?.frameCount ?? 0) + 1,
					bytesReceived: (current?.bytesReceived ?? 0) + frame.payload.byteLength,
				}));

				// Track decode buffer (see #runLegacy: bridge on the consumer's continuity signal,
				// never on group adjacency, which proves nothing about the timeline).
				if (previous !== undefined && next.continuous) {
					this.#addBuffered(Time.Milli.fromMicro(previous), Time.Milli.fromMicro(frame.timestamp));
				}

				previous = frame.timestamp;

				if (decoder.state === "closed") break;
				this.#maybeEmitSei(frame.payload, frame.timestamp as Time.Micro);
				decoder.decode(
					new EncodedVideoChunk({
						type: frame.keyframe ? "key" : "delta",
						data: frame.payload,
						timestamp: frame.timestamp,
					}),
				);
			}
		});
	}

	// React to the container consumer's discontinuity counter. On a change the publisher has
	// rewound the timeline, so drop what's queued downstream and re-anchor the shared clock
	// before the new utterance. Clearing `timestamp` is load-bearing: otherwise its stale high
	// value would late-reject the rewound (lower-timestamp) frames at the output guard. Bumping
	// the generation drops in-flight decodes on output. The held frame is left in place so the
	// last picture shows until the new keyframe renders, instead of flashing empty. Returns true
	// if a rewind was handled.
	#onDiscontinuity(count: number): boolean {
		if (count === this.#discontinuity) return false;
		this.#discontinuity = count;
		this.timestamp.set(undefined);
		this.#buffered.set([]);
		this.sync.reset();
		return true;
	}

	#maybeEmitSei(payload: BufferSource, timestamp: Time.Micro): void {
		if (!this.emitSei.peek()) return;
		const bytes = payload instanceof Uint8Array ? payload : new Uint8Array(payload as ArrayBuffer);
		const codec = this.config.codec.startsWith("hev") || this.config.codec.startsWith("hvc") ? "hevc" : "avc";
		const pairs = extractCcData(bytes, codec);
		if (!pairs.length) return;
		this.ccData.set({ timestamp, pairs, payloadBytes: bytes.byteLength });
	}

		// Sleep until the clock says to paint this frame, waking early if pacing is turned off or the
	// run is torn down. Returns false when the frame should be dropped.
	//
	// Turning pacing off has to release parked frames, otherwise a deep buffer holds them (and their
	// GPU memory) until deadlines that no longer apply. They paint rather than drop: a parked frame
	// is early, not stale, so it is the freshest picture available the moment pacing stops.
	//
	// Dispose the registration once the race settles: one that outlives the frame retains an entry
	// per decoded frame for as long as the track lives, which is unbounded during steady playback.
	async #park(timestamp: Time.Milli, effect: Effect): Promise<boolean> {
		let release: Dispose | undefined;
		try {
			const released = new Promise<boolean>((resolve) => {
				release = this.paced.changed(() => resolve(!this.paced.peek()));
			});
			const wait = this.sync.wait(timestamp).then(() => true);
			return (await Promise.race([wait, released, effect.cancel])) ?? false;
		} finally {
			release?.();
		}
	}

	// Add a range to the decode buffer (decoded, waiting to render)
	#addBuffered(start: Time.Milli, end: Time.Milli): void {
		if (start > end) return;

		this.#buffered.mutate((current) => {
			for (const range of current) {
				// Check if there's any overlap, then merge
				if (range.start <= end && range.end >= start) {
					range.start = Time.Milli.min(range.start, start);
					range.end = Time.Milli.max(range.end, end);
					return;
				}
			}

			current.push({ start, end });
			current.sort((a, b) => a.start - b.start);
		});
	}

	// Trim the decode buffer up to the rendered timestamp
	#trimBuffered(timestamp: Time.Milli): void {
		this.#buffered.mutate((current) => {
			while (current.length > 0) {
				if (current[0].end >= timestamp) {
					current[0].start = Time.Milli.max(current[0].start, timestamp);
					break;
				}
				current.shift();
			}
		});
	}

	close(): void {
		this.#signals.close();

		this.frame.update((prev) => {
			prev?.close();
			return undefined;
		});
	}
}

async function supported(config: Catalog.VideoConfig): Promise<boolean> {
	if (!Catalog.containerSupported(config.container)) {
		// `kind` is the literal "unknown" tag; the container the publisher actually named is in `raw`.
		const kind = config.container.kind === "unknown" ? config.container.raw.kind : config.container.kind;
		console.warn(`video: ignoring rendition with unknown container: ${kind}`);
		return false;
	}

	let description: Uint8Array | undefined;
	if (config.description) {
		description = Util.Hex.toBytes(config.description);
	} else if (config.container.kind === "cmaf") {
		try {
			description = Container.Cmaf.decodeInitSegment(base64ToBytes(config.container.init)).description;
		} catch (err) {
			// A malformed init segment means we can't extract the codec
			// description, so we can't probe support reliably. Reject the
			// track rather than letting isConfigSupported pass on a
			// description-less config and then having runCmaf fail later.
			console.warn(`video: malformed CMAF init segment for codec ${config.codec}`, err);
			return false;
		}
	}
	const { supported } = await VideoDecoder.isConfigSupported({
		codec: config.codec,
		description,
		displayAspectWidth: config.displayAspectWidth,
		displayAspectHeight: config.displayAspectHeight,
		optimizeForLatency: config.optimizeForLatency ?? true,
	});

	if (supported) return true;

	// Safari rejects `avc3.*` codec strings even though its H.264 decoder handles
	// inline SPS/PPS. Rewrite to `avc1.*` and retry; mutate config.codec so the
	// later `decoder.configure()` call uses the accepted string too.
	if (config.codec.startsWith("avc3.")) {
		const avc1 = `avc1.${config.codec.slice("avc3.".length)}`;
		const retry = await VideoDecoder.isConfigSupported({
			codec: avc1,
			description,
			displayAspectWidth: config.displayAspectWidth,
			displayAspectHeight: config.displayAspectHeight,
			optimizeForLatency: config.optimizeForLatency ?? true,
		});
		if (retry.supported) {
			config.codec = avc1;
			return true;
		}
	}

	return false;
}

Object.assign(supported, { [supportCacheKey]: decoderConfigKey });


/**
 * Pull A/53 GA94 `cc_data` pairs out of an Annex-B or 4-byte length-prefixed
 * (AVCC/HVCC) access unit. Exported so a page or test can reuse the same parse
 * the decoder uses when `emitSei` is on.
 */
export function extractCcData(buf: Uint8Array, codec: "avc" | "hevc" = "avc"): CcDataPair[] {
	const nals = looksLikeAvcc(buf) ? splitLengthPrefixed(buf) : splitAnnexB(buf);
	const pairs: CcDataPair[] = [];
	for (const nal of nals) pairs.push(...extractCcDataFromNal(nal, codec));
	return pairs;
}

function looksLikeAvcc(buf: Uint8Array): boolean {
	if (buf.length < 4) return false;
	const size = ((buf[0] << 24) | (buf[1] << 16) | (buf[2] << 8) | buf[3]) >>> 0;
	return size > 0 && size + 4 <= buf.length && buf[0] === 0 && buf[1] === 0;
}

function splitAnnexB(buf: Uint8Array): Uint8Array[] {
	const nals: Uint8Array[] = [];
	let i = 0;
	while (i + 3 < buf.length) {
		const sc3 = buf[i] === 0 && buf[i + 1] === 0 && buf[i + 2] === 1;
		const sc4 = i + 4 <= buf.length && buf[i] === 0 && buf[i + 1] === 0 && buf[i + 2] === 0 && buf[i + 3] === 1;
		if (!sc3 && !sc4) {
			i += 1;
			continue;
		}
		const start = i + (sc4 ? 4 : 3);
		let j = start;
		while (j + 3 < buf.length) {
			const n3 = buf[j] === 0 && buf[j + 1] === 0 && buf[j + 2] === 1;
			const n4 = j + 4 <= buf.length && buf[j] === 0 && buf[j + 1] === 0 && buf[j + 2] === 0 && buf[j + 3] === 1;
			if (n3 || n4) break;
			j += 1;
		}
		if (j === start && j + 3 >= buf.length) j = buf.length;
		else if (j + 3 >= buf.length) j = buf.length;
		nals.push(buf.subarray(start, j));
		i = j;
	}
	return nals;
}

function splitLengthPrefixed(buf: Uint8Array): Uint8Array[] {
	const nals: Uint8Array[] = [];
	let i = 0;
	while (i + 4 <= buf.length) {
		const size = ((buf[i] << 24) | (buf[i + 1] << 16) | (buf[i + 2] << 8) | buf[i + 3]) >>> 0;
		i += 4;
		if (size <= 0 || i + size > buf.length) break;
		nals.push(buf.subarray(i, i + size));
		i += size;
	}
	return nals;
}

function stripEmulationPrevention(nal: Uint8Array): Uint8Array {
	const out: number[] = [];
	for (let i = 0; i < nal.length; i++) {
		if (
			nal[i] === 0x03 &&
			i >= 2 &&
			nal[i - 1] === 0 &&
			nal[i - 2] === 0 &&
			i + 1 < nal.length &&
			nal[i + 1] <= 0x03
		) {
			continue;
		}
		out.push(nal[i]);
	}
	return Uint8Array.from(out);
}

function readSeiPayloads(rbsp: Uint8Array): Array<{ type: number; payload: Uint8Array }> {
	const out: Array<{ type: number; payload: Uint8Array }> = [];
	let i = 0;
	while (i < rbsp.length) {
		let type = 0;
		while (i < rbsp.length && rbsp[i] === 0xff) {
			type += 255;
			i += 1;
		}
		if (i >= rbsp.length) break;
		type += rbsp[i++];
		let length = 0;
		while (i < rbsp.length && rbsp[i] === 0xff) {
			length += 255;
			i += 1;
		}
		if (i >= rbsp.length) break;
		length += rbsp[i++];
		if (i + length > rbsp.length) break;
		out.push({ type, payload: rbsp.subarray(i, i + length) });
		i += length;
	}
	return out;
}

function parseAtscUserData(payload: Uint8Array): CcDataPair[] {
	// ITU-T T.35 country 0xB5, provider 0x0031, 'GA94', type 0x03
	if (payload.length < 8) return [];
	if (payload[0] !== 0xb5) return [];
	if (((payload[1] << 8) | payload[2]) !== 0x0031) return [];
	if (payload[3] !== 0x47 || payload[4] !== 0x41 || payload[5] !== 0x39 || payload[6] !== 0x34) return [];
	if (payload[7] !== 0x03) return [];
	if (payload.length < 10) return [];
	const count = payload[8] & 0x1f;
	const pairs: CcDataPair[] = [];
	let o = 9;
	for (let n = 0; n < count && o + 2 < payload.length; n++) {
		const marker = payload[o];
		pairs.push({
			valid: (marker & 0x04) !== 0,
			type: marker & 0x03,
			data1: payload[o + 1],
			data2: payload[o + 2],
		});
		o += 3;
	}
	return pairs;
}

function extractCcDataFromNal(raw: Uint8Array, codec: "avc" | "hevc"): CcDataPair[] {
	if (!raw.length) return [];
	let isSei = false;
	let headerSize = 1;
	if (codec === "hevc") {
		if (raw.length < 2) return [];
		const nalType = (raw[0] >> 1) & 0x3f;
		isSei = nalType === 39 || nalType === 40;
		headerSize = 2;
	} else {
		isSei = (raw[0] & 0x1f) === 6;
		headerSize = 1;
	}
	if (!isSei) return [];
	const rbsp = stripEmulationPrevention(raw.subarray(headerSize));
	const pairs: CcDataPair[] = [];
	for (const msg of readSeiPayloads(rbsp)) {
		if (msg.type === 4) pairs.push(...parseAtscUserData(msg.payload));
	}
	return pairs;
}
