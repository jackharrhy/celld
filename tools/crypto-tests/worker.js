import { rfcVectors, freshVapid } from "./rfc-vectors.js";

const assert = (condition, message) => { if (!condition) throw Error(message); };
const equal = (a, b) => {
  a = new Uint8Array(a); b = new Uint8Array(b);
  return a.length === b.length && a.every((value, index) => value === b[index]);
};
async function rejects(name, operation) {
  try { await operation(); } catch (error) {
    assert(error.name === name, `expected ${name}, got ${error.name}: ${error.message}`);
    return;
  }
  throw Error(`expected ${name}`);
}

// Run inside the embedded Worker, not as a host WebCrypto substitute.
export default {
  async fetch() {
    const results = [];
    try {
      const subtle = crypto.subtle;
      for (const [namedCurve, size, bits, hash] of [
        ["P-256", 65, 256, "SHA-256"],
        ["P-384", 97, 384, "SHA-384"],
        ["P-521", 133, 528, "SHA-512"],
      ]) {
        const algorithm = { name: "ECDH", namedCurve };
        const own = await subtle.generateKey(algorithm, true, ["deriveBits"]);
        const peer = await subtle.generateKey(algorithm, true, ["deriveBits"]);
        const point = new Uint8Array(await subtle.exportKey("raw", peer.publicKey));
        assert(point.length === size && point[0] === 4, `${namedCurve} SEC1 encoding`);
        const imported = await subtle.importKey("raw", point, algorithm, true, []);
        assert(imported.type === "public" && imported.algorithm.namedCurve === namedCurve, "key metadata");
        assert(equal(await subtle.exportKey("raw", imported), point), "raw roundtrip");
        const secret = await subtle.deriveBits({ name: "ECDH", public: imported }, own.privateKey, bits);
        assert(equal(secret, await subtle.deriveBits({ name: "ECDH", public: own.publicKey }, peer.privateKey, bits)), "ECDH agreement");
        for (const format of ["spki", "jwk"]) {
          const encoded = await subtle.exportKey(format, peer.publicKey);
          const restored = await subtle.importKey(format, encoded, algorithm, true, []);
          assert(equal(await subtle.exportKey("raw", restored), point), `${format} to raw`);
        }
        const padded = new Uint8Array(size + 6); padded.set(point, 3);
        for (const view of [padded.subarray(3, size + 3), new DataView(padded.buffer, 3, size)]) {
          const restored = await subtle.importKey("raw", view, algorithm, true, []);
          assert(equal(await subtle.exportKey("raw", restored), point), "BufferSource offsets");
        }
        const hidden = await subtle.importKey("raw", point, algorithm, false, []);
        await rejects("InvalidAccessError", () => subtle.exportKey("raw", hidden));
        await rejects("InvalidAccessError", () => subtle.exportKey("raw", own.privateKey));
        await rejects("SyntaxError", () => subtle.importKey("raw", point, algorithm, true, ["deriveBits"]));
        await rejects("TypeError", () => subtle.importKey("raw", {}, algorithm, true, []));
        await rejects("NotSupportedError", () => subtle.importKey("raw", point, { name: "ECDH", namedCurve: "P-999" }, true, []));
        const offCurve = new Uint8Array(size); offCurve[0] = 4;
        const oversized = new Uint8Array(size + 1); oversized.set(point);
        const compressed = new Uint8Array((size + 1) / 2); compressed[0] = 2;
        compressed.set(point.subarray(1, compressed.length), 1);
        for (const invalid of [new Uint8Array(), point.slice(0, -1), oversized, new Uint8Array([0]), offCurve, compressed]) {
          await rejects("DataError", () => subtle.importKey("raw", invalid, algorithm, true, []));
        }
        const wrongCurve = namedCurve === "P-256" ? "P-384" : "P-256";
        await rejects("DataError", () => subtle.importKey("raw", point, { name: "ECDH", namedCurve: wrongCurve }, true, []));
        const signing = await subtle.generateKey({ name: "ECDSA", namedCurve }, true, ["sign", "verify"]);
        const verifyKey = await subtle.importKey("raw", await subtle.exportKey("raw", signing.publicKey), { name: "ECDSA", namedCurve }, true, ["verify"]);
        const message = new TextEncoder().encode("isolated raw EC regression");
        assert(equal(await subtle.exportKey("raw", verifyKey), await subtle.exportKey("raw", signing.publicKey)), "raw ECDSA key roundtrip");
        // Upstream WebCrypto signing supports only P-256; raw key conversion
        // must not silently expand that separate algorithm contract.
        if (namedCurve === "P-256") {
          const signature = await subtle.sign({ name: "ECDSA", hash }, signing.privateKey, message);
          assert(signature.byteLength === 64, "P1363 signature width");
          assert(await subtle.verify({ name: "ECDSA", hash }, verifyKey, signature, message), "raw ECDSA verification");
        }
        await rejects("SyntaxError", () => subtle.importKey("raw", point, { name: "ECDSA", namedCurve }, true, ["sign"]));
        results.push(namedCurve);
      }
      const vectors = await rfcVectors();
      return Response.json({ ok: true, curves: results, vectors, vapid: await freshVapid() });
    } catch (error) {
      return Response.json({ ok: false, curves: results, name: error.name, message: error.message }, { status: 500 });
    }
  },
};
