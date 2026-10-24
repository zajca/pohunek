use p256::ecdsa::SigningKey as AliasedSigningKey;

fn main() {
    let _ = AliasedSigningKey::from_slice(&[1; 32]);
}
