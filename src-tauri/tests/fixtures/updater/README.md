# Updater signature fixtures

These files drive `src-tauri/tests/updater_signature.rs`, which runs the Tauri
updater plugin's discovery and signature verification against a local server.

They were made with a **disposable** key pair, generated for this test only.
The private halves were deleted after signing and are not stored anywhere.
They are not production keys and do not match the production updater key. The
production public key is in `src-tauri/tauri.conf.json`.

| File | What it is |
|---|---|
| `disposable.pub` | Public key the test configures the plugin with. |
| `payload-2.0.0.bin` | The payload the valid signature covers. |
| `payload-2.0.0.sig` | The signature of that payload, made with the disposable key. |
| `payload-2.0.0-tampered.bin` | Bytes served in place of the payload. The signature above does not cover them. |
| `payload-2.0.0-other-key.sig` | A signature of the same payload by a second disposable key. |

## Regenerating

A key is needed only to make new fixtures. Use the Tauri CLI (`cargo tauri
signer`). The tests themselves need no key and no secret.

```bash
cargo tauri signer generate --ci -w /tmp/fixture-key -p "$PW"   # writes key and key.pub
cargo tauri signer sign -f /tmp/fixture-key -p "$PW" payload-2.0.0.bin
```

Keep the private key out of the repository. Delete it after you have copied
the public key and the signatures.

The test assumes the running version is `0.1.0`, which Tauri's mock context
reports. The first test asserts that.

These fixtures were made with the locally installed `cargo tauri` 2.10.1. The
release workflow pins CLI 2.12.4, and this test does not exercise signatures
from that version. The updater signing probe does, against the production key,
once it is authorized to run.
