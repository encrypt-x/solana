//! The `sigverify` module provides digital signature verification functions.
//! By default, signatures are verified in parallel using all available CPU
//! cores.  When perf-libs are available signature verification is offloaded
//! to the GPU.
//!

use crate::cuda_runtime::PinnedVec;
use crate::packet::{Packet, Packets};
use crate::recycler::Recycler;
use crate::result::Result;
use bincode::serialized_size;
use rayon::ThreadPool;
use solana_ledger::perf_libs;
use solana_metrics::inc_new_counter_debug;
use solana_sdk::message::MessageHeader;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::short_vec::decode_len;
use solana_sdk::signature::Signature;
#[cfg(test)]
use solana_sdk::transaction::Transaction;
use std::mem::size_of;

use solana_rayon_threadlimit::get_thread_count;
pub const NUM_THREADS: u32 = 10;
use std::cell::RefCell;

thread_local!(static PAR_THREAD_POOL: RefCell<ThreadPool> = RefCell::new(rayon::ThreadPoolBuilder::new()
                    .num_threads(get_thread_count())
                    .build()
                    .unwrap()));

pub type TxOffset = PinnedVec<u32>;

type TxOffsets = (TxOffset, TxOffset, TxOffset, TxOffset, Vec<Vec<u32>>);

#[derive(Debug, PartialEq, Eq)]
struct PacketOffsets {
    pub sig_len: u32,
    pub sig_start: u32,
    pub msg_start: u32,
    pub pubkey_start: u32,
}

impl PacketOffsets {
    pub fn new(sig_len: u32, sig_start: u32, msg_start: u32, pubkey_start: u32) -> Self {
        Self {
            sig_len,
            sig_start,
            msg_start,
            pubkey_start,
        }
    }
}

struct UnsanitizedPacketOffsets {
    pub correct: bool,
    pub packet_offsets: PacketOffsets,
}

impl UnsanitizedPacketOffsets {
    pub fn new(
        correct: bool,
        sig_len: u32,
        sig_start: u32,
        msg_start: u32,
        pubkey_start: u32,
    ) -> Self {
        Self {
            correct,
            packet_offsets: PacketOffsets::new(sig_len, sig_start, msg_start, pubkey_start),
        }
    }
}

pub fn init() {
    if let Some(api) = perf_libs::api() {
        unsafe {
            (api.ed25519_set_verbose)(true);
            if !(api.ed25519_init)() {
                panic!("ed25519_init() failed");
            }
            (api.ed25519_set_verbose)(false);
        }
    }
}

fn verify_packet(packet: &Packet) -> u8 {
    let packet_offsets = get_packet_offsets(packet, 0);
    let mut sig_start = packet_offsets.sig_start as usize;
    let mut pubkey_start = packet_offsets.pubkey_start as usize;
    let msg_start = packet_offsets.msg_start as usize;

    if packet_offsets.sig_len == 0 {
        return 0;
    }

    if packet.meta.size <= msg_start {
        return 0;
    }

    let msg_end = packet.meta.size;
    for _ in 0..packet_offsets.sig_len {
        let pubkey_end = pubkey_start as usize + size_of::<Pubkey>();
        let sig_end = sig_start as usize + size_of::<Signature>();

        if pubkey_end >= packet.meta.size || sig_end >= packet.meta.size {
            return 0;
        }

        let signature = Signature::new(&packet.data[sig_start..sig_end]);
        if !signature.verify(
            &packet.data[pubkey_start..pubkey_end],
            &packet.data[msg_start..msg_end],
        ) {
            return 0;
        }
        pubkey_start += size_of::<Pubkey>();
        sig_start += size_of::<Signature>();
    }
    1
}

fn batch_size(batches: &[Packets]) -> usize {
    batches.iter().map(|p| p.packets.len()).sum()
}

