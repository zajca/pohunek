use openidconnect::core::CoreRsaPrivateSigningKey as AliasedPrivateKey;

fn main() {
    let _ = AliasedPrivateKey::from_pem("", None);
}
