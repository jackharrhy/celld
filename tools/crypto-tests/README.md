# Embedded Worker raw EC regressions

Deploy `wrangler.jsonc` with the candidate executable into a fresh isolated SQLite
bucket and replica directory. Start that same executable on isolated loopback,
then GET the Worker. A successful HTTP 200 response reports `ok: true`,
`curves: ["P-256", "P-384", "P-521"]`, `vectors: ["RFC8291", "RFC8292"]`, and a
disposable VAPID authorization fixture. HTTP 500 reports the first failure.
Use the normal CLI documented in the root README; never point this fixture at
an existing application bucket or serving runtime.

The fixture covers generated, SPKI, JWK and raw public key conversion; SEC1
lengths; ECDH agreement; ECDSA P1363 signature width and verification; offset
BufferSources; extractability; public usages; and malformed, compressed,
wrong-curve and off-curve points. RFC 8291 Appendix A checks both ECDH directions,
HMAC/HKDF intermediate values, CEK, nonce, exact AES-GCM ciphertext and decryption.
RFC 8292 section 2.4 checks the independent published ES256 signature, 65-byte
public point, 64-byte JOSE signature and exact JWT claims. The historical token
is intentionally expired; it is never used for authorization. Other keys are
disposable and generated inside the test. No external endpoint is contacted.

The host runner deploys and starts a supplied candidate, checks the response,
and independently verifies the Worker-generated VAPID signature using Node's
OpenSSL verifier in both P1363 and DER forms. It validates the 65-byte public
key and exact JWT claims. Its fixed historical timestamp is only a fixture.

```sh
node tools/crypto-tests/run.mjs /absolute/candidate/celld /absolute/native/esbuild
```

The optional `runtime` suite uses the same runner for standalone storage and
crash recovery; see `../runtime-tests/README.md`.

Run it only inside a bounded OS sandbox with isolated loopback, empty environment,
read-only source/toolchain and scratch-only writes. The runner itself supplies a
small explicit child environment but does not create the OS boundary. It prints
the tested executable's SHA-256 and results, cleans its temporary state and does
not contact a real push service.

This fixture must run in the actual embedded Worker. Running it under Node is
not Celld qualification. These checks also do not replace the fork's storage,
replication, SDK and CLI qualification or browser/push-provider interoperability. Retain the
candidate source, image and executable identities with the result. Do not mark
a candidate ready when this fixture or the remaining qualification is unrun.
