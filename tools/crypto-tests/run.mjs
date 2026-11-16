import assert from "node:assert/strict";
import { createHash, createPublicKey, verify } from "node:crypto";
import { createServer } from "node:net";
import { spawn } from "node:child_process";
import { mkdtemp, copyFile, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, join } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

const [binary, esbuild] = process.argv.slice(2);
assert(binary && isAbsolute(binary), "pass an absolute candidate celld path");
assert(esbuild && isAbsolute(esbuild), "pass an absolute native esbuild path");
const binarySha256 = createHash("sha256").update(await readFile(binary)).digest("hex");
const source = dirname(fileURLToPath(import.meta.url));
const children = [];
let logs = "";
let receipt;
const server = createServer();
await new Promise((resolve, reject) => {
  server.once("error", reject);
  server.listen(0, "127.0.0.1", resolve);
});
const port = server.address().port;
await new Promise((resolve) => server.close(resolve));
const scratch = await mkdtemp(join(tmpdir(), "celld-crypto-"));
const env = {
  PATH: "/usr/bin:/bin",
  HOME: scratch,
  TMPDIR: scratch,
  MALLOC_ARENA_MAX: "2",
  CELLD_BUCKET: `sqlite://${join(scratch, "objects.sqlite3")}`,
  CELLD_WATCH: join(scratch, "replica"),
  CELLD_DURABILITY: "bucket",
  CELLD_ADDR: `127.0.0.1:${port}`,
  CELLD_INTERNAL_ADDR: "127.0.0.1:0",
  CELLD_ESBUILD: esbuild,
};
function run(args) {
  const child = spawn(binary, args, { cwd: scratch, env, stdio: ["ignore", "pipe", "pipe"] });
  children.push(child);
  const finished = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code, signal) => resolve({ code, signal }));
  });
  // Attach the rejection handler before any asynchronous deployment work.
  finished.catch(() => {});
  for (const stream of [child.stdout, child.stderr]) {
    stream.on("data", (data) => { logs = (logs + data).slice(-65536); });
  }
  return { child, finished };
}
async function bounded(promise, milliseconds) {
  const controller = new AbortController();
  try {
    return await Promise.race([
      promise,
      delay(milliseconds, null, { signal: controller.signal }).then(() => { throw Error("candidate timeout"); }),
    ]);
  } finally { controller.abort(); }
}
try {
  for (const file of ["worker.js", "rfc-vectors.js", "wrangler.jsonc"]) {
    await copyFile(join(source, file), join(scratch, file));
  }
  const deployment = run(["deploy", join(scratch, "wrangler.jsonc")]);
  assert.deepEqual(await bounded(deployment.finished, 60000), { code: 0, signal: null }, "candidate deployment failed");
  const runtime = run(["--no-control-plane"]);
  let result;
  for (let attempt = 0; attempt < 200; attempt++) {
    assert.equal(runtime.child.exitCode, null, "candidate exited");
    assert.equal(runtime.child.signalCode, null, "candidate terminated");
    let response;
    try {
      response = await fetch(`http://127.0.0.1:${port}/`, { signal: AbortSignal.timeout(20000) });
    } catch (error) {
      if (error.name === "TimeoutError") throw error;
      await delay(100);
      continue;
    }
    result = await response.json();
    assert.equal(response.status, 200, JSON.stringify(result));
    break;
  }
  const { vapid, ...checks } = result ?? {};
  assert.deepEqual(checks, { ok: true, curves: ["P-256", "P-384", "P-521"], vectors: ["RFC8291", "RFC8292"] });
  const match = /^vapid t=([\w-]+)\.([\w-]+)\.([\w-]+), k=([\w-]+)$/.exec(vapid.authorization);
  assert(match, "VAPID authorization syntax");
  const [, header, claims, signatureText, publicText] = match;
  const point = Buffer.from(publicText, "base64url");
  const signature = Buffer.from(signatureText, "base64url");
  assert.equal(point.length, 65); assert.equal(point[0], 4);
  assert.equal(signature.length, 64);
  assert.deepEqual(JSON.parse(Buffer.from(header, "base64url")), { typ: "JWT", alg: "ES256" });
  assert.deepEqual(JSON.parse(Buffer.from(claims, "base64url")), {
    aud: "https://push.example.invalid", exp: 1700003600, sub: "mailto:fixture@example.invalid",
  });
  const key = createPublicKey({ format: "jwk", key: {
    kty: "EC", crv: "P-256", x: point.subarray(1, 33).toString("base64url"), y: point.subarray(33).toString("base64url"),
  } });
  const input = Buffer.from(`${header}.${claims}`);
  assert(verify("sha256", input, { key, dsaEncoding: "ieee-p1363" }, signature), "independent VAPID P1363 verification");
  const integer = (bytes) => {
    while (bytes.length > 1 && bytes[0] === 0) bytes = bytes.subarray(1);
    if (bytes[0] & 128) bytes = Buffer.concat([Buffer.from([0]), bytes]);
    return Buffer.concat([Buffer.from([2, bytes.length]), bytes]);
  };
  const components = Buffer.concat([integer(signature.subarray(0, 32)), integer(signature.subarray(32))]);
  const der = Buffer.concat([Buffer.from([0x30, components.length]), components]);
  assert(verify("sha256", input, key, der), "independent DER verification");
  receipt = { status: "PASS", binarySha256, result: checks, vapid: "independently verified P1363 and DER" };
} catch (error) {
  console.error(logs);
  throw error;
} finally {
  try {
    const cleanup = await Promise.allSettled(children.map(async (child) => {
      if (child.exitCode !== null || child.signalCode !== null) return;
      const exited = new Promise((resolve) => child.once("exit", resolve));
      child.kill("SIGTERM");
      try { await bounded(exited, 5000); } catch {
        child.kill("SIGKILL");
        await bounded(exited, 5000);
      }
    }));
    const failures = cleanup.filter((result) => result.status === "rejected");
    if (failures.length) throw new AggregateError(failures.map((result) => result.reason), "candidate cleanup failed");
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
}
console.log(JSON.stringify(receipt));