// internal function to be unit-tested; should be used only by get_packet_offsets
fn do_get_packet_offsets(packet: &Packet, current_offset: u32) -> UnsanitizedPacketOffsets {
    // This directly reads the length of Transaction.signatures (serialized with short_vec)
    let (sig_len_untrusted, sig_size) = decode_len(&packet.data);

    // This directly reads MessageHeader.num_required_signatures (serialized with u8)
    let msg_start_offset = sig_size + sig_len_untrusted * size_of::<Signature>();
    // Using msg_start_offset which is based on sig_len_untrusted introduces uncertainty.
    // Ultimately, the actual sigverify will determine the uncertainty.
    let sig_len_maybe_trusted = packet.data[msg_start_offset] as usize;

    let msg_header_size = serialized_size(&MessageHeader::default()).unwrap() as usize;

    // This directly reads the length of Message.account_keys (serialized with short_vec)
    let (_pubkey_len, pubkey_len_size) =
        decode_len(&packet.data[(msg_start_offset + msg_header_size)..]);

    let sig_start = current_offset as usize + sig_size;
    let msg_start = current_offset as usize + msg_start_offset;
    let pubkey_start = msg_start + msg_header_size + pubkey_len_size;

    UnsanitizedPacketOffsets::new(
        sig_len_maybe_trusted == sig_len_untrusted,
        sig_len_untrusted as u32,
        sig_start as u32,
        msg_start as u32,
        pubkey_start as u32,
    )
}

fn get_packet_offsets(packet: &Packet, current_offset: u32) -> PacketOffsets {
    let unsanitized_packet_offsets = do_get_packet_offsets(packet, current_offset);
    if unsanitized_packet_offsets.correct {
        unsanitized_packet_offsets.packet_offsets
    } else {
        // force sigverify to fail by returning zeros
        PacketOffsets::new(0, 0, 0, 0)
    }
}

pub fn generate_offsets(batches: &[Packets], recycler: &Recycler<TxOffset>) -> Result<TxOffsets> {
    debug!("allocating..");
    let mut signature_offsets: PinnedVec<_> = recycler.allocate("sig_offsets");
    signature_offsets.set_pinnable();
    let mut pubkey_offsets: PinnedVec<_> = recycler.allocate("pubkey_offsets");
    pubkey_offsets.set_pinnable();
    let mut msg_start_offsets: PinnedVec<_> = recycler.allocate("msg_start_offsets");
    msg_start_offsets.set_pinnable();
    let mut msg_sizes: PinnedVec<_> = recycler.allocate("msg_size_offsets");
    msg_sizes.set_pinnable();
    let mut current_packet = 0;
    let mut v_sig_lens = Vec::new();
    batches.iter().for_each(|p| {
        let mut sig_lens = Vec::new();
        p.packets.iter().for_each(|packet| {
            let current_offset = current_packet as u32 * size_of::<Packet>() as u32;

            let packet_offsets = get_packet_offsets(packet, current_offset);

            sig_lens.push(packet_offsets.sig_len);

            trace!("pubkey_offset: {}", packet_offsets.pubkey_start);

            let mut pubkey_offset = packet_offsets.pubkey_start;
            let mut sig_offset = packet_offsets.sig_start;
            for _ in 0..packet_offsets.sig_len {
                signature_offsets.push(sig_offset);
                sig_offset += size_of::<Signature>() as u32;

                pubkey_offsets.push(pubkey_offset);
                pubkey_offset += size_of::<Pubkey>() as u32;

                msg_start_offsets.push(packet_offsets.msg_start);

                msg_sizes
                    .push(current_offset + (packet.meta.size as u32) - packet_offsets.msg_start);
            }
            current_packet += 1;
        });
        v_sig_lens.push(sig_lens);
    });
    Ok((
        signature_offsets,
        pubkey_offsets,
        msg_start_offsets,
        msg_sizes,
        v_sig_lens,
    ))
}

pub fn ed25519_verify_cpu(batches: &[Packets]) -> Vec<Vec<u8>> {
    use rayon::prelude::*;
    let count = batch_size(batches);
    debug!("CPU ECDSA for {}", batch_size(batches));
    let rv = PAR_THREAD_POOL.with(|thread_pool| {
        thread_pool.borrow().install(|| {
            batches
                .into_par_iter()
                .map(|p| p.packets.par_iter().map(verify_packet).collect())
                .collect()
        })
    });
    inc_new_counter_debug!("ed25519_verify_cpu", count);
    rv
}

pub fn ed25519_verify_disabled(batches: &[Packets]) -> Vec<Vec<u8>> {
    use rayon::prelude::*;
    let count = batch_size(batches);
    debug!("disabled ECDSA for {}", batch_size(batches));
    let rv = batches
        .into_par_iter()
        .map(|p| vec![1u8; p.packets.len()])
        .collect();
    inc_new_counter_debug!("ed25519_verify_disabled", count);
    rv
}

