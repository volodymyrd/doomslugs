use std::collections::HashMap;
use std::sync::Arc;
use crate::clock::{Clock, Duration, Instant, Utc};
use crate::model::{AccountId, Approval, ApprovalInner, ApprovalStake, Balance, BlockHeight, BlockHeightDelta, CryptoHash, ValidatorSigner};

/// Have that many iterations in the timer instead of `loop` to prevent potential bugs from blocking
/// the node
const MAX_TIMER_ITERS: usize = 20;

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
        std::cmp::min(self.max_delay, self.min_delay + self.delay_step * n32.saturating_sub(2))
    }
}

struct DoomslugTip {
    block_hash: CryptoHash,
    height: BlockHeight,
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

/// The result of processing an approval.
#[derive(PartialEq, Eq, Debug)]
pub enum DoomslugBlockProductionReadiness {
    NotReady,
    ReadySince(Instant),
}

struct DoomslugApprovalsTracker {
    clock: Clock,
    witness: HashMap<AccountId, (Approval, Utc)>,
    account_id_to_stakes: HashMap<AccountId, (Balance, Balance)>,
    total_stake_this_epoch: Balance,
    approved_stake_this_epoch: Balance,
    total_stake_next_epoch: Balance,
    approved_stake_next_epoch: Balance,
    time_passed_threshold: Option<Instant>,
    threshold_mode: DoomslugThresholdMode,
}

impl DoomslugApprovalsTracker {
    fn new(
        clock: Clock,
        account_id_to_stakes: HashMap<AccountId, (Balance, Balance)>,
        threshold_mode: DoomslugThresholdMode,
    ) -> Self {
        let total_stake_this_epoch = account_id_to_stakes.values().map(|(x, _)| x).sum::<Balance>();
        let total_stake_next_epoch = account_id_to_stakes.values().map(|(_, x)| x).sum::<Balance>();

        DoomslugApprovalsTracker {
            clock,
            witness: Default::default(),
            account_id_to_stakes,
            total_stake_this_epoch,
            total_stake_next_epoch,
            approved_stake_this_epoch: 0,
            approved_stake_next_epoch: 0,
            time_passed_threshold: None,
            threshold_mode,
        }
    }

    /// Given a single approval (either an endorsement or a skip-message) updates the approved
    /// stake on the block that is being approved, and returns whether the block is now ready to be
    /// produced.
    ///
    /// # Arguments
    /// * now      - the current timestamp
    /// * approval - the approval to process
    ///
    /// # Returns
    /// Whether the block is ready to be produced
    fn process_approval(&mut self, approval: &Approval) -> DoomslugBlockProductionReadiness {
        let mut increment_approved_stake = false;
        self.witness.entry(approval.account_id.clone()).or_insert_with(|| {
            increment_approved_stake = true;
            (approval.clone(), self.clock.now_utc())
        });

        if increment_approved_stake {
            let stakes = self.account_id_to_stakes.get(&approval.account_id).map_or((0, 0), |x| *x);
            self.approved_stake_this_epoch += stakes.0;
            self.approved_stake_next_epoch += stakes.1;
        }

        // We call to `get_block_production_readiness` here so that if the number of approvals crossed
        // the threshold, the timer for block production starts.
        self.get_block_production_readiness()
    }

    /// Withdraws an approval. This happens if a newer approval for the same `target_height` comes
    /// from the same account. Removes the approval from the `witness` and updates approved and
    /// endorsed stakes.
    fn withdraw_approval(&mut self, account_id: &AccountId) {
        let approval = match self.witness.remove(account_id) {
            None => return,
            Some(approval) => approval.0,
        };

        let stakes = self.account_id_to_stakes.get(&approval.account_id).map_or((0, 0), |x| *x);
        self.approved_stake_this_epoch -= stakes.0;
        self.approved_stake_next_epoch -= stakes.1;
    }

    /// Returns whether the block has enough approvals, and if yes, since what moment it does.
    ///
    /// # Arguments
    /// * now - the current timestamp
    ///
    /// # Returns
    /// `NotReady` if the block doesn't have enough approvals yet to cross the threshold
    /// `ReadySince` if the block has enough approvals to pass the threshold, and since when it
    ///     does
    fn get_block_production_readiness(&mut self) -> DoomslugBlockProductionReadiness {
        if (self.approved_stake_this_epoch > self.total_stake_this_epoch * 2 / 3
            && (self.approved_stake_next_epoch > self.total_stake_next_epoch * 2 / 3
            || self.total_stake_next_epoch == 0))
            || self.threshold_mode == DoomslugThresholdMode::NoApprovals
        {
            if self.time_passed_threshold == None {
                self.time_passed_threshold = Some(self.clock.now());
            }
            DoomslugBlockProductionReadiness::ReadySince(self.time_passed_threshold.unwrap())
        } else {
            DoomslugBlockProductionReadiness::NotReady
        }
    }
}

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

impl DoomslugApprovalsTrackersAtHeight {
    fn new(clock: Clock) -> Self {
        Self { clock, approval_trackers: HashMap::new(), last_approval_per_account: HashMap::new() }
    }

