use openidconnect::core::CoreRsaPrivateSigningKey as AliasedPrivateKey;

#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn main() {
    let _ = AliasedPrivateKey::from_pem("", None);
}