pub fn ed25519_verify(
    batches: &[Packets],
    recycler: &Recycler<TxOffset>,
    recycler_out: &Recycler<PinnedVec<u8>>,
) -> Vec<Vec<u8>> {
    let api = perf_libs::api();
    if api.is_none() {
        return ed25519_verify_cpu(batches);
    }
    let api = api.unwrap();

    use crate::packet::PACKET_DATA_SIZE;
    let count = batch_size(batches);

    // micro-benchmarks show GPU time for smallest batch around 15-20ms
    // and CPU speed for 64-128 sigverifies around 10-20ms. 64 is a nice
    // power-of-two number around that accounting for the fact that the CPU
    // may be busy doing other things while being a real validator
    // TODO: dynamically adjust this crossover
    if count < 64 {
        return ed25519_verify_cpu(batches);
    }

    let (signature_offsets, pubkey_offsets, msg_start_offsets, msg_sizes, sig_lens) =
        generate_offsets(batches, recycler).unwrap();

    debug!("CUDA ECDSA for {}", batch_size(batches));
    debug!("allocating out..");
    let mut out = recycler_out.allocate("out_buffer");
    out.set_pinnable();
    let mut elems = Vec::new();
    let mut rvs = Vec::new();

    let mut num_packets = 0;
    for p in batches {
        elems.push(perf_libs::Elems {
            elems: p.packets.as_ptr(),
            num: p.packets.len() as u32,
        });
        let mut v = Vec::new();
        v.resize(p.packets.len(), 0);
        rvs.push(v);
        num_packets += p.packets.len();
    }
    out.resize(signature_offsets.len(), 0);
    trace!("Starting verify num packets: {}", num_packets);
    trace!("elem len: {}", elems.len() as u32);
    trace!("packet sizeof: {}", size_of::<Packet>() as u32);
    trace!("len offset: {}", PACKET_DATA_SIZE as u32);
    const USE_NON_DEFAULT_STREAM: u8 = 1;
    unsafe {
        let res = (api.ed25519_verify_many)(
            elems.as_ptr(),
            elems.len() as u32,
            size_of::<Packet>() as u32,
            num_packets as u32,
            signature_offsets.len() as u32,
            msg_sizes.as_ptr(),
            pubkey_offsets.as_ptr(),
            signature_offsets.as_ptr(),
            msg_start_offsets.as_ptr(),
            out.as_mut_ptr(),
            USE_NON_DEFAULT_STREAM,
        );
        if res != 0 {
            trace!("RETURN!!!: {}", res);
        }
    }
    trace!("done verify");
    let mut num = 0;
    for (vs, sig_vs) in rvs.iter_mut().zip(sig_lens.iter()) {
        for (v, sig_v) in vs.iter_mut().zip(sig_vs.iter()) {
            if *sig_v == 0 {
                *v = 0;
            } else {
                let mut vout = 1;
                for _ in 0..*sig_v {
                    if 0 == out[num] {
                        vout = 0;
                    }
                    num += 1;
                }
                *v = vout;
            }
            if *v != 0 {
                trace!("VERIFIED PACKET!!!!!");
            }
        }
    }
    inc_new_counter_debug!("ed25519_verify_gpu", count);
    recycler_out.recycle(out);
    recycler.recycle(signature_offsets);
    recycler.recycle(pubkey_offsets);
    recycler.recycle(msg_sizes);
    recycler.recycle(msg_start_offsets);
    rvs
}

#[cfg(test)]
pub fn make_packet_from_transaction(tx: Transaction) -> Packet {
    use bincode::serialize;

    let tx_bytes = serialize(&tx).unwrap();
    let mut packet = Packet::default();
    packet.meta.size = tx_bytes.len();
    packet.data[..packet.meta.size].copy_from_slice(&tx_bytes);
    return packet;
}

#[cfg(test)]
mod tests {
    use crate::packet::{Packet, Packets};
    use crate::recycler::Recycler;
    use crate::sigverify;
    use crate::sigverify::PacketOffsets;
    use crate::test_tx::{test_multisig_tx, test_tx};
    use bincode::{deserialize, serialize, serialized_size};
    use solana_sdk::hash::Hash;
    use solana_sdk::message::{Message, MessageHeader};
    use solana_sdk::signature::Signature;
    use solana_sdk::transaction::Transaction;
    use std::mem::size_of;

    const SIG_OFFSET: usize = 1;

    pub fn memfind<A: Eq>(a: &[A], b: &[A]) -> Option<usize> {
        assert!(a.len() >= b.len());
        let end = a.len() - b.len() + 1;
        for i in 0..end {
            if a[i..i + b.len()] == b[..] {
                return Some(i);
            }
        }
        None
    }