    /// This method is a wrapper around `DoomslugApprovalsTracker::process_approval`, see comment
    /// above it for more details.
    /// This method has an extra logic that ensures that we only track one approval per `account_id`,
    /// if we already know some other approval for this account, we first withdraw it from the
    /// corresponding tracker, and associate the new approval with the account.
    ///
    /// # Arguments
    /// * `approval` - the approval to be processed
    /// * `stakes`   - all the stakes of all the block producers in the current epoch
    /// * `threshold_mode` - how many approvals are needed to produce a block. Is used to compute
    ///                the return value
    ///
    /// # Returns
    /// Same as `DoomslugApprovalsTracker::process_approval`
    fn process_approval(
        &mut self,
        approval: &Approval,
        stakes: &[(ApprovalStake, bool)],
        threshold_mode: DoomslugThresholdMode,
    ) -> DoomslugBlockProductionReadiness {
        if let Some(last_parent) = self.last_approval_per_account.get(&approval.account_id) {
            let should_remove = self
                .approval_trackers
                .get_mut(last_parent)
                .map(|x| {
                    x.withdraw_approval(&approval.account_id);
                    x.witness.is_empty()
                })
                .unwrap_or(false);

            if should_remove {
                self.approval_trackers.remove(last_parent);
            }
        }

        let account_id_to_stakes = stakes
            .iter()
            .filter_map(|(x, is_slashed)| {
                if *is_slashed {
                    None
                } else {
                    Some((x.account_id.clone(), (x.stake_this_epoch, x.stake_next_epoch)))
                }
            })
            .collect::<HashMap<_, _>>();

        assert_eq!(account_id_to_stakes.len(), stakes.len());

        if !account_id_to_stakes.contains_key(&approval.account_id) {
            return DoomslugBlockProductionReadiness::NotReady;
        }

        self.last_approval_per_account.insert(approval.account_id.clone(), approval.inner.clone());
        self.approval_trackers
            .entry(approval.inner.clone())
            .or_insert_with(|| {
                DoomslugApprovalsTracker::new(
                    self.clock.clone(),
                    account_id_to_stakes,
                    threshold_mode,
                )
            })
            .process_approval(approval)
    }
}

/// Contains all the logic for Doomslug, but no integration with chain or storage. The integration
/// happens via `PersistentDoomslug` struct. The split is to simplify testing of the logic separate
/// from the chain.
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
            largest_final_height: 0,
            largest_threshold_height: 0,
            largest_approval_height: 0,
            tip: DoomslugTip { block_hash: CryptoHash::default(), height: 0 },
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
            assert!(skip_delay >= 2 * self.timer.endorsement_delay);

            let tip_height = self.tip.height;

            if self.endorsement_pending
                && now >= self.timer.last_endorsement_sent + self.timer.endorsement_delay
            {
                if tip_height >= self.largest_target_height {
                    self.largest_target_height = tip_height + 1;

                    if let Some(approval) = self.create_approval(tip_height + 1, signer) {
                        ret.push(approval);
                    }
                }

                self.timer.last_endorsement_sent = now;
                self.endorsement_pending = false;
            }

