// Public test vectors: RFC 8291 Appendix A and RFC 8292 section 2.4.
const decode = (text) => Uint8Array.from(atob(text.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));
const encode = (bytes) => btoa(String.fromCharCode(...new Uint8Array(bytes))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
const utf8 = (text) => new TextEncoder().encode(text);
const concat = (...parts) => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let offset = 0;
  for (const part of parts) { out.set(part, offset); offset += part.length; }
  return out;
};
const expect = (bytes, value, label) => {
  if (encode(bytes) !== value) throw Error(`RFC vector mismatch: ${label}`);
};

export async function freshVapid() {
  const pair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, false, ["sign", "verify"]);
  const header = encode(utf8(JSON.stringify({ typ: "JWT", alg: "ES256" })));
  const claims = encode(utf8(JSON.stringify({ aud: "https://push.example.invalid", exp: 1700003600, sub: "mailto:fixture@example.invalid" })));
  const signingInput = `${header}.${claims}`;
  const signature = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" }, pair.privateKey, utf8(signingInput));
  const publicKey = encode(await crypto.subtle.exportKey("raw", pair.publicKey));
  return { authorization: `vapid t=${signingInput}.${encode(signature)}, k=${publicKey}` };
}

export async function rfcVectors() {
  const subtle = crypto.subtle;
  const asPublic = decode("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8");
  const uaPublic = decode("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
  const algorithm = { name: "ECDH", namedCurve: "P-256" };
  const privateKey = (point, d) => subtle.importKey("jwk", {
    kty: "EC", crv: "P-256", x: encode(point.slice(1, 33)), y: encode(point.slice(33)), d,
  }, algorithm, false, ["deriveBits"]);
  const asPrivate = await privateKey(asPublic, "yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw");
  const uaPrivate = await privateKey(uaPublic, "q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94");
  const asPeer = await subtle.importKey("raw", asPublic, algorithm, false, []);
  const uaPeer = await subtle.importKey("raw", uaPublic, algorithm, false, []);
  const secret = await subtle.deriveBits({ name: "ECDH", public: uaPeer }, asPrivate, 256);
  const expectedSecret = "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs";
  expect(secret, expectedSecret, "sender ECDH");
  expect(await subtle.deriveBits({ name: "ECDH", public: asPeer }, uaPrivate, 256), expectedSecret, "receiver ECDH");
  const hmac = async (key, data) => {
    const imported = await subtle.importKey("raw", key, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
    return new Uint8Array(await subtle.sign("HMAC", imported, data));
  };
  const hkdf = async (key, salt, info, length) => {
    const imported = await subtle.importKey("raw", key, "HKDF", false, ["deriveBits"]);
    return new Uint8Array(await subtle.deriveBits({ name: "HKDF", hash: "SHA-256", salt, info }, imported, length * 8));
  };
  const auth = decode("BTBZMqHH6r4Tts7J_aSIgg");
  const salt = decode("DGv6ra1nlYgDCS1FRnbzlw");
  const prkKey = await hmac(auth, secret);
  expect(prkKey, "Snr3JMxaHVDXHWJn5wdC52WjpCtd2EIEGBykDcZW32k", "PRK_key");
  const info = concat(utf8("WebPush: info\0"), uaPublic, asPublic);
  const ikm = await hkdf(secret, auth, info, 32);
  expect(ikm, "S4lYMb_L0FxCeq0WhDx813KgSYqU26kOyzWUdsXYyrg", "IKM");
  const prk = await hmac(salt, ikm);
  expect(prk, "09_eUZGrsvxChDCGRCdkLiDXrReGOEVeSCdCcPBSJSc", "PRK");
  const cek = await hkdf(ikm, salt, utf8("Content-Encoding: aes128gcm\0"), 16);
  const nonce = await hkdf(ikm, salt, utf8("Content-Encoding: nonce\0"), 12);
  expect(cek, "oIhVW04MRdy2XN9CiKLxTg", "CEK");
  expect(nonce, "4h_95klXJ5E_qnoN", "nonce");
  const aes = await subtle.importKey("raw", cek, "AES-GCM", false, ["encrypt", "decrypt"]);
  const plaintext = concat(decode("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24"), new Uint8Array([2]));
  const ciphertext = await subtle.encrypt({ name: "AES-GCM", iv: nonce }, aes, plaintext);
  expect(ciphertext, "8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ", "AES-GCM ciphertext");
  expect(await subtle.decrypt({ name: "AES-GCM", iv: nonce }, aes, ciphertext), encode(plaintext), "AES-GCM plaintext");

  const header = "eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiJ9";
  const payload = "eyJhdWQiOiJodHRwczovL3B1c2guZXhhbXBsZS5uZXQiLCJleHAiOjE0NTM1MjM3NjgsInN1YiI6Im1haWx0bzpwdXNoQGV4YW1wbGUuY29tIn0";
  const signature = decode("i3CYb7t4xfxCDquptFOepC9GAu_HLGkMlMuCGSK2rpiUfnK9ojFwDXb1JrErtmysazNjjvW2L9OkSSHzvoD1oA");
  const vapidPoint = decode("BA1Hxzyi1RUM1b5wjxsn7nGxAszw2u61m164i3MrAIxHF6YK5h4SDYic-dRuU_RCPCfA5aq9ojSwk5Y2EmClBPs");
  if (vapidPoint.length !== 65 || signature.length !== 64) throw Error("VAPID raw/JOSE encoding");
  const vapid = await subtle.importKey("raw", vapidPoint, { name: "ECDSA", namedCurve: "P-256" }, false, ["verify"]);
  if (!await subtle.verify({ name: "ECDSA", hash: "SHA-256" }, vapid, signature, utf8(`${header}.${payload}`))) throw Error("RFC 8292 ES256 signature");
  const claims = JSON.parse(new TextDecoder().decode(decode(payload)));
  if (claims.aud !== "https://push.example.net" || claims.exp !== 1453523768 || claims.sub !== "mailto:push@example.com") throw Error("RFC 8292 claims");
  // The historical token is deliberately expired; this is signature validation,
  // not authorization of a request or an attempt to contact the example origin.
  return ["RFC8291", "RFC8292"];
}
