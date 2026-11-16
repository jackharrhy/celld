const assert = (value, message) => { if (!value) throw Error(message); };
const equal = (a, b) => a.length === b.length && a.every((byte, index) => byte === b[index]);
const content = Uint8Array.from({ length: 8193 }, (_, i) => i % 251);

export class State {
  constructor(state) { this.state = state; }
  async fetch() {
    const sql = this.state.storage.sql;
    sql.exec("CREATE TABLE IF NOT EXISTS receipts (value INTEGER NOT NULL)");
    sql.exec("INSERT INTO receipts VALUES (1)");
    const [{ count }] = [...sql.exec("SELECT COUNT(*) AS count FROM receipts")];
    await this.state.storage.put("last", count);
    assert(await this.state.storage.get("last") === count, "DO KV roundtrip");
    return Response.json({ count });
  }
}
export default {
  async fetch(request, env) {
    try {
      const expected = Number(new URL(request.url).searchParams.get("expected"));
      assert(expected === 1 || expected === 2, "isolated test phase");
      assert(env.TEST_SECRET === (expected === 1 ? "test-only-runtime-value" : "manifest-default"), "runtime secret override must not persist");
      if (expected === 1) {
        await env.FILES.put("qualification/full", content, {
          httpMetadata: { contentType: "application/octet-stream" },
          customMetadata: { marker: "fixture-only" },
        });
        const upload = await env.FILES.createMultipartUpload("qualification/multipart");
        const part = new Uint8Array(6 * 1024 * 1024).fill(37);
        const first = await upload.uploadPart(1, part);
        const last = await upload.uploadPart(2, new Uint8Array([1, 2, 3]));
        await upload.complete([first, last]);
      }
      const full = await env.FILES.get("qualification/full");
      assert(full && full.size === content.length, "R2 full get metadata");
      assert(full.httpMetadata.contentType === "application/octet-stream", "R2 HTTP metadata");
      assert(full.customMetadata.marker === "fixture-only", "R2 custom metadata");
      assert(equal(new Uint8Array(await full.arrayBuffer()), content), "R2 full get payload");
      const head = await env.FILES.head("qualification/full");
      assert(head.size === content.length && head.etag === full.etag, "R2 HEAD");
      const range = await env.FILES.get("qualification/full", { range: { offset: 7, length: 19 } });
      assert(equal(new Uint8Array(await range.arrayBuffer()), content.slice(7, 26)), "R2 bounded range");
      const suffix = await env.FILES.get("qualification/full", { range: { suffix: 11 } });
      assert(equal(new Uint8Array(await suffix.arrayBuffer()), content.slice(-11)), "R2 suffix range");
      const listed = await env.FILES.list({ prefix: "qualification/" });
      assert(listed.objects.length === 2, "R2 list");
      const multipart = await env.FILES.get("qualification/multipart");
      const body = new Uint8Array(await multipart.arrayBuffer());
      assert(body.length === 6 * 1024 * 1024 + 3 && body[0] === 37 && body[body.length - 4] === 37, "R2 multipart body");
      assert(equal(body.slice(-3), new Uint8Array([1, 2, 3])), "R2 multipart tail");
      const response = await env.STATE.get(env.STATE.idFromName("persistent")).fetch("https://fixture.invalid/");
      const { count } = await response.json();
      assert(count === expected, "SQL replicated restore count");
      return Response.json({ ok: true, count, r2Bytes: content.length, multipartBytes: body.length, runtimeSecret: "checked" });
    } catch (error) {
      return Response.json({ ok: false, name: error.name, message: error.message }, { status: 500 });
    }
  },
};
