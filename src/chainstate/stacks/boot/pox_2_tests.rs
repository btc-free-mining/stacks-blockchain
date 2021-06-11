use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::convert::TryInto;

use address::AddressHashMode;
use chainstate::burn::ConsensusHash;
use chainstate::stacks::boot::{
    BOOT_CODE_COST_VOTING_TESTNET as BOOT_CODE_COST_VOTING, BOOT_CODE_POX_TESTNET,
};
use chainstate::stacks::db::{MinerPaymentSchedule, StacksHeaderInfo, MINER_REWARD_MATURITY};
use chainstate::stacks::index::MarfTrieId;
use chainstate::stacks::*;
use clarity_vm::database::marf::MarfedKV;
use core::*;
use util::db::{DBConn, FromRow};
use util::hash::to_hex;
use util::hash::{Sha256Sum, Sha512Trunc256Sum};
use vm::contexts::OwnedEnvironment;
use vm::contracts::Contract;
use vm::costs::CostOverflowingMath;
use vm::database::*;
use vm::errors::{
    CheckErrors, Error, IncomparableError, InterpreterError, InterpreterResult, RuntimeErrorType,
};
use vm::eval;
use vm::representations::SymbolicExpression;
use vm::tests::{execute, is_committed, is_err_code, symbols_from_values};
use vm::types::Value::Response;
use vm::types::{
    OptionalData, PrincipalData, QualifiedContractIdentifier, ResponseData, StandardPrincipalData,
    TupleData, TupleTypeSignature, TypeSignature, Value, NONE,
};

use crate::{
    burnchains::Burnchain,
    chainstate::{burn::db::sortdb::SortitionDB, stacks::miner::test::make_coinbase},
    clarity_vm::{clarity::ClarityBlockConnection, database::marf::WritableMarfStore},
    util::boot::boot_code_id,
};
use types::chainstate::{
    BlockHeaderHash, BurnchainHeaderHash, StacksAddress, StacksBlockId, VRFSeed,
};
use types::proof::{ClarityMarfTrieId, TrieMerkleProof};

use clarity_vm::clarity::Error as ClarityError;

use super::test::*;

const USTX_PER_HOLDER: u128 = 1_000_000;

