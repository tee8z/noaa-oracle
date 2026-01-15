use maud::{html, Markup};

/// Oracle information display fragment
/// Shows the oracle's public key and npub with copy functionality
pub fn oracle_info(pubkey: &str, npub: &str) -> Markup {
    html! {
        div class="box oracle-info" {
            h2 class="title is-5 mb-4" { "Oracle Identity" }

            div class="columns is-multiline" {
                // Public Key
                div class="column is-full-mobile is-half-desktop" {
                    div class="mb-3" {
                        p class="info-label" { "Public Key (Base64)" }
                        div class="is-flex is-align-items-center" {
                            code class="info-value is-flex-grow-1 mr-2" id="pubkey-value" {
                                (pubkey)
                            }
                            button class="button is-small copy-btn"
                                   onclick="copyToClipboard('pubkey-value', this)"
                                   title="Copy to clipboard" {
                                span class="icon is-small" {
                                    (copy_icon())
                                }
                            }
                        }
                    }
                }

                // Nostr npub
                div class="column is-full-mobile is-half-desktop" {
                    div class="mb-3" {
                        p class="info-label" { "Nostr Public Key (npub)" }
                        div class="is-flex is-align-items-center" {
                            code class="info-value is-flex-grow-1 mr-2" id="npub-value" {
                                (npub)
                            }
                            button class="button is-small copy-btn"
                                   onclick="copyToClipboard('npub-value', this)"
                                   title="Copy to clipboard" {
                                span class="icon is-small" {
                                    (copy_icon())
                                }
                            }
                        }
                    }
                }
            }
        }

        script {
            r#"
            function copyToClipboard(elementId, button) {
                const text = document.getElementById(elementId).innerText;
                navigator.clipboard.writeText(text).then(() => {
                    button.classList.add('copied');
                    const icon = button.querySelector('svg');
                    const originalPath = icon.innerHTML;
                    icon.innerHTML = '<polyline points="20 6 9 17 4 12"></polyline>';
                    setTimeout(() => {
                        button.classList.remove('copied');
                        icon.innerHTML = originalPath;
                    }, 2000);
                });
            }
            "#
        }
    }
}

fn copy_icon() -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" {
            rect x="9" y="9" width="13" height="13" rx="2" ry="2" {}
            path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" {}
        }
    }
}