    #[test]
    fn test_layout() {
        let tx = test_tx();
        let tx_bytes = serialize(&tx).unwrap();
        let packet = serialize(&tx).unwrap();
        assert_matches!(memfind(&packet, &tx_bytes), Some(0));
        assert_matches!(memfind(&packet, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]), None);
    }

    #[test]
    fn test_system_transaction_layout() {
        let tx = test_tx();
        let tx_bytes = serialize(&tx).unwrap();
        let message_data = tx.message_data();
        let packet = sigverify::make_packet_from_transaction(tx.clone());

        let packet_offsets = sigverify::get_packet_offsets(&packet, 0);

        assert_eq!(
            memfind(&tx_bytes, &tx.signatures[0].as_ref()),
            Some(SIG_OFFSET)
        );
        assert_eq!(
            memfind(&tx_bytes, &tx.message().account_keys[0].as_ref()),
            Some(packet_offsets.pubkey_start as usize)
        );
        assert_eq!(
            memfind(&tx_bytes, &message_data),
            Some(packet_offsets.msg_start as usize)
        );
        assert_eq!(
            memfind(&tx_bytes, &tx.signatures[0].as_ref()),
            Some(packet_offsets.sig_start as usize)
        );
        assert_eq!(packet_offsets.sig_len, 1);
    }

    #[test]
    fn test_untrustworthy_sigs() {
        let required_num_sigs = 14;
        let actual_num_sigs = 5;

        let message = Message {
            header: MessageHeader {
                num_required_signatures: required_num_sigs,
                num_credit_only_signed_accounts: 12,
                num_credit_only_unsigned_accounts: 11,
            },
            account_keys: vec![],
            recent_blockhash: Hash::default(),
            instructions: vec![],
        };
        let mut tx = Transaction::new_unsigned(message);
        tx.signatures = vec![Signature::default(); actual_num_sigs as usize];
        let packet = sigverify::make_packet_from_transaction(tx.clone());

        let unsanitized_packet_offsets = sigverify::do_get_packet_offsets(&packet, 0);

        assert_eq!(unsanitized_packet_offsets.correct, false);
        assert_eq!(
            unsanitized_packet_offsets.packet_offsets.sig_len as usize,
            actual_num_sigs
        );
    }

    #[test]
    fn test_large_sigs() {
        // use any large number to be misinterpreted as 2 bytes when decoded as short_vec
        let required_num_sigs = 214;
        let actual_num_sigs = 5;

        let message = Message {
            header: MessageHeader {
                num_required_signatures: required_num_sigs,
                num_credit_only_signed_accounts: 12,
                num_credit_only_unsigned_accounts: 11,
            },
            account_keys: vec![],
            recent_blockhash: Hash::default(),
            instructions: vec![],
        };
        let mut tx = Transaction::new_unsigned(message);
        // reduce to actual_num_sigs to avoid packet error
        tx.signatures = vec![Signature::default(); actual_num_sigs];
        let packet = sigverify::make_packet_from_transaction(tx.clone());

        let unsanitized_packet_offsets = sigverify::do_get_packet_offsets(&packet, 0);

        let expected_sig_size = 1;
        let expected_sigs_size = actual_num_sigs * size_of::<Signature>();
        let expected_msg_header_size = serialized_size(&MessageHeader::default()).unwrap() as usize;
        let expected_pubkey_size = 1;
        let expected_pubkey_start = expected_sig_size
            + expected_sigs_size
            + expected_msg_header_size
            + expected_pubkey_size;

        assert_eq!(
            expected_pubkey_start,
            unsanitized_packet_offsets.packet_offsets.pubkey_start as usize
        );
    }

    #[test]
    fn test_system_transaction_data_layout() {
        use crate::packet::PACKET_DATA_SIZE;
        let mut tx0 = test_tx();
        tx0.message.instructions[0].data = vec![1, 2, 3];
        let message0a = tx0.message_data();
        let tx_bytes = serialize(&tx0).unwrap();
        assert!(tx_bytes.len() < PACKET_DATA_SIZE);
        assert_eq!(
            memfind(&tx_bytes, &tx0.signatures[0].as_ref()),
            Some(SIG_OFFSET)
        );
        let tx1 = deserialize(&tx_bytes).unwrap();
        assert_eq!(tx0, tx1);
        assert_eq!(tx1.message().instructions[0].data, vec![1, 2, 3]);

        tx0.message.instructions[0].data = vec![1, 2, 4];
        let message0b = tx0.message_data();
        assert_ne!(message0a, message0b);
    }

    // Just like get_packet_offsets, but not returning redundant information.
    fn get_packet_offsets_from_tx(tx: Transaction, current_offset: u32) -> PacketOffsets {
        let packet = sigverify::make_packet_from_transaction(tx);
        let packet_offsets = sigverify::get_packet_offsets(&packet, current_offset);
        PacketOffsets::new(
            packet_offsets.sig_len,
            packet_offsets.sig_start - current_offset,
            packet_offsets.msg_start - packet_offsets.sig_start,
            packet_offsets.pubkey_start - packet_offsets.msg_start,
        )
    }

    #[test]
    fn test_get_packet_offsets() {
        assert_eq!(
            get_packet_offsets_from_tx(test_tx(), 0),
            PacketOffsets::new(1, 1, 64, 4)
        );
        assert_eq!(
            get_packet_offsets_from_tx(test_tx(), 100),
            PacketOffsets::new(1, 1, 64, 4)
        );

        // Ensure we're not indexing packet by the `current_offset` parameter.
        assert_eq!(
            get_packet_offsets_from_tx(test_tx(), 1_000_000),
            PacketOffsets::new(1, 1, 64, 4)
        );

        // Ensure we're returning sig_len, not sig_size.
        assert_eq!(
            get_packet_offsets_from_tx(test_multisig_tx(), 0),
            PacketOffsets::new(2, 1, 128, 4)
        );
    }

    fn generate_packet_vec(
        packet: &Packet,
        num_packets_per_batch: usize,
        num_batches: usize,
    ) -> Vec<Packets> {
        // generate packet vector
        let batches: Vec<_> = (0..num_batches)
            .map(|_| {
                let mut packets = Packets::default();
                packets.packets.resize(0, Packet::default());
                for _ in 0..num_packets_per_batch {
                    packets.packets.push(packet.clone());
                }
                assert_eq!(packets.packets.len(), num_packets_per_batch);
                packets
            })
            .collect();
        assert_eq!(batches.len(), num_batches);

        batches
    }

    fn test_verify_n(n: usize, modify_data: bool) {
        let tx = test_tx();
        let mut packet = sigverify::make_packet_from_transaction(tx);

        // jumble some data to test failure
        if modify_data {
            packet.data[20] = packet.data[20].wrapping_add(10);
        }

        let batches = generate_packet_vec(&packet, n, 2);

        let recycler = Recycler::default();
        let recycler_out = Recycler::default();
        // verify packets
        let ans = sigverify::ed25519_verify(&batches, &recycler, &recycler_out);

        // check result
        let ref_ans = if modify_data { 0u8 } else { 1u8 };
        assert_eq!(ans, vec![vec![ref_ans; n], vec![ref_ans; n]]);
    }

    #[test]
    fn test_verify_tampered_sig_len() {
        let mut tx = test_tx().clone();
        // pretend malicious leader dropped a signature...
        tx.signatures.pop();
        let packet = sigverify::make_packet_from_transaction(tx);

        let batches = generate_packet_vec(&packet, 1, 1);

        let recycler = Recycler::default();
        let recycler_out = Recycler::default();
        // verify packets
        let ans = sigverify::ed25519_verify(&batches, &recycler, &recycler_out);

        assert_eq!(ans, vec![vec![0u8; 1]]);
    }

    #[test]
    fn test_verify_zero() {
        test_verify_n(0, false);
    }

    #[test]
    fn test_verify_one() {
        test_verify_n(1, false);
    }

    #[test]
    fn test_verify_seventy_one() {
        test_verify_n(71, false);
    }

    #[test]
    fn test_verify_multisig() {
        solana_logger::setup();

        let tx = test_multisig_tx();
        let mut packet = sigverify::make_packet_from_transaction(tx);

        let n = 4;
        let num_batches = 3;
        let mut batches = generate_packet_vec(&packet, n, num_batches);

        packet.data[40] = packet.data[40].wrapping_add(8);

        batches[0].packets.push(packet);

        let recycler = Recycler::default();
        let recycler_out = Recycler::default();
        // verify packets
        let ans = sigverify::ed25519_verify(&batches, &recycler, &recycler_out);

        // check result
        let ref_ans = 1u8;
        let mut ref_vec = vec![vec![ref_ans; n]; num_batches];
        ref_vec[0].push(0u8);
        assert_eq!(ans, ref_vec);
    }

    #[test]
    fn test_verify_fail() {
        test_verify_n(5, true);
    }
}
