---
type: Runbook
id: runbook/plugin-index-keys
title: Plugin index key ceremony and compromise procedure
description: Owner procedure to create, endorse, install and rotate the online key that signs the official plugin index, and to revoke it after a leak or loss, using the xtask ceremony commands that read every secret from a file.
source_kind: manual
intents: [update, help]
since: 0.40.0
---

# Plugin Index Key Ceremony

The official plugin index is signed by an online index key `K`. Hosts trust `K`
only through a record that the offline primary root `P` signed (scope
`official`, at most 366 days), and they refuse `K` once a revocation list signed
by `P` names it. This runbook is for the owner. Agents and CI never run a real
ceremony: the commands below touch `P`, which stays offline, and create the
secret that is installed into the signing environment. The trust model is in
[Plugin index](../concepts/runtime-catalog.md#plugin-index).

All four commands are `cargo xtask` commands run from a checkout at the release
tag that shipped them:

| Command | Writes | Secret it reads |
|---|---|---|
| `catalog keygen --output <key-file>` | a new 0600 key file | none (OS randomness) |
| `index endorse --endorser-key-file <P> --key-file <K> --scope official --not-before <s> --not-after <s> --output <record.json>` | the signed key record of `K` | the seeds of `P` and `K` |
| `index revoke --endorser-key-file <P> --sequence <n> [--key-id <id>]... [--previous <revocations.json>] --output <revocations.json>` | the signed revocation list | the seed of `P` |
| `index channel --anchor <anchor.json> --endorser-key-id <id> --output <channel.json>` | the channel file that names `P` | none |

Rules every command follows:

- A seed is read only from a file: a regular file you own, mode `0600` or
  stricter, one hard link, not a symbolic link. Anything else is refused with
  the reason and the path, never the content. There is no flag, environment
  variable or prompt that takes a key, and no command prints a seed.
- Every output path must not exist yet. A refused input writes nothing, and a
  finished document is never overwritten; move or delete the old file
  deliberately.
- Documents are verified with the daemon's rules before they are written. A
  window shorter than one index lifetime (14 days), longer than 366 days, empty
  or already over is refused.
- `index revoke` writes exactly the ids given with `--key-id`. With
  `--previous` it requires a higher sequence and every previously revoked id;
  a list never shrinks, so repeat the earlier ids on every rotation. Without
  `--previous` only `--sequence 1` is accepted. At most 64 ids. The primary
  cannot revoke itself: that is an anchor change in a core release.

## 1. Create and endorse the index key (O4)

Work on the owner's machine, in a private RAM-backed directory:

```sh
umask 077; d=$(mktemp -d /dev/shm/pohunek-index-XXXXXX)
cargo xtask catalog keygen --output "$d/official-index.key"
```

It prints `key_id=<64 hex>` and `public_key=<64 hex>`. Stage the primary seed
as `$d/primary.key`, in the same RAM-backed directory and under the same
`umask 077`. An offline medium (a vfat or exFAT USB stick) normally fails the
reader's `0600`, own-uid and one-link rules, so copy the seed from it
(`cp <offline-medium>/<primary-seed> "$d/primary.key"`) rather than pointing a
command at the medium. Then:

```sh
now=$(date -u +%s)
cargo xtask index endorse --endorser-key-file "$d/primary.key" \
  --key-file "$d/official-index.key" --scope official \
  --not-before "$now" --not-after $((now + 31536000)) \
  --output official.record.json
cargo xtask index revoke --endorser-key-file "$d/primary.key" --sequence 1 \
  --output revocations.json
shred -u "$d/primary.key"
```

`endorse` prints the record's `key_id` and `endorsed_by`; the second must be the
primary's key id. `shred -u` destroys the staged copy of the primary seed after
the last command that reads it; put the offline medium away. Open a pull request to
the index repository that adds `keys/official.record.json` and
`revocations/revocations.json` (both are public). Keep `$d` for the next step.

## 2. Install the signing secret (O5)

Create the `index-sign-official` environment in the index repository (branch
`main` only, administrator bypass off), then store the seed from the file and
destroy the file:

```sh
gh secret set INDEX_SIGNING_KEY_OFFICIAL --env index-sign-official \
  --repo pohunekd/pohunek-plugin-index < "$d/official-index.key"
shred -u "$d/official-index.key"; rmdir "$d"
```

The seed never reaches a terminal, an issue, a chat or an agent session. A lost
key is replaced (sections 1 and 3), never restored.

## 3. Rotate or revoke after a leak or loss

Revoke `K`, introduce its replacement `K'` and swap the secret in one step, so
there is no window in which the published index is signed by a revoked key:

1. Create `K'` and its record as in section 1 (the private `$d`,
   `catalog keygen`, the staged `$d/primary.key`, then `index endorse` with a
   fresh window). Skip section 1's `index revoke` and `shred -u`: the primary
   seed stays staged until step 2 has run.
2. Write the next revocation list with the primary, keeping every id of the
   previous list, then destroy the staged primary seed:

   ```sh
   cargo xtask index revoke --endorser-key-file "$d/primary.key" \
     --previous revocations/revocations.json --sequence <n+1> \
     --key-id <K> [--key-id <earlier-revoked-id>]... \
     --output revocations.next.json
   shred -u "$d/primary.key"
   ```

3. In one pull request to the index repository, add the record of `K'` and
   replace `revocations/revocations.json` with the new list.
4. Replace `INDEX_SIGNING_KEY_OFFICIAL` with the seed of `K'` (section 2) and
   dispatch the sign workflow.

Stopping part-way publishes nothing wrong: until both the record and the secret
match, the sign job refuses to publish and the previous index stays the latest
one until it expires. Every host that installs by id after the new document
refuses `K` permanently, because revoked ids accumulate in its index state.

The primary itself is replaced only by a trust anchor change in a core release.

## 4. The channel file

`packaging/plugin-index-channel.json` names the primary as the index endorser.
Regenerate it only when the anchor changes:

```sh
cargo xtask index channel --anchor packaging/runtime-catalog-anchor.json \
  --endorser-key-id <primary key id> --output <new-channel.json>
```

The command refuses an id that is not a root of the anchor.

## 5. Build, sign and verify an index

These commands read no key file; the online key is used only by a tool outside
the workspace, so code built from the workspace never holds it.

| Command | Writes | Checks |
|---|---|---|
| `index build --entries <dir> --revocations <file> --key-record <file>... --sequence <s> --expires-at <s> --output <unsigned.json>` | the unsigned index | entry layout and `[source]`, then the daemon's body rules |
| `index message --unsigned <unsigned.json> --output <message.bin>` | the bytes a signature covers | the key-free parse of the daemon |
| `index attach --unsigned <unsigned.json> --key-id <K> --signature <sig.hex> --anchor <anchor.json> --channel <channel.json> --output <signed.json>` | the signed index | `verify_index` with the signer `K` |
| `index verify --index <signed.json> --anchor <anchor.json> --channel <channel.json> [--index-high-water <n>] [--revocation-high-water <n>]` | nothing | `verify_index` at the current time |
| `index verify --record <record.json> --revocations <revocations.json> [--previous-revocations <file>] --anchor <anchor.json> --channel <channel.json>` | nothing | the record, the list signature and, with `--previous-revocations`, a higher sequence and a superset |
| `index throwaway --entries <dir> --output <dir>` | an anchor, a channel and an index of a random root | the index under its own anchor |

The signer signs `message.bin` as a plain Ed25519 message and writes the
signature as 128 lowercase hex characters (one optional newline). A signature
over other bytes, a key id without a record, a signer the anchor or the
revocation list revokes, and a document that already carries signatures are
refused and leave no output. The `[source]` table of an entry file never
reaches the index; `index build --print-source` is how admission reads it.

`index throwaway` is for proofs only: its root and key exist for the run, its
output lies outside the checkout, and the committed anchor and channel do not
name its root, so `index verify` against them fails.

## 6. Build and prove a plugin archive

Neither command reads a key file or the network. They run from a checkout at a
core release tag, the way `plugin-release.yml` and the index admission run them.

| Command | Writes | Checks |
|---|---|---|
| `plugin build --dir <package dir> --output-dir <dir>` | `<dir>/<package_id>-<version>.tar.zst` (mode 0644, never replaces a file) | the canonical archive of the directory; the identity comes from its `runtime.toml` |
| `plugin prove --archive <file> --entry <entry.toml> --release-dir <dir> --evidence <file>` | the evidence JSON | the steps below |

`plugin prove` takes the entry file of the index repository (with its `[source]`
table) and runs these steps in a private root under `$TMPDIR`, or under `/tmp` when the
control socket path would not fit there (a stock macOS `$TMPDIR`); each failure
names its step in the evidence:

1. `digest`: the SHA-256 of the archive equals the entry's digest, the archive
   reads under the strict reader, and its descriptor has the entry's package id,
   runtime id and version. Nothing is created before this step passes.
2. `trust`: `index throwaway` signs an index of exactly this entry.
3. `host`: `pohunek`, `pohunekd` and `pohunek-sessiond` of `--release-dir` are
   copied beside the throwaway anchor and channel, and the daemon starts with a
   cleared environment, its own home, XDG directories and control socket.
4. `install`: `pohunek plugin install <archive> --index <index> --yes --json`
   reports `installed`, origin `official`, enabled and selected.
5. `inspect`: `pohunek plugin inspect <package_id> --json` reports the entry's
   runtime without a fault, and `pohunek plugin doctor <package_id>` finds
   nothing.
6. `cleanup`: the daemon stops and the root is removed. A failure here is a
   failure of the proof, also when the earlier steps passed.

The root is removed on every path. The evidence is
`{"schema":1,"core_version":...,"package_id":...,"version":...,"digest":...,
"platform":...,"result":"pass"|"fail","failed_step":null|"<step>"}`;
`core_version` is the first line of `pohunekd --version` and is `null` when the
proof ended before `pohunekd --version` ran. The exit status is 0 for a pass, 1 for a
failed step and 2 for a refused invocation (an unreadable input, or an evidence
file that exists); a refused invocation writes no evidence. The runtime program
of the package is never launched, so no model runs. A runtime id an index never
authorizes (`codex`, `claude`, `hermes`, `pi`, `shell`) fails at `install` with
`package_runtime_not_claimable`.
