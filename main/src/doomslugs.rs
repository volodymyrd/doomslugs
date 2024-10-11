use crate::clock::{Clock, Duration, Instant};
use crate::model::{
    AccountId, Approval, ApprovalInner, Balance, BlockHeight, BlockHeightDelta, CryptoHash,
    ValidatorSigner,
};
use std::collections::HashMap;
use std::sync::Arc;

/// Have that many iterations in the timer instead of `loop` to prevent potential bugs from blocking
/// the node
const MAX_TIMER_ITERS: usize = 20;

/// How many heights ahead to track approvals. This needs to be sufficiently large so that we can
/// recover after rather long network interruption, but not too large to consume too much memory if
/// someone in the network spams with invalid approvals. Note that we will only store approvals for
/// heights that are targeting us, which is once per as many heights as there are block producers,
/// thus 10_000 heights in practice will mean on the order of one hundred entries.
const MAX_HEIGHTS_AHEAD_TO_STORE_APPROVALS: BlockHeight = 10_000;

// Number of blocks (before head) for which to keep the history of approvals (for debugging).
const MAX_HEIGHTS_BEFORE_TO_STORE_APPROVALS: u64 = 20;

#[derive(Debug)]
struct DoomslugApprovalsTracker {
    clock: Clock,
    witness: HashMap<AccountId, (Approval, Instant)>,
    account_id_to_stakes: HashMap<AccountId, (Balance, Balance)>,
    // total_stake_this_epoch: Balance,
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
impl DoomslugTimer {
    /// Computes the delay to sleep given the number of heights from the last final block
    /// This is what `T` represents in the paper.
    ///
    /// # Arguments
    /// * `n` - number of heights since the last block with doomslug finality
    ///
    /// # Returns
    /// Duration to sleep
    pub fn get_delay(&self, n: BlockHeightDelta) -> Duration {
        let n32 = u32::try_from(n).unwrap_or(u32::MAX);
        std::cmp::min(
            self.max_delay,
            self.min_delay + self.delay_step * n32.saturating_sub(2),
        )
    }
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

    /// Updates the current tip of the chain. Restarts the timer accordingly.
    ///
    /// # Arguments
    /// * `block_hash`     - the hash of the new tip
    /// * `height`         - the height of the tip
    /// * `last_ds_final_height` - last height at which a block in this chain has doomslug finality
    pub fn set_tip(
        &mut self,
        block_hash: CryptoHash,
        height: BlockHeight,
        // last_final_height: BlockHeight,
    ) {
        debug_assert!(height > self.tip.height || self.tip.height == 0);
        self.tip = DoomslugTip { block_hash, height };

        //self.largest_final_height.set(last_final_height);
        self.timer.height = height + 1;
        self.timer.started = self.clock.now();

        self.approval_tracking.retain(|h, _| {
            *h > height.saturating_sub(MAX_HEIGHTS_BEFORE_TO_STORE_APPROVALS)
                && *h <= height + MAX_HEIGHTS_AHEAD_TO_STORE_APPROVALS
        });

        self.endorsement_pending = true;
    }

