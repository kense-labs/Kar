//! An owner window can carry the encrypted handoff without receiving the token decryption key.
use super::*;
use ring::{
    agreement::{self, EphemeralPrivateKey, UnparsedPublicKey, X25519},
    hkdf,
    rand::SystemRandom,
};
use zeroize::Zeroizing;
#[derive(Serialize, Deserialize)]
pub(super) struct Envelope {
    pub public: String,
    #[serde(flatten)]
    pub body: store::Sealed,
}
pub(super) enum Receiver {
    Fresh(Option<EphemeralPrivateKey>),
    Paired(String, Zeroizing<Vec<u8>>),
}
fn invalid() -> Error {
    bad("invalid ChatGPT transfer peer; prepare a fresh ticket")
}
fn ephemeral() -> Result<(String, EphemeralPrivateKey)> {
    let secret =
        EphemeralPrivateKey::generate(&X25519, &SystemRandom::new()).map_err(|_| invalid())?;
    Ok((
        B64.encode(secret.compute_public_key().map_err(|_| invalid())?.as_ref()),
        secret,
    ))
}
pub(super) fn receiver() -> Result<(String, Receiver)> {
    let (public, sk) = ephemeral()?;
    Ok((public, Receiver::Fresh(Some(sk))))
}
fn derive(sk: EphemeralPrivateKey, peer: &[u8], aad: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    agreement::agree_ephemeral(sk, &UnparsedPublicKey::new(&X25519, peer), |ikm| {
        let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &Sha256::digest(aad)).extract(ikm);
        let mut out = Zeroizing::new(vec![0; 32]);
        prk.expand(&[b"Kar ChatGPT profile transfer v1"], hkdf::HKDF_SHA256)
            .map_err(|_| invalid())?
            .fill(&mut out)
            .map_err(|_| invalid())?;
        Ok(out)
    })
    .map_err(|_| invalid())?
}
pub(super) fn sender(peer: &[u8], aad: &[u8]) -> Result<(String, Zeroizing<Vec<u8>>)> {
    let (public, sk) = ephemeral()?;
    Ok((public, derive(sk, peer, aad)?))
}
impl Receiver {
    pub fn key(&mut self, public: &str, aad: &[u8]) -> Result<&[u8]> {
        if let Self::Fresh(sk) = self {
            let peer = B64.decode(public).map_err(|_| invalid())?;
            if peer.len() != 32 {
                return Err(invalid());
            }
            *self = Self::Paired(
                public.into(),
                derive(sk.take().ok_or_else(invalid)?, &peer, aad)?,
            );
        }
        match self {
            Self::Paired(p, k) if p == public => Ok(k),
            _ => Err(invalid()),
        }
    }
}
