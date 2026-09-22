/*!
 * pin_identity.rs
 *
 * Application case: who a peer *is* — the announced identity and the pin table.
 *
 * Author: arkSong <arksong2018@gmail.com>
 * Created: 2024-01-15
 * Version: 0.4.0
 * License: MIT
 *
 * Features:
 * - `NetworkIdentity`: the X25519 key pair an instance announces, persisted so a
 *   peer that pinned it sees the same value after a restart
 * - `PinStore`: trust on first use — the first identity seen under a nickname is
 *   pinned, and a later change under the same name is reported and counted
 * - `NetworkIdentity::agree`: what the identity is for, the pair secret both ends
 *   derive from their own secret half and the other's public half
 *
 * ```bash
 * cargo run --example pin_identity
 * ```
 *
 * This is the identity half of the design (architecture document §3.10 and A12):
 * the value a peer pins is already the public key the handshake uses. It is a
 * pure library example: no transport, no peer, no network.
 */

use anyhow::{bail, Context, Result};
use meta_text::identity::{public_key_from_hex, IdentityStore, NetworkIdentity};
use meta_text::trust::{PinOutcome, PinStore};

/// Show what an instance announces, derives and remembers about its peers.
fn main() -> Result<()> {
    // 1. State lives in a temporary directory, as everywhere in `examples/`.
    let dir = tempfile::tempdir().context("create a data directory")?;

    // 2. An identity is created once and then *loaded*: `load_or_create` is the
    //    call a core makes at startup, and the file it writes is what makes the
    //    announcement stable. A per-session random value could never be pinned,
    //    because there would be nothing to compare against.
    let store = IdentityStore::new(dir.path());
    let identity = store.load_or_create();
    println!("identity      : {}", identity.public_hex());
    println!("fingerprint   : {}", identity.fingerprint());
    println!("stored in     : {}", store.path().display());

    // 3. `Debug` is redacted: a secret that reaches a log line is a secret that
    //    leaked. The value below is the *public* half only.
    println!("debug         : {identity:?}");

    // 4. A restart over the same directory must produce the same announcement —
    //    this is the difference the identity work bought, and the reason a
    //    fingerprint is worth comparing at all.
    let restarted = IdentityStore::new(dir.path()).load_or_create();
    println!(
        "after restart : same identity: {}",
        restarted.public_hex() == identity.public_hex()
    );

    // 5. What the key pair is for: the two ends derive the same secret from
    //    (own secret, other's public). Neither the secret half nor the derived
    //    value is printed here; only whether the two sides agree.
    let peer = NetworkIdentity::generate();
    let peer_public = public_key_from_hex(&peer.public_hex()).context("a 32 byte public key")?;
    let own_public = public_key_from_hex(&identity.public_hex()).context("a 32 byte public key")?;
    let ours = identity.agree(peer_public);
    let theirs = peer.agree(own_public);
    println!(
        "agree         : both ends derive the same secret: {}",
        ours == theirs
    );

    // 6. Trust on first use. `observe` is what the transport calls for every
    //    identity frame, and it is the *change* that matters: a first sighting is
    //    not interesting, a second one under the same nickname is.
    let pins = PinStore::load(dir.path());
    match pins.observe("bob", &peer.public_hex()) {
        PinOutcome::FirstSeen => println!("first sighting: bob pinned"),
        other => bail!("expected a first sighting, got {other:?}"),
    }
    match pins.observe("bob", &peer.public_hex()) {
        PinOutcome::Known => println!("second sighting: the pinned identity, unchanged"),
        other => bail!("expected a known peer, got {other:?}"),
    }

    // 7. Somebody else announces itself as `bob`. The pin moves on — refusing
    //    would only push the peer to a new nickname — but the change is reported
    //    and counted, so `/peers` can flag it and `MetricsView` can expose it.
    let impostor = NetworkIdentity::generate();
    match pins.observe("bob", &impostor.public_hex()) {
        PinOutcome::Changed { previous } => {
            println!("changed       : bob announced a new identity (was {previous})");
        }
        other => bail!("expected a change, got {other:?}"),
    }
    println!("pins in table : {}", pins.len());
    println!("changes seen  : {}", pins.changed());
    println!("refusals      : {}", pins.refused());
    println!("bob is now    : {:?}", pins.pinned("bob"));

    // 8. The table is on disk, so the warning survives the next start: a reload
    //    sees the newest identity as the pinned one and a *third* value as
    //    another change.
    let reloaded = PinStore::load(dir.path());
    println!("after reload  : {:?}", reloaded.pinned("bob"));
    match reloaded.observe("bob", &impostor.public_hex()) {
        PinOutcome::Known => println!("reload        : the same identity is known, not a change"),
        other => bail!("expected the identity to be known after a reload, got {other:?}"),
    }

    // 9. A core without a writable data directory uses the volatile store; it
    //    still detects a change for as long as the process lives.
    let volatile = PinStore::volatile();
    let _ = volatile.observe("carol", &peer.public_hex());
    println!("volatile      : holds {} pin(s) in memory", volatile.len());
    Ok(())
}