            if now >= self.timer.started + skip_delay {
                assert!(!self.endorsement_pending);

                self.largest_target_height
                    = std::cmp::max(self.timer.height + 1, self.largest_target_height);

                if let Some(approval) = self.create_approval(self.timer.height + 1, signer) {
                    ret.push(approval);
                }

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
            Approval::new(self.tip.block_hash, self.tip.height, target_height, &*signer)
        })
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
        last_final_height: BlockHeight,
    ) {
        assert!(height > self.tip.height || self.tip.height == 0);
        self.tip = DoomslugTip { block_hash, height };

        self.largest_final_height = last_final_height;
        self.timer.height = height + 1;
        self.timer.started = self.clock.now();

        self.endorsement_pending = true;
    }

    /// Records an approval message, and return whether the block has passed the threshold / ready
    /// to be produced without waiting any further. See the comment for `DoomslugApprovalTracker::process_approval`
    /// for details
    #[must_use]
    fn on_approval_message_internal(
        &mut self,
        approval: &Approval,
        stakes: &[(ApprovalStake, bool)],
    ) -> DoomslugBlockProductionReadiness {
        let threshold_mode = self.threshold_mode;
        let ret = self
            .approval_tracking
            .entry(approval.target_height)
            .or_insert_with(|| DoomslugApprovalsTrackersAtHeight::new(self.clock.clone()))
            .process_approval(approval, stakes, threshold_mode);

        if approval.target_height > self.largest_approval_height {
            self.largest_approval_height = approval.target_height;
        }

        if ret != DoomslugBlockProductionReadiness::NotReady {
            if approval.target_height > self.largest_threshold_height {
                self.largest_threshold_height = approval.target_height;
            }
        }

        ret
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use crate::clock::{Duration, FakeClock, Utc};
    use crate::doomslugs::{Doomslug, DoomslugBlockProductionReadiness, DoomslugThresholdMode};
    use crate::model::{Approval, ApprovalInner, ApprovalStake, hash, SecretKey};
    use crate::tests::create_test_signer;

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
            DoomslugThresholdMode::NoApprovals,
        );

        // Set a new tip, must produce an endorsement
        ds.set_tip(hash(&[1]), 1, 1);
        clock.advance(Duration::milliseconds(399));
        assert_eq!(ds.process_timer(&signer).len(), 0);
        clock.advance(Duration::milliseconds(1));
        let approval = ds.process_timer(&signer).into_iter().nth(0).unwrap();
        assert_eq!(approval.inner, ApprovalInner::Endorsement(hash(&[1])));
        assert_eq!(approval.target_height, 2);

        // Same tip => no approval
        assert_eq!(ds.process_timer(&signer), vec![]);

        // The block was `ds_final` and therefore started the timer. Try checking before one second expires
        clock.advance(Duration::milliseconds(599));
        assert_eq!(ds.process_timer(&signer), vec![]);

        // But one second should trigger the skip
        clock.advance(Duration::milliseconds(1));
        match ds.process_timer(&signer) {
            approvals if approvals.is_empty() => assert!(false),
            approvals => {
                assert_eq!(approvals[0].inner, ApprovalInner::Skip(1));
                assert_eq!(approvals[0].target_height, 3);
            }
        }

        // Not processing a block at height 2 should not produce an appoval
        ds.set_tip(hash(&[2]), 2, 0);
        clock.advance(Duration::milliseconds(400));
        assert_eq!(ds.process_timer(&signer), vec![]);

        // Go forward more so we have 1 second
        clock.advance(Duration::milliseconds(600));

        // But at height 3 should (also neither block has finality set, keep last final at 0 for now)
        ds.set_tip(hash(&[3]), 3, 0);
        clock.advance(Duration::milliseconds(400));
        let approval = ds.process_timer(&signer).into_iter().nth(0).unwrap();
        assert_eq!(approval.inner, ApprovalInner::Endorsement(hash(&[3])));
        assert_eq!(approval.target_height, 4);

        // Go forward more so we have another second
        clock.advance(Duration::milliseconds(600));

        clock.advance(Duration::milliseconds(199));
        assert_eq!(ds.process_timer(&signer), vec![]);

        clock.advance(Duration::milliseconds(1));
        match ds.process_timer(&signer) {
            approvals if approvals.is_empty() => assert!(false),
            approvals if approvals.len() == 1 => {
                assert_eq!(approvals[0].inner, ApprovalInner::Skip(3));
                assert_eq!(approvals[0].target_height, 5);
            }
            _ => assert!(false),
        }

        // Go forward more so we have another second
        clock.advance(Duration::milliseconds(800));

        // Now skip 5 (the extra delay is 200+300 = 500)
        clock.advance(Duration::milliseconds(499));
        assert_eq!(ds.process_timer(&signer), vec![]);

        clock.advance(Duration::milliseconds(1));
        match ds.process_timer(&signer) {
            approvals if approvals.is_empty() => assert!(false),
            approvals => {
                assert_eq!(approvals[0].inner, ApprovalInner::Skip(3));
                assert_eq!(approvals[0].target_height, 6);
            }
        }

        // Go forward more so we have another second
        clock.advance(Duration::milliseconds(500));

        // Skip 6 (the extra delay is 0+200+300+400 = 900)
        clock.advance(Duration::milliseconds(899));
        assert_eq!(ds.process_timer(&signer), vec![]);

        clock.advance(Duration::milliseconds(1));
        match ds.process_timer(&signer) {
            approvals if approvals.is_empty() => assert!(false),
            approvals => {
                assert_eq!(approvals[0].inner, ApprovalInner::Skip(3));
                assert_eq!(approvals[0].target_height, 7);
            }
        }

        // Go forward more so we have another second
        clock.advance(Duration::milliseconds(100));

        // Accept block at 5 with finality on the prev block, expect it to not produce an approval
        ds.set_tip(hash(&[5]), 5, 4);
        clock.advance(Duration::milliseconds(400));
        assert_eq!(ds.process_timer(&signer), vec![]);

        // Skip a whole bunch of heights by moving 100 seconds ahead
        clock.advance(Duration::seconds(100));
        assert!(ds.process_timer(&signer).len() > 10);

        // Add some random small number of milliseconds to test that when the next block is added, the
        // timer is reset
        clock.advance(Duration::milliseconds(17));

        // No approval, since we skipped 6
        ds.set_tip(hash(&[6]), 6, 4);
        clock.advance(Duration::milliseconds(400));
        assert_eq!(ds.process_timer(&signer), vec![]);

        // The block height was less than the timer height, and thus the timer was reset.
        // The wait time for height 7 with last ds final block at 5 is 1100
        clock.advance(Duration::milliseconds(699));
        assert_eq!(ds.process_timer(&signer), vec![]);

        clock.advance(Duration::milliseconds(1));
        match ds.process_timer(&signer) {
            approvals if approvals.is_empty() => assert!(false),
            approvals => {
                assert_eq!(approvals[0].inner, ApprovalInner::Skip(6));
                assert_eq!(approvals[0].target_height, 8);
            }
        }
    }

    #[test]
    fn test_doomslug_approvals() {
        let accounts: Vec<(&str, u128, u128)> =
            vec![("test1", 2, 0), ("test2", 1, 0), ("test3", 3, 0), ("test4", 1, 0)];
        let stakes = accounts
            .iter()
            .map(|(account_id, stake_this_epoch, stake_next_epoch)| ApprovalStake {
                account_id: account_id.parse().unwrap(),
                stake_this_epoch: *stake_this_epoch,
                stake_next_epoch: *stake_next_epoch,
                public_key: SecretKey::from_seed(account_id).public_key(),
            })
            .map(|stake| (stake, false))
            .collect::<Vec<_>>();
        let signers = accounts
            .iter()
            .map(|(account_id, _, _)| create_test_signer(account_id))
            .collect::<Vec<_>>();

        let clock = FakeClock::new(Utc::UNIX_EPOCH);
        let mut ds = Doomslug::new(
            clock.clock(),
            0,
            Duration::milliseconds(400),
            Duration::milliseconds(1000),
            Duration::milliseconds(100),
            Duration::milliseconds(3000),
            DoomslugThresholdMode::TwoThirds,
        );

        // In the comments below the format is
        // account, height -> approved stake
        // The total stake is 7, so the threshold is 5

        // "test1", 2 -> 2
        assert_eq!(
            ds.on_approval_message_internal(
                &Approval::new(hash(&[1]), 1, 2, &signers[0]),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::NotReady,
        );

        // "test3", 4 -> 3
        assert_eq!(
            ds.on_approval_message_internal(
                &Approval::new(hash(&[1]), 1, 4, &signers[2]),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::NotReady,
        );

        // "test4", 4 -> 4
        assert_eq!(
            ds.on_approval_message_internal(
                &Approval::new(hash(&[1]), 1, 4, &signers[3]),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::NotReady,
        );

        clock.advance(Duration::milliseconds(100));

        // "test1", 4 -> same account, still 5
        assert_eq!(
            ds.on_approval_message_internal(
                &Approval::new(hash(&[1]), 1, 4, &signers[3]),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::NotReady,
        );

        // "test2", 4 -> 5
        assert_eq!(
            ds.on_approval_message_internal(
                &Approval::new(hash(&[1]), 1, 4, &signers[1]),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::ReadySince(clock.now()),
        );

        // "test1", 4 -> 7
        assert_eq!(
            ds.on_approval_message_internal(
                &Approval::new(hash(&[1]), 1, 4, &signers[0]),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::ReadySince(clock.now()),
        );

        // "test4", 2 -> 3
        assert_eq!(
            ds.on_approval_message_internal(
                &Approval::new(hash(&[1]), 1, 2, &signers[3]),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::NotReady,
        );

        clock.advance(Duration::milliseconds(200));

        // "test3", 2 -> 6
        assert_eq!(
            ds.on_approval_message_internal(
                &Approval::new(hash(&[1]), 1, 2, &signers[2]),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::ReadySince(clock.now()),
        );

        // A different parent hash
        assert_eq!(
            ds.on_approval_message_internal(
                &Approval::new(hash(&[2]), 2, 4, &signers[1]),
                &stakes,
            ),
            DoomslugBlockProductionReadiness::NotReady,
        );
    }
}
