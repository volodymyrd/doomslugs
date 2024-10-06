use crate::clock::{Clock, Duration, Instant};
use crate::model::{AccountId, Approval, ApprovalInner, Balance, BlockHeight, CryptoHash};
use std::collections::HashMap;

#[derive(Debug)]
struct DoomslugApprovalsTracker {
    clock: Clock,
    witness: HashMap<AccountId, (Approval, Instant)>,
    account_id_to_stakes: HashMap<AccountId, (Balance, Balance)>,
    total_stake_this_epoch: Balance,
    approved_stake_this_epoch: Balance,
    total_stake_next_epoch: Balance,
    approved_stake_next_epoch: Balance,
    time_passed_threshold: Option<Instant>,
    threshold_mode: DoomslugThresholdMode,
}

#[derive(Debug)]
/// Approvals can arrive before the corresponding blocks, and we need a meaningful way to keep as
/// many approvals as possible that can be useful in the future, while not allowing an adversary
/// to spam us with invalid approvals.
/// To that extent, for each `account_id` and each `target_height` we keep exactly one approval,
/// whichever came last. We only maintain those for
///  a) `account_id`s that match the corresponding epoch (and for which we can validate a signature)
///  b) `target_height`s for which we produce blocks
///  c) `target_height`s within a meaningful horizon from the current tip.
/// This class is responsible for maintaining witnesses for the blocks, while also ensuring that
/// only one approval per (`account_id`) is kept. We instantiate one such class per height, thus
/// ensuring that only one approval is kept per (`target_height`, `account_id`). `Doomslug` below
/// ensures that only instances within the horizon are kept, and the user of the `Doomslug` is
/// responsible for ensuring that only approvals for proper account_ids with valid signatures are
/// provided.
struct DoomslugApprovalsTrackersAtHeight {
    clock: Clock,
    approval_trackers: HashMap<ApprovalInner, DoomslugApprovalsTracker>,
    last_approval_per_account: HashMap<AccountId, ApprovalInner>,
}

/// The threshold for doomslug to create a block.
/// `TwoThirds` means the block can only be produced if at least 2/3 of the stake is approving it,
///             and is what should be used in production (and what guarantees finality)
/// `NoApprovals` means the block production is not blocked on approvals. This is used
///             in many tests (e.g. `cross_shard_tx`) to create lots of forkfulness.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum DoomslugThresholdMode {
    NoApprovals,
    TwoThirds,
}

#[derive(Debug)]
struct DoomslugTip {
    block_hash: CryptoHash,
    height: BlockHeight,
}

#[derive(Debug)]
struct DoomslugTimer {
    started: Instant,
    last_endorsement_sent: Instant,
    height: BlockHeight,
    endorsement_delay: Duration,
    min_delay: Duration,
    delay_step: Duration,
    max_delay: Duration,
}

#[derive(Debug)]
pub struct Doomslug {
    clock: Clock,
    approval_tracking: HashMap<BlockHeight, DoomslugApprovalsTrackersAtHeight>,
    /// Largest target height for which we issued an approval
    largest_target_height: BlockHeight,
    /// Largest height for which we saw a block containing 1/2 endorsements in it
    largest_final_height: BlockHeight,
    /// Largest height for which we saw threshold approvals (and thus can potentially create a block)
    largest_threshold_height: BlockHeight,
    /// Largest target height of approvals that we've received
    largest_approval_height: BlockHeight,
    /// Information Doomslug tracks about the chain tip
    tip: DoomslugTip,
    /// Whether an endorsement (or in general an approval) was sent since updating the tip
    endorsement_pending: bool,
    /// Information to track the timer (see `start_timer` routine in the paper)
    timer: DoomslugTimer,
    /// How many approvals to have before producing a block. In production should be always `HalfStake`,
    ///    but for many tests we use `NoApprovals` to invoke more forkfulness
    threshold_mode: DoomslugThresholdMode,
}

impl Doomslug {
    pub fn new(
        clock: Clock,
        largest_target_height: BlockHeight,
        endorsement_delay: Duration,
        min_delay: Duration,
        delay_step: Duration,
        max_delay: Duration,
        threshold_mode: DoomslugThresholdMode,
    ) -> Self {
        Doomslug {
            clock: clock.clone(),
            approval_tracking: HashMap::new(),
            largest_target_height,
            largest_approval_height: 0,
            largest_final_height: 0,
            largest_threshold_height: 0,
            tip: DoomslugTip {
                block_hash: CryptoHash::default(),
                height: 0,
            },
            endorsement_pending: false,
            timer: DoomslugTimer {
                started: clock.now(),
                last_endorsement_sent: clock.now(),
                height: 0,
                endorsement_delay,
                min_delay,
                delay_step,
                max_delay,
            },
            threshold_mode,
        }
    }
}
