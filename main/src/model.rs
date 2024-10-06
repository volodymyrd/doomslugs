use std::collections::HashMap;
use std::fmt;
use std::fmt::{Display, Formatter};
use std::hash::{Hash, Hasher};
use std::str::FromStr;
use std::sync::Arc;
use sha2::Digest;

/// Height of the block.
pub type BlockHeight = u64;

/// Balance is type for storing amounts of tokens.
pub type Balance = u128;

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CryptoHash(pub [u8; 32]);

impl CryptoHash {
    pub const LENGTH: usize = 32;

    pub const fn new() -> Self {
        Self([0; Self::LENGTH])
    }

    /// Calculates hash of given bytes.
    pub fn hash_bytes(bytes: &[u8]) -> CryptoHash {
        CryptoHash(sha2::Sha256::digest(bytes).into())
    }
}

impl AsRef<[u8]> for CryptoHash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Hash for CryptoHash {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write(self.as_ref());
    }
}

impl Default for CryptoHash {
    fn default() -> Self {
        Self::new()
    }
}

/// Calculates a hash of a bytes slice.
pub fn hash(data: &[u8]) -> CryptoHash {
    CryptoHash::hash_bytes(data)
}

/// The part of the block approval that is different for endorsements and skips
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ApprovalInner {
    Endorsement(CryptoHash),
    Skip(BlockHeight),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signature {
    ED25519(ed25519_dalek::Signature),
}

#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct ED25519PublicKey(pub [u8; ed25519_dalek::PUBLIC_KEY_LENGTH]);

/// Public key.
#[derive(Debug, Clone, PartialEq, PartialOrd, Ord, Eq)]
pub enum PublicKey {
    /// 256 bit elliptic curve based public-key.
    ED25519(ED25519PublicKey),
}

impl Display for PublicKey {
    fn fmt(&self, fmt: &mut Formatter) -> std::fmt::Result {
        let (key_data) = match self {
            PublicKey::ED25519(public_key) => (&public_key.0[..]),
        };
        write!(fmt, "{}", Bs58(key_data))
    }
}

#[derive(Debug, Clone, Eq)]
// This is actually a keypair, because ed25519_dalek api only has keypair.sign
// From ed25519_dalek doc: The first SECRET_KEY_LENGTH of bytes is the SecretKey
// The last PUBLIC_KEY_LENGTH of bytes is the public key, in total it's KEYPAIR_LENGTH
pub struct ED25519SecretKey(pub [u8; ed25519_dalek::KEYPAIR_LENGTH]);

impl PartialEq for ED25519SecretKey {
    fn eq(&self, other: &Self) -> bool {
        self.0[..ed25519_dalek::SECRET_KEY_LENGTH] == other.0[..ed25519_dalek::SECRET_KEY_LENGTH]
    }
}

/// Secret key container supporting different curves.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum SecretKey {
    ED25519(ED25519SecretKey),
}
impl SecretKey {
    fn ed25519_key_pair_from_seed(seed: &str) -> ed25519_dalek::SigningKey {
        let seed_bytes = seed.as_bytes();
        let len = std::cmp::min(ed25519_dalek::SECRET_KEY_LENGTH, seed_bytes.len());
        let mut seed: [u8; ed25519_dalek::SECRET_KEY_LENGTH] =
            [b' '; ed25519_dalek::SECRET_KEY_LENGTH];
        seed[..len].copy_from_slice(&seed_bytes[..len]);
        ed25519_dalek::SigningKey::from_bytes(&seed)
    }
    pub fn from_seed(seed: &str) -> Self {
        let keypair = SecretKey::ed25519_key_pair_from_seed(seed);
        SecretKey::ED25519(ED25519SecretKey(keypair.to_keypair_bytes()))
    }

    pub fn public_key(&self) -> PublicKey {
        match &self {
            SecretKey::ED25519(secret_key) => PublicKey::ED25519(ED25519PublicKey(
                secret_key.0[ed25519_dalek::SECRET_KEY_LENGTH..]
                    .try_into()
                    .unwrap(),
            )),
        }
    }
}

#[derive(Eq, Ord, Hash, Clone, Debug, PartialEq, PartialOrd)]
pub struct AccountId(pub(crate) Box<str>);

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl FromStr for AccountId {
    type Err = ();

    fn from_str(account_id: &str) -> Result<Self, Self::Err> {
        Ok(Self(account_id.into()))
    }
}

/// Block approval by other block producers with a signature
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub inner: ApprovalInner,
    pub target_height: BlockHeight,
    pub signature: Signature,
    pub account_id: AccountId,
}

/// Stores validator and its stake for two consecutive epochs.
/// It is necessary because the blocks on the epoch boundary need to contain approvals from both
/// epochs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalStake {
    /// Account that stakes money.
    pub account_id: AccountId,
    /// Public key of the proposed validator.
    pub public_key: PublicKey,
    /// Stake / weight of the validator.
    pub stake_this_epoch: Balance,
    pub stake_next_epoch: Balance,
}

/// Signer that keeps secret key in memory.
#[derive(Clone, PartialEq, Eq)]
pub struct Signer {
    pub account_id: AccountId,
    pub public_key: PublicKey,
    pub secret_key: SecretKey,
}

impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "InMemorySigner(account_id: {}, public_key: {})",
            self.account_id, self.public_key
        )
    }
}

impl Signer {
    pub fn from_seed(account_id: AccountId, seed: &str) -> Self {
        let secret_key = SecretKey::from_seed(seed);
        Self {
            account_id,
            public_key: secret_key.public_key(),
            secret_key,
        }
    }
}
/// Signer that keeps secret key in memory and signs locally.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatorSigner {
    account_id: AccountId,
    signer: Arc<Signer>,
}

impl ValidatorSigner {
    pub fn from_seed(account_id: AccountId, seed: &str) -> Self {
        let signer = Arc::new(Signer::from_seed(account_id.clone(), seed).into());
        Self { account_id, signer }
    }
}

/// Helper struct which provides Display implementation for bytes slice
/// encoding them using base58.
// TODO(mina86): Get rid of it once bs58 has this feature.  There’s currently PR
// for that: https://github.com/Nullus157/bs58-rs/pull/97
struct Bs58<'a>(&'a [u8]);

impl<'a> Display for Bs58<'a> {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> fmt::Result {
        // The largest buffer we’re ever encoding is 65-byte long.  Base58
        // increases size of the value by less than 40%.  96-byte buffer is
        // therefore enough to fit the largest value we’re ever encoding.
        let mut buf = [0u8; 96];
        let len = bs58::encode(self.0).into(&mut buf[..]).unwrap();
        let output = &buf[..len];
        // SAFETY: we know that alphabet can only include ASCII characters
        // thus our result is an ASCII string.
        fmt.write_str(unsafe { std::str::from_utf8_unchecked(output) })
    }
}