/// In this test case, two Stackers, Alice and Bob stack and interact with the
///  PoX v1 contract and PoX v2 contract across the epoch transition.
///
/// Alice: stacks via PoX v1 for 4 cycles. The third of these cycles occurs after
///        the PoX v1 -> v2 transition, and so Alice gets "early unlocked".
///        After the early unlock, Alice re-stacks in PoX v2
///        Alice tries to stack again via PoX v1, which is allowed by the contract,
///        but forbidden by the VM (because PoX has transitioned to v2)
/// Bob:   stacks via PoX v2 for 6 cycles. He attempted to stack via PoX v1 as well,
///        but is forbidden because he has already placed an account lock via PoX v2.
///
#[test]
fn test_simple_pox_lockup_transition_pox_2() {
    let AUTO_UNLOCK_HT = 12;
    let mut burnchain = Burnchain::default_unittest(0, &BurnchainHeaderHash::zero());
    burnchain.pox_constants.reward_cycle_length = 5;
    burnchain.pox_constants.prepare_length = 2;
    burnchain.pox_constants.anchor_threshold = 1;
    burnchain.pox_constants.v1_unlock_height = AUTO_UNLOCK_HT + 25;

    let first_v2_cycle = burnchain
        .block_height_to_reward_cycle(burnchain.pox_constants.v1_unlock_height as u64)
        .unwrap()
        + 1;

    eprintln!("First v2 cycle = {}", first_v2_cycle);

    let epochs = StacksEpoch::all(0, 25 + 10);

    let (mut peer, mut keys) = instantiate_pox_peer_with_epoch(
        &burnchain,
        "test_simple_pox_lockup_transition_pox_2",
        6002,
        Some(epochs.clone()),
    );

    let num_blocks = 35;

    let alice = keys.pop().unwrap();
    let bob = keys.pop().unwrap();
    let charlie = keys.pop().unwrap();

    let mut alice_reward_cycle = 0;

    for tenure_id in 0u32..num_blocks {
        let microblock_privkey = StacksPrivateKey::new();
        let microblock_pubkeyhash =
            Hash160::from_node_public_key(&StacksPublicKey::from_private(&microblock_privkey));
        let tip = SortitionDB::get_canonical_burn_chain_tip(&peer.sortdb.as_ref().unwrap().conn())
            .unwrap();

        let (burn_ops, stacks_block, microblocks) = peer.make_tenure(
            |ref mut miner,
             ref mut sortdb,
             ref mut chainstate,
             vrf_proof,
             ref parent_opt,
             ref parent_microblock_header_opt| {
                let parent_tip = get_parent_tip(parent_opt, chainstate, sortdb);
                let coinbase_tx = make_coinbase(miner, tenure_id as usize);

                let mut block_txs = vec![coinbase_tx];

                if tenure_id == 1 {
                    // Alice locks up exactly 25% of the liquid STX supply, so this should succeed.
                    // this locks for cycles 6, 7, 8, 9.
                    //  however, the v2 unlock occurs before cycle 8, so alice will be unlocked before cycles 8 and 9
                    let alice_lockup = make_pox_lockup(
                        &alice,
                        0,
                        1024 * POX_THRESHOLD_STEPS_USTX,
                        AddressHashMode::SerializeP2PKH,
                        key_to_stacks_addr(&alice).bytes,
                        4,
                        tip.block_height,
                    );
                    block_txs.push(alice_lockup);
                } else if tenure_id == 10 {
                    // Lets have Bob lock up for v2
                    // this will lock for cycles 8, 9, 10, and 11
                    //  the first v2 cycle will be 8
                    let bob_lockup = make_pox_2_lockup(
                        &bob,
                        0,
                        512 * POX_THRESHOLD_STEPS_USTX,
                        AddressHashMode::SerializeP2PKH,
                        key_to_stacks_addr(&bob).bytes,
                        6,
                        tip.block_height,
                    );
                    block_txs.push(bob_lockup);
                } else if tenure_id == 11 {
                    // Bob tries to lock in PoX v1 too, but it shouldn't work!
                    let bob_lockup = make_pox_lockup(
                        &bob,
                        1,
                        512 * POX_THRESHOLD_STEPS_USTX,
                        AddressHashMode::SerializeP2PKH,
                        key_to_stacks_addr(&bob).bytes,
                        4,
                        tip.block_height,
                    );
                    block_txs.push(bob_lockup);
                } else if tenure_id == 13 {
                    // Alice auto-unlocked, so stack in PoX v2
                    let alice_lockup = make_pox_2_lockup(
                        &alice,
                        1,
                        512 * POX_THRESHOLD_STEPS_USTX,
                        AddressHashMode::SerializeP2PKH,
                        key_to_stacks_addr(&alice).bytes,
                        12,
                        tip.block_height,
                    );
                    block_txs.push(alice_lockup);
                } else if tenure_id == 31 {
                    // Alice would have unlocked under v1 rules, so try to stack again via PoX 1 and expect a runtime error
                    //  in the tx.
                    let alice_lockup = make_pox_lockup(
                        &alice,
                        2,
                        512 * POX_THRESHOLD_STEPS_USTX,
                        AddressHashMode::SerializeP2PKH,
                        key_to_stacks_addr(&alice).bytes,
                        12,
                        tip.block_height,
                    );
                    block_txs.push(alice_lockup);
                }

                let block_builder = StacksBlockBuilder::make_regtest_block_builder(
                    &parent_tip,
                    vrf_proof,
                    tip.total_burn,
                    microblock_pubkeyhash,
                )
                .unwrap();
                let (anchored_block, _size, _cost) =
                    StacksBlockBuilder::make_anchored_block_from_txs(
                        block_builder,
                        chainstate,
                        &sortdb.index_conn(),
                        block_txs,
                    )
                    .unwrap();
                (anchored_block, vec![])
            },
        );

        let (_, _, consensus_hash) = peer.next_burnchain_block(burn_ops);
        peer.process_stacks_epoch_at_tip(&stacks_block, &microblocks);

        let total_liquid_ustx = get_liquid_ustx(&mut peer);
        let tip_index_block =
            StacksBlockHeader::make_index_block_hash(&consensus_hash, &stacks_block.block_hash());

        eprintln!("tenure_id: {}", tenure_id);
        let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());

        let expected_alice_balance = if tenure_id < 1 {
            1024 * POX_THRESHOLD_STEPS_USTX
        } else if tenure_id < AUTO_UNLOCK_HT {
            0
        } else if tenure_id < 13 {
            1024 * POX_THRESHOLD_STEPS_USTX
        } else {
            512 * POX_THRESHOLD_STEPS_USTX
        };

        assert_eq!(alice_balance, expected_alice_balance);

        if tenure_id <= 1 {
            if tenure_id < 1 {
                // Alice has not locked up STX
                let alice_balance = get_balance(&mut peer, &key_to_stacks_addr(&alice).into());
                assert_eq!(alice_balance, 1024 * POX_THRESHOLD_STEPS_USTX);

                let alice_account = get_account(&mut peer, &key_to_stacks_addr(&alice).into());
                assert_eq!(
                    alice_account.stx_balance.amount_unlocked(),
                    1024 * POX_THRESHOLD_STEPS_USTX
                );
                assert_eq!(alice_account.stx_balance.amount_locked(), 0);
                assert_eq!(alice_account.stx_balance.unlock_height(), 0);
            }
            let min_ustx = with_sortdb(&mut peer, |ref mut chainstate, ref sortdb| {
                chainstate.get_stacking_minimum(sortdb, &tip_index_block)
            })
            .unwrap();
            assert_eq!(min_ustx, total_liquid_ustx / 480);

            // no reward addresses
            let reward_addrs = with_sortdb(&mut peer, |ref mut chainstate, ref sortdb| {
                get_reward_addresses_with_par_tip(chainstate, &burnchain, sortdb, &tip_index_block)
            })
            .unwrap();
            assert_eq!(reward_addrs.len(), 0);

            // record the first reward cycle when Alice's tokens get stacked
            let tip_burn_block_height =
                get_par_burn_block_height(peer.chainstate(), &tip_index_block);
            alice_reward_cycle = 1 + burnchain
                .block_height_to_reward_cycle(tip_burn_block_height)
                .unwrap() as u128;
            let cur_reward_cycle = burnchain
                .block_height_to_reward_cycle(tip_burn_block_height)
                .unwrap() as u128;

            eprintln!(
                "\nalice reward cycle: {}\ncur reward cycle: {}\n",
                alice_reward_cycle, cur_reward_cycle
            );
        } else {
            // Alice's address is locked as of the next reward cycle
            let tip_burn_block_height =
                get_par_burn_block_height(peer.chainstate(), &tip_index_block);
            let cur_reward_cycle = burnchain
                .block_height_to_reward_cycle(tip_burn_block_height)
                .unwrap() as u128;

            let (min_ustx, reward_addrs, total_stacked) =
                with_sortdb(&mut peer, |ref mut c, ref sortdb| {
                    (
                        c.get_stacking_minimum(sortdb, &tip_index_block).unwrap(),
                        get_reward_addresses_with_par_tip(c, &burnchain, sortdb, &tip_index_block)
                            .unwrap(),
                        c.test_get_total_ustx_stacked(sortdb, &tip_index_block, cur_reward_cycle)
                            .unwrap(),
                    )
                });

            eprintln!("\ntenure: {}\nreward cycle: {}\nmin-uSTX: {}\naddrs: {:?}\ntotal_liquid_ustx: {}\ntotal-stacked: {}\n", tenure_id, cur_reward_cycle, min_ustx, &reward_addrs, total_liquid_ustx, total_stacked);

            if cur_reward_cycle >= alice_reward_cycle {
                if cur_reward_cycle < first_v2_cycle as u128 {
                    let (amount_ustx, pox_addr, lock_period, first_reward_cycle) =
                        get_stacker_info(&mut peer, &key_to_stacks_addr(&alice).into()).unwrap();
                    eprintln!("\nAlice: {} uSTX stacked for {} cycle(s); addr is {:?}; first reward cycle is {}\n", amount_ustx, lock_period, &pox_addr, first_reward_cycle);

                    // one reward address, and it's Alice's
                    // either way, there's a single reward address
                    assert_eq!(reward_addrs.len(), 1);
                    assert_eq!(
                        (reward_addrs[0].0).version,
                        AddressHashMode::SerializeP2PKH.to_version_testnet()
                    );
                    assert_eq!((reward_addrs[0].0).bytes, key_to_stacks_addr(&alice).bytes);
                    assert_eq!(reward_addrs[0].1, 1024 * POX_THRESHOLD_STEPS_USTX);
                } else {
                    // v2 reward cycles have begun, so reward addrs should be read from PoX2 which is Bob + Alice
                    assert_eq!(reward_addrs.len(), 2);
                    assert_eq!(
                        (reward_addrs[0].0).version,
                        AddressHashMode::SerializeP2PKH.to_version_testnet()
                    );
                    assert_eq!((reward_addrs[0].0).bytes, key_to_stacks_addr(&bob).bytes);
                    assert_eq!(reward_addrs[0].1, 512 * POX_THRESHOLD_STEPS_USTX);

                    assert_eq!(
                        (reward_addrs[1].0).version,
                        AddressHashMode::SerializeP2PKH.to_version_testnet()
                    );
                    assert_eq!((reward_addrs[1].0).bytes, key_to_stacks_addr(&alice).bytes);
                    assert_eq!(reward_addrs[1].1, 512 * POX_THRESHOLD_STEPS_USTX);
                }
            } else {
                // no reward addresses
                assert_eq!(reward_addrs.len(), 0);
            }
        }
    }
}