use crate::model::{ValidatorSigner};

// #[test]
// fn test_fuzzy_doomslug_liveness_and_safety() {
//     println!("Start test...");
//     for (time_to_gst_millis, height_goal) in &[(0, 200), (1000, 200)] {
//         for delta in &[100] {
//             println!(
//                 "Staring set of tests. Time to GST: {}, delta: {}",
//                 time_to_gst_millis, delta
//             );
//             for _ in 0..1 {
//                 let (took, height) = one_iter(
//                     Duration::milliseconds(*time_to_gst_millis),
//                     Duration::milliseconds(*delta),
//                     *height_goal,
//                 );
//                 println!(
//                     " --> Took {} (simulated) milliseconds and {} heights",
//                     took.whole_milliseconds(),
//                     height
//                 );
//             }
//         }
//     }
// }

// fn one_iter(
//     time_to_gst: Duration,
//     delta: Duration,
//     height_goal: BlockHeight,
// ) -> (Duration, BlockHeight) {
//     let account_ids = ["test1"];
//
//     let stakes = account_ids
//         .iter()
//         .map(|account_id| ApprovalStake {
//             account_id: account_id.parse().unwrap(),
//             stake_this_epoch: 1,
//             stake_next_epoch: 1,
//             public_key: SecretKey::from_seed(account_id).public_key(),
//         })
//         .map(|stake| (stake, false))
//         .collect::<Vec<_>>();
//
//     let signers = account_ids
//         .iter()
//         .map(|account_id| Some(Arc::new(create_test_signer(account_id))))
//         .collect::<Vec<_>>();
//
//
//
//     (Duration::default(), BlockHeight::default())
// }

pub fn create_test_signer(account_name: &str) -> ValidatorSigner {
    ValidatorSigner::from_seed(account_name.parse().unwrap(), account_name).into()
}

// fn block_hash(height: BlockHeight, ord: usize) -> CryptoHash {
//     hash(([height.to_le_bytes(), ord.to_le_bytes()].concat()).as_ref())
// }
