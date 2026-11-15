import { describe, expect, it } from "bun:test";
import { extractCcData } from "./decoder";

// Annex-B AVC SEI (type 6) carrying one valid GA94 pair: 0x11, 0x22.
function ga94AnnexB(): Uint8Array {
	const payload = [0xb5, 0x00, 0x31, 0x47, 0x41, 0x39, 0x34, 0x03, 0x01, 0x04, 0x11, 0x22];
	// A 3-byte start code. A 4-byte start is also a plausible AVCC length, so the
	// detector would not take the Annex-B path.
	return new Uint8Array([0x00, 0x00, 0x01, 0x06, 0x04, payload.length, ...payload, 0x80]);
}

describe("extractCcData", () => {
	it("reads a GA94 pair from an Annex-B SEI", () => {
		expect(extractCcData(ga94AnnexB())).toEqual([{ valid: true, type: 0, data1: 0x11, data2: 0x22 }]);
	});

	it("reads the same pair from a 4-byte length-prefixed access unit", () => {
		const nal = ga94AnnexB().subarray(3);
		const prefixed = new Uint8Array(4 + nal.byteLength);
		prefixed[3] = nal.byteLength;
		prefixed.set(nal, 4);
		expect(extractCcData(prefixed)).toEqual([{ valid: true, type: 0, data1: 0x11, data2: 0x22 }]);
	});

	it("returns nothing for a slice NAL", () => {
		expect(extractCcData(new Uint8Array([0x00, 0x00, 0x00, 0x01, 0x65, 0x88]))).toEqual([]);
	});
});