    /// Is expected to be called periodically and processed the timer (`start_timer` in the paper)
    /// If the `cur_time` way ahead of last time the `process_timer` was called, will only process
    /// a bounded number of steps, to avoid an infinite loop in case of some bugs.
    /// Processes sending delayed approvals or skip messages
    /// A major difference with the paper is that we process endorsement from the `process_timer`,
    /// not at the time of receiving a block. It is done to stagger blocks if the network is way
    /// too fast (e.g. during tests, or if a large set of validators have connection significantly
    /// better between themselves than with the rest of the validators)
    ///
    /// # Arguments
    /// * `cur_time` - is expected to receive `now`. Doesn't directly use `now` to simplify testing
    ///
    /// # Returns
    /// A vector of approvals that need to be sent to other block producers as a result of processing
    /// the timers
    #[must_use]
    pub fn process_timer(&mut self, signer: &Option<Arc<ValidatorSigner>>) -> Vec<Approval> {
        let now = self.clock.now();
        let mut ret = vec![];
        for _ in 0..MAX_TIMER_ITERS {
            let skip_delay = self
                .timer
                .get_delay(self.timer.height.saturating_sub(self.largest_final_height));

            // The `endorsement_delay` is time to send approval to the block producer at `timer.height`,
            // while the `skip_delay` is the time before sending the approval to BP of `timer_height + 1`,
            // so it makes sense for them to be at least 2x apart
            debug_assert!(skip_delay >= 2 * self.timer.endorsement_delay);

            let tip_height = self.tip.height;

            if self.endorsement_pending
                && now >= self.timer.last_endorsement_sent + self.timer.endorsement_delay
            {
                if tip_height >= self.largest_target_height {
                    self.largest_target_height += 1;

                    if let Some(approval) = self.create_approval(tip_height + 1, signer) {
                        ret.push(approval);
                    }
                    // self.update_history(ApprovalHistoryEntry {
                    //     parent_height: tip_height,
                    //     target_height: tip_height + 1,
                    //     timer_started_ago_millis: (self
                    //         .clock
                    //         .now()
                    //         .signed_duration_since(self.timer.last_endorsement_sent))
                    //     .whole_milliseconds()
                    //     .max(0) as u64,
                    //     expected_delay_millis: self.timer.endorsement_delay.whole_milliseconds()
                    //         as u64,
                    //     approval_creation_time: self.clock.now_utc(),
                    // });
                }

                self.timer.last_endorsement_sent = now;
                self.endorsement_pending = false;
            }

            if now >= self.timer.started + skip_delay {
                debug_assert!(!self.endorsement_pending);

                self.largest_target_height =
                    std::cmp::max(self.timer.height + 1, self.largest_target_height);

                if let Some(approval) = self.create_approval(self.timer.height + 1, signer) {
                    ret.push(approval);
                }
                // self.update_history(ApprovalHistoryEntry {
                //     parent_height: tip_height,
                //     target_height: self.timer.height + 1,
                //     timer_started_ago_millis: (self
                //         .clock
                //         .now()
                //         .signed_duration_since(self.timer.started))
                //     .whole_milliseconds()
                //     .max(0) as u64,
                //     expected_delay_millis: skip_delay.whole_milliseconds() as u64,
                //     approval_creation_time: self.clock.now_utc(),
                // });

                // Restart the timer
                self.timer.started += skip_delay;
                self.timer.height += 1;
            } else {
                break;
            }
        }

        ret
    }
    fn create_approval(
        &self,
        target_height: BlockHeight,
        signer: &Option<Arc<ValidatorSigner>>,
    ) -> Option<Approval> {
        signer.as_ref().map(|signer| {
            Approval::new(
                self.tip.block_hash,
                self.tip.height,
                target_height,
                &*signer,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::clock::{Duration, FakeClock, Utc};
    use crate::doomslugs::{Doomslug, DoomslugThresholdMode};
    use crate::model::{ApprovalInner, hash};
    use crate::tests::create_test_signer;
    use std::sync::Arc;

    #[test]
    fn test_endorsements_and_skips_basic() {
        let clock = FakeClock::new(Utc::UNIX_EPOCH);
        let signer = Some(Arc::new(create_test_signer("test").into()));
        let mut ds = Doomslug::new(
            clock.clock(),
            0,
            Duration::milliseconds(400),
            Duration::milliseconds(1000),
            Duration::milliseconds(100),
            Duration::milliseconds(3000),
            DoomslugThresholdMode::TwoThirds,
        );

        // Set a new potential tip, must produce an endorsement
        ds.set_tip(hash(&[1]), 1);
        // move timer in future, plus 399ms
        clock.advance(Duration::milliseconds(399));
        assert_eq!(ds.process_timer(&signer).len(), 0);
        clock.advance(Duration::milliseconds(1));
        let approvals= ds.process_timer(&signer);
        assert!(approvals.len() > 0);
        let approval = approvals.into_iter().nth(0).unwrap();
        assert_eq!(approval.inner, ApprovalInner::Endorsement(hash(&[1])));
        assert_eq!(approval.target_height, 2);
    }
}
