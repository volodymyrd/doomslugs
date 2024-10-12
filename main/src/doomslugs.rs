use std::sync::Arc;
use crate::clock::{Clock, Duration, Instant};
use crate::model::{Approval, BlockHeight, BlockHeightDelta, CryptoHash, ValidatorSigner};

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

/// Contains all the logic for Doomslug, but no integration with chain or storage. The integration
/// happens via `PersistentDoomslug` struct. The split is to simplify testing of the logic separate
/// from the chain.
pub struct Doomslug {
    clock: Clock,
    /// Largest target height for which we issued an approval
    largest_target_height: BlockHeight,
    /// Largest height for which we saw a block containing 1/2 endorsements in it
    largest_final_height: BlockHeight,
    /// Information Doomslug tracks about the chain tip
    tip: DoomslugTip,
    /// Whether an endorsement (or in general an approval) was sent since updating the tip
    endorsement_pending: bool,
    /// Information to track the timer (see `start_timer` routine in the paper)
    timer: DoomslugTimer,
}

impl Doomslug {
    pub fn new(
        clock: Clock,
        largest_target_height: BlockHeight,
        endorsement_delay: Duration,
        min_delay: Duration,
        delay_step: Duration,
        max_delay: Duration,
    ) -> Self {
        Doomslug {
            clock: clock.clone(),
            largest_target_height,
            largest_final_height: 0,
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use crate::clock::{Duration, FakeClock, Utc};
    use crate::doomslugs::{Doomslug};
    use crate::model::{ApprovalInner, hash};
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
}
