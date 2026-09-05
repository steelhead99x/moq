import { expect, mock, test } from "bun:test";
import type * as Catalog from "@moq/hang/catalog";
import { Time } from "@moq/net";
import { Signal } from "@moq/signals";
import type { Source } from "./source";
import type { Sync } from "../sync";

mock.module("./render-worklet.ts?worklet", () => ({ default: "blob:fake-render" }));

const { Decoder } = await import("./decoder.ts");

const flush = () => new Promise<void>((resolve) => queueMicrotask(resolve));
async function settle(times = 5): Promise<void> {
	for (let i = 0; i < times; i++) await flush();
}

const catalog: Catalog.AudioConfig = {
	codec: "opus",
	sampleRate: 48000,
	numberOfChannels: 1,
	container: { kind: "legacy" },
};

function stubSource(): Source {
	return {
		out: { config: new Signal<Catalog.AudioConfig | undefined>(catalog) },
		close() {},
	} as unknown as Source;
}

function stubSync(): Sync {
	return {
		in: { latency: new Signal<"real-time">("real-time") },
		out: {
			buffer: new Signal(Time.Milli(100)),
			buffered: new Signal(false),
			maxBuffer: new Signal(Time.Milli(100)),
		},
		reset() {},
		close() {},
	} as unknown as Sync;
}

function installFakeWebAudio() {
	const addModule = () => new Promise<void>(() => {});
	const created: FakeAudioContext[] = [];

	class FakeAudioContext {
		state: AudioContextState = "suspended";
		sampleRate: number;
		audioWorklet = { addModule };
		closeCalls = 0;
		constructor(options?: AudioContextOptions) {
			this.sampleRate = options?.sampleRate ?? 48000;
			created.push(this);
		}
		close(): Promise<void> {
			this.closeCalls++;
			return Promise.resolve();
		}
	}

	class FakeAudioWorkletNode {
		constructor(_context: unknown, _name: string) {
			throw new DOMException("Unknown AudioWorklet name 'render'", "InvalidStateError");
		}
		disconnect(): void {}
	}

	const originals = new Map<string, PropertyDescriptor | undefined>();
	for (const [name, value] of Object.entries({
		AudioContext: FakeAudioContext,
		AudioWorkletNode: FakeAudioWorkletNode,
	})) {
		originals.set(name, Object.getOwnPropertyDescriptor(globalThis, name));
		Object.defineProperty(globalThis, name, { configurable: true, writable: true, value });
	}

	return {
		created,
		FakeAudioContext,
		[Symbol.dispose]() {
			for (const [name, original] of originals) {
				if (original) Object.defineProperty(globalThis, name, original);
				else Reflect.deleteProperty(globalThis, name);
			}
		},
	};
}

test("owns and closes a private AudioContext by default", async () => {
	using fake = installFakeWebAudio();
	const decoder = new Decoder(stubSource(), stubSync());
	await settle();
	expect(fake.created.length).toBe(1);
	decoder.close();
	await settle();
	expect(fake.created[0]?.closeCalls).toBe(1);
});

test("reuses an injected AudioContext and does not close it", async () => {
	using fake = installFakeWebAudio();
	const injected = new fake.FakeAudioContext({ sampleRate: 48000 }) as unknown as AudioContext;
	expect(fake.created.length).toBe(1);

	const a = new Decoder(stubSource(), stubSync(), { context: injected });
	const b = new Decoder(stubSource(), stubSync(), { context: injected });
	await settle();

	expect(fake.created.length).toBe(1);
	expect(a.out.context.peek()).toBe(injected);
	expect(b.out.context.peek()).toBe(injected);

	a.close();
	b.close();
	await settle();
	expect((injected as unknown as { closeCalls: number }).closeCalls).toBe(0);
});
