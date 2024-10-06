use crate::clock::{DisplayInstant, Duration, FakeClock, Instant, Utc};
use crate::doomslugs::{Doomslug, DoomslugThresholdMode};
use crate::model::{
    hash, Approval, ApprovalStake, BlockHeight, CryptoHash, SecretKey, ValidatorSigner,
};
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn test_fuzzy_doomslug_liveness_and_safety() {
    println!("Start test...");
    for (time_to_gst_millis, height_goal) in &[(0, 200), (1000, 200)] {
        for delta in &[100] {
            println!(
                "Staring set of tests. Time to GST: {}, delta: {}",
                time_to_gst_millis, delta
            );
            for _ in 0..1 {
                let (took, height) = one_iter(
                    Duration::milliseconds(*time_to_gst_millis),
                    Duration::milliseconds(*delta),
                    *height_goal,
                );
                println!(
                    " --> Took {} (simulated) milliseconds and {} heights",
                    took.whole_milliseconds(),
                    height
                );
            }
        }
    }
}

fn one_iter(
    time_to_gst: Duration,
    delta: Duration,
    height_goal: BlockHeight,
) -> (Duration, BlockHeight) {
    let account_ids = ["test1"];

    let stakes = account_ids
        .iter()
        .map(|account_id| ApprovalStake {
            account_id: account_id.parse().unwrap(),
            stake_this_epoch: 1,
            stake_next_epoch: 1,
            public_key: SecretKey::from_seed(account_id).public_key(),
        })
        .map(|stake| (stake, false))
        .collect::<Vec<_>>();

    let signers = account_ids
        .iter()
        .map(|account_id| Some(Arc::new(create_test_signer(account_id))))
        .collect::<Vec<_>>();

    let clock = FakeClock::new(Utc::UNIX_EPOCH);

    let mut doomslugs: Vec<_> = std::iter::repeat_with(|| {
        Doomslug::new(
            clock.clock(),
            0,
            Duration::milliseconds(200),
            Duration::milliseconds(1000),
            Duration::milliseconds(100),
            delta * 20, // some arbitrary number larger than delta * 6
            DoomslugThresholdMode::TwoThirds,
        )
    })
    .take(signers.len())
    .collect();

    let started = clock.now();
    println!("STARTED TIME: {}", DisplayInstant(started));
    let gst = clock.now() + time_to_gst;
    println!("GST: {}", DisplayInstant(gst));
    let mut approval_queue: Vec<(Approval, Instant)> = vec![];
    let mut block_queue: Vec<(BlockHeight, usize, BlockHeight, Instant, CryptoHash)> = vec![];
    let mut largest_produced_height: BlockHeight = 1;
    let mut chain_lengths = HashMap::new();
    let mut hash_to_block_info: HashMap<CryptoHash, (BlockHeight, BlockHeight, CryptoHash)> =
        HashMap::new();
    let mut hash_to_prev_hash: HashMap<CryptoHash, CryptoHash> = HashMap::new();

    let mut blocks_with_finality: Vec<(CryptoHash, BlockHeight)> = vec![];

    chain_lengths.insert(block_hash(1, 0), 1);

    (Duration::default(), BlockHeight::default())
}

pub fn create_test_signer(account_name: &str) -> ValidatorSigner {
    ValidatorSigner::from_seed(account_name.parse().unwrap(), account_name).into()
}

fn block_hash(height: BlockHeight, ord: usize) -> CryptoHash {
    hash(([height.to_le_bytes(), ord.to_le_bytes()].concat()).as_ref())
}
