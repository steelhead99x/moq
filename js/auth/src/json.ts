/**
 * JSON text for a key or key set, refusing a repeated member.
 *
 * `JSON.parse` keeps the last copy of a member. `moq-auth` uses serde, which refuses it,
 * so which value a key holds must not depend on which parser read it.
 *
 * @module
 */

/** Parse `text`, refusing a repeated object member at any depth. `invalid` is the error when the text is not JSON. */
export function parseJson(text: string, kind: "JWK" | "JWKS", invalid: string): unknown {
	if (hasDuplicateMember(text)) throw new Error(`Failed to parse ${kind}: duplicate member`);
	try {
		return JSON.parse(text);
	} catch {
		throw new Error(invalid);
	}
}

function hasDuplicateMember(text: string): boolean {
	try {
		const scan = new Scan(text);
		scan.value();
		return scan.duplicate;
	} catch {
		// Malformed JSON is reported by JSON.parse. Only a repeated member is this function's result.
		return false;
	}
}

class Scan {
	duplicate = false;
	private i = 0;
	private readonly text: string;

	constructor(text: string) {
		this.text = text;
	}

	value(): void {
		this.skip();
		const c = this.text[this.i];
		if (c === "{") this.object();
		else if (c === "[") this.array();
		else if (c === '"') this.string();
		else if (c === "-" || (c !== undefined && c >= "0" && c <= "9")) this.number();
		else if (this.text.startsWith("true", this.i)) this.i += 4;
		else if (this.text.startsWith("false", this.i)) this.i += 5;
		else if (this.text.startsWith("null", this.i)) this.i += 4;
		else throw new Error("bad json");
	}

	private object(): void {
		this.i++;
		const seen = new Set<string>();
		this.skip();
		if (this.text[this.i] === "}") {
			this.i++;
			return;
		}
		for (;;) {
			this.skip();
			const key = this.string();
			if (seen.has(key)) this.duplicate = true;
			seen.add(key);
			this.skip();
			if (this.text[this.i] !== ":") throw new Error("bad json");
			this.i++;
			this.value();
			this.skip();
			if (this.text[this.i] === "}") {
				this.i++;
				return;
			}
			if (this.text[this.i] !== ",") throw new Error("bad json");
			this.i++;
		}
	}

	private array(): void {
		this.i++;
		this.skip();
		if (this.text[this.i] === "]") {
			this.i++;
			return;
		}
		for (;;) {
			this.value();
			this.skip();
			if (this.text[this.i] === "]") {
				this.i++;
				return;
			}
			if (this.text[this.i] !== ",") throw new Error("bad json");
			this.i++;
		}
	}

	private string(): string {
		if (this.text[this.i] !== '"') throw new Error("bad json");
		this.i++;
		let out = "";
		while (this.i < this.text.length) {
			const c = this.text[this.i++];
			if (c === '"') return out;
			if (c !== "\\") {
				out += c;
				continue;
			}
			const e = this.text[this.i++];
			if (e === "u") {
				const hex = this.text.slice(this.i, this.i + 4);
				if (!/^[0-9a-fA-F]{4}$/.test(hex)) throw new Error("bad json");
				out += String.fromCharCode(Number.parseInt(hex, 16));
				this.i += 4;
				continue;
			}
			const decoded = { b: "\b", f: "\f", n: "\n", r: "\r", t: "\t", '"': '"', "\\": "\\", "/": "/" }[e];
			if (decoded === undefined) throw new Error("bad json");
			out += decoded;
		}
		throw new Error("bad json");
	}

	private number(): void {
		this.i++;
		while (this.i < this.text.length && /[0-9eE+.-]/.test(this.text[this.i])) this.i++;
	}

	private skip(): void {
		while (this.i < this.text.length && " \n\r\t".includes(this.text[this.i])) this.i++;
	}
}
