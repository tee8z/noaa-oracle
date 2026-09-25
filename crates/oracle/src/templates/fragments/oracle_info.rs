use maud::{Markup, html};

/// The oracle's keys, folded away near the bottom of the dashboard: most
/// visitors come for the weather. Clicking a key selects all of it.
pub fn oracle_info(pubkey: &str, npub: &str) -> Markup {
    html! {
        details class="box oracle-info" {
            summary { "Oracle keys" span class="muted" { " — to verify attestations" } }
            dl {
                dt { "Public key (base64)" }
                dd { code class="key" { (pubkey) } }
                dt { "Nostr public key (npub)" }
                dd { code class="key" { (npub) } }
            }
        }
    }
}
