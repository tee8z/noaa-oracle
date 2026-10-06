# Announcements and attestations on Nostr

The oracle can publish each event's announcement, and later its attestation,
to Nostr relays. A contract built on an event can then be settled from the
relays alone, after the oracle and the coordinator are gone. The same record
is served by the oracle at `GET /oracle/events/{id}` while it runs; see
[attestation.md](attestation.md) for the event, outcome, and attestation
contract this document refers to.

Publishing is off unless configured (see [Configuration](#configuration)).

## Why not NIP-88 kinds 88 and 89

The DLC oracle draft for Nostr (kinds 88 and 89, used by some DLC oracles)
carries a dlcspecs `oracle_announcement` and `oracle_attestation` TLV. The
oracle's attestation is the same BIP340 construction (dlctix tags the message
hash `DLC/oracle/attestation/v0`), but its events do not fit those messages:

- an event's outcomes are rankings of entries, up to 20,000 of them, and the
  attested message is the winners' indices as 8-byte integers, not one of a
  list of outcome strings;
- a dlcspecs announcement must be signed by the attestation key, and the
  oracle keeps that key for attestations and signed statements;
- kind 88 and 89 events are not replaceable, so a reader has to find and pair
  two events, where one addressable event can carry the current state.

So the oracle publishes NIP-78 application data instead.

## The event

Kind `30078` (NIP-78), addressable: relays keep the newest event per author,
kind, and `d` tag. The oracle publishes the announcement when the event is
created and replaces it with the attested record once it signs.

| Field | Value |
| --- | --- |
| `kind` | `30078` |
| `pubkey` | the oracle's Nostr publishing key (below). Never the attestation key. |
| `tags` | `["d", "oracle:<event id>"]`, `["oracle", "<attestation key, compressed hex>"]`, `["stage", "announced" \| "attested"]`, `["r", "<oracle URL>/oracle/events/<event id>"]`, `["alt", "DLC oracle <stage> event"]` |
| `content` | the record below, as JSON |
| `created_at` | when it was published; an attested record is always newer than the announcement it replaces |

The event id in the `d` tag is the oracle's event id (a UUID, lowercase with
hyphens), the same as in the coordinator's contract and in
`/oracle/events/{id}`.

### Record

```json
{
  "v": 1,
  "type": "oracle-event",
  "event_id": "0192f3a0-0000-7000-8000-0000000000aa",
  "oracle_pubkey": "02…",
  "nonce_point": "03…",
  "signing_date": 1790172800,
  "expiry": 1790259200,
  "outcomes": {"kind": "ranking", "entries": 3, "places": 1},
  "locking_points_sha256": "…",
  "statement": {"statement": {…}, "signature": "…"},
  "attestation": "…",
  "outcome_index": 2,
  "winners": [2]
}
```

| Field | Meaning |
| --- | --- |
| `v`, `type` | `1` and `"oracle-event"`. A reader refuses other values. |
| `event_id` | The oracle event. |
| `oracle_pubkey` | The attestation key `P`, compressed (33 bytes, hex). The same key as `GET /oracle/pubkey` `key` (base64 there). |
| `nonce_point` | `R`, compressed hex, as `nonce_point` in `/oracle/events/{id}`. |
| `signing_date`, `expiry` | UNIX seconds. `expiry` is `event_announcement.expiry`. |
| `outcomes` | `ranking`: outcome `i` is the `i`th of `(0..entries).permutations(places)`, then the refund-all outcome `[0, 1, …, entries − 1]` ([Outcomes and announcement](attestation.md#outcomes-and-announcement)). |
| `locking_points_sha256` | SHA-256 of every announced locking point, 33 bytes compressed each, in outcome order. The list itself (up to 20,000 points) is not published; a reader derives it from `P`, `R`, and the outcomes. |
| `statement` | The [signed event statement](attestation.md#signed-event-statement) once every entry is in; otherwise `null`, also in the attested record of an event that never filled. |
| `attestation` | The attestation scalar `s` (32 bytes, hex) once signed; otherwise `null`. |
| `outcome_index`, `winners` | The attested outcome's index and its winners (entry indices in ascending entry-id order); `null` until signed. |

Readers should ignore fields they do not know.

## Finding a record

With the oracle's attestation key and the event id (both are in the
coordinator's contract):

```json
["REQ", "<sub>", {"kinds": [30078], "#d": ["oracle:<event id>"]}]
```

Add `"authors": ["<publishing key>"]` when the publishing key is known; it is
in `GET /oracle/pubkey` under `nostr.pubkey`, with the relay list. Without it,
take every event the relays return, check each as below, and keep a record
whose `oracle_pubkey` is the expected key. Prefer one with an attestation.

The record does not depend on its author: the checks below hold only for the
oracle's own attestation, whoever relayed it. The publishing key and the `d`
tag just make it easy to find.

## Checking a record

1. Check the Nostr event id and signature (NIP-01), that `kind` is `30078`,
   and that the `d` tag is `oracle:<event_id>` for the record's `event_id`.
2. Check `oracle_pubkey` is the oracle key the contract uses, and
   `nonce_point` is the contract's nonce point. When `statement` is present,
   its BIP340 signature verifies against `oracle_pubkey` (x-only) and it
   names the same event and nonce point.
3. When `attestation` is present: `winners` is outcome `outcome_index` of
   `outcomes`, and `s·G` equals the locking point
   `attestation_locking_point(P, R, message)` (dlctix), where `message` is
   each winner as an 8-byte big-endian integer, concatenated. This is the
   check the oracle makes before it publishes an attestation. The same `s`
   unlocks the contract's outcome with that locking point.

`oracle::publication::parse_event` and `Record::verify` do steps 1 to 3 in
Rust; the caller compares the keys in step 2.

## Publishing

Publications go through an outbox in the event database, one row per event,
stage, and relay. Creating or attesting an event queues its rows after the
event write commits. A background task sends due rows and, on failure,
retries them 30 seconds later, doubling the wait up to an hour. A relay that
is down only grows the outbox; creating events, attesting, and the API never
wait on a relay. When two oracle processes share the database, one publishes
at a time.

At startup the oracle queues every event whose signing date is at most 30
days ago, for each relay that has not accepted it yet; each hour it does the
same for the last two days. Rows are sent at most 20 per pass, with passes
two seconds apart while rows remain.

Metrics: `oracle_nostr_published_total` and
`oracle_nostr_publish_failures_total` count deliveries per relay, and
`oracle_nostr_outbox_depth` counts deliveries not yet accepted.

## Configuration

```toml
[nostr]
enabled = true
relays = ["wss://relay.example.com", "wss://relay2.example.com"]
# key_path = "./oracle_nostr_key.pem"
```

The publishing key is a secp256k1 key in its own PEM file, created with mode
0600 when missing. It must not be the oracle signing key file, and the oracle
refuses to start if the two keys are equal. Flags and environment variables:
`--nostr-enabled` / `NOAA_ORACLE_NOSTR_ENABLED`, `--nostr-relays` /
`NOAA_ORACLE_NOSTR_RELAYS` (comma separated), `--nostr-key` /
`NOAA_ORACLE_NOSTR_KEY`.

Losing the publishing key does not affect contracts: a new key publishes the
same records, which still verify against the attestation key.
