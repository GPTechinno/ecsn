//! The chain store on NOR flash, in a simulated flash: the regtest chain validated on it and
//! after a reset, header states, reorgs, the journal and the accumulator ring, and power cuts.

mod common;

use bitcoin::{BlockHash, Network};
use common::{fixture, Fixture, Shared};
use ecsn::flash::{Flash, RamFlash};
use ecsn::node::{Config, Node};
use ecsn::store::{FlashChainStore, Layout, MemChainStore, RECORD_SIZE};
use floresta_chain::pruned_utreexo::UpdatableChainstate;
use floresta_chain::{
    AssumeUtreexoValue, BestChain, BlockchainInterface, ChainStore, ChainstoreError,
    DiskBlockHeader,
};
use rustreexo::proof::Proof;

const CAPACITY: u32 = 4 << 20;

fn config() -> Config {
    Config {
        network: Network::Regtest,
        user_agent: String::new(),
        assume_utreexo: None,
        blocks_in_flight: 1,
        ipv6: false,
        v2: true,
        v2_bridge: true,
    }
}

fn mount<F: Flash>(flash: F) -> FlashChainStore<F> {
    let (store, formatted) = FlashChainStore::mount(flash, Network::Regtest, false).unwrap();
    assert!(!formatted);
    store
}

fn formatted(capacity: u32) -> FlashChainStore<RamFlash> {
    let (store, formatted) =
        FlashChainStore::mount(RamFlash::new(capacity), Network::Regtest, true).unwrap();
    assert!(formatted);
    store
}

/// The store after a reset.
fn reset(store: FlashChainStore<RamFlash>) -> FlashChainStore<RamFlash> {
    mount(RamFlash::from_bytes(store.into_flash().bytes().to_vec()))
}

/// Every header of the fixture, and its blocks validated up to 111.
fn validate<S: ChainStore>(fixture: &Fixture, store: S) -> Node<S> {
    let node = Node::new(store, config(), 0).unwrap();
    for block in &fixture.blocks {
        node.chain().accept_header(block.header).unwrap();
    }
    for (i, block) in fixture.blocks[..110].iter().enumerate() {
        node.connect_block(i as u32 + 1, block, &[], Proof::default())
            .unwrap();
    }
    node.connect_block(
        111,
        &fixture.blocks[110],
        &fixture.leaf_data,
        fixture.proof.clone(),
    )
    .unwrap();
    node.chain().flush().unwrap();
    node
}

fn best(hash: BlockHash, depth: u32) -> BestChain {
    BestChain {
        best_block: hash,
        depth,
        validation_index: hash,
        alternative_tips: Vec::new(),
    }
}

#[test]
fn the_board_flash_layout() {
    let capacity = 128 << 20;
    let layout = Layout::new(capacity, 8192).unwrap();
    assert_eq!(layout.slots, 1 << 21);
    assert_eq!(layout.records, 1_296_810);
    assert!(3 * layout.records <= 2 * layout.slots);
    let parts = [
        (0, 8192),
        (layout.journal_at, 2 * layout.journal_bank),
        (layout.roots_at, layout.roots_size),
        (layout.slots_at, 4 * layout.slots),
        (layout.index_at, 4 * layout.records),
        (layout.records_at, RECORD_SIZE * layout.records),
    ];
    for pair in parts.windows(2) {
        assert!(pair[0].0 + pair[0].1 <= pair[1].0);
        assert_eq!(pair[1].0 % 8192, 0);
    }
    assert!(parts[5].0 + parts[5].1 <= capacity);
}

#[test]
fn a_blank_flash_is_formatted_only_when_asked() {
    let blank = RamFlash::new(CAPACITY);
    assert!(FlashChainStore::mount(blank, Network::Regtest, false).is_err());
    let store = formatted(CAPACITY);
    let bytes = store.into_flash().bytes().to_vec();
    // a store of another network is refused, not formatted
    let other = RamFlash::from_bytes(bytes.clone());
    assert!(FlashChainStore::mount(other, Network::Signet, false).is_err());
    mount(RamFlash::from_bytes(bytes));
}

#[test]
fn a_store_of_another_network_is_formatted_for_this_one_when_asked() {
    let bytes = formatted(CAPACITY).into_flash().bytes().to_vec();
    let flash = RamFlash::from_bytes(bytes.clone());
    assert_eq!(
        FlashChainStore::stored_network(&flash).unwrap(),
        Some(Network::Regtest)
    );
    assert_eq!(
        FlashChainStore::stored_network(&RamFlash::new(CAPACITY)).unwrap(),
        None
    );
    let (store, formatted) =
        FlashChainStore::mount(RamFlash::from_bytes(bytes), Network::Signet, true).unwrap();
    assert!(formatted);
    let bytes = store.into_flash().bytes().to_vec();
    let (_, formatted) =
        FlashChainStore::mount(RamFlash::from_bytes(bytes.clone()), Network::Signet, false)
            .unwrap();
    assert!(!formatted);
    assert!(FlashChainStore::mount(RamFlash::from_bytes(bytes), Network::Regtest, false).is_err());
}

#[test]
fn formatting_erases_what_the_flash_held() {
    let fixture = fixture();
    let flash = Shared::new(CAPACITY);
    let mut programmed = flash.clone();
    programmed.write(100_000, &[0x12; 64]).unwrap();
    programmed.write(CAPACITY - 4096, &[0; 4096]).unwrap();
    let (store, formatted) = FlashChainStore::mount(flash, Network::Regtest, true).unwrap();
    assert!(formatted);
    validate(&fixture, store);
}

#[test]
fn the_regtest_chain_validates_as_on_the_ram_store_and_survives_a_reset() {
    let fixture = fixture();
    let flash = Shared::new(CAPACITY);
    let node = validate(
        &fixture,
        FlashChainStore::mount(flash.clone(), Network::Regtest, true)
            .unwrap()
            .0,
    );
    let ram = validate(&fixture, MemChainStore::new());
    let acc = node.chain().acc();
    assert_eq!(acc, ram.chain().acc());
    assert_eq!(acc.leaves, fixture.leaves);

    let again = Node::new(mount(flash.reset()), config(), 0).unwrap();
    let chain = again.chain();
    assert_eq!(
        chain.get_best_block().unwrap(),
        ram.chain().get_best_block().unwrap()
    );
    assert_eq!(chain.get_validation_index().unwrap(), 111);
    assert_eq!(chain.acc(), acc);
    for (i, block) in fixture.blocks.iter().enumerate() {
        let height = i as u32 + 1;
        let hash = block.block_hash();
        assert_eq!(chain.get_block_hash(height).unwrap(), hash);
        assert_eq!(chain.get_block_height(&hash).unwrap(), Some(height));
        assert_eq!(chain.get_block_header(&hash).unwrap(), block.header);
    }
}

#[test]
fn header_states_after_a_reset_follow_the_index_and_the_validated_block() {
    let fixture = fixture();
    let header = |i: usize| fixture.blocks[i - 1].header;
    let hash = |i: usize| fixture.blocks[i - 1].block_hash();
    let mut store = formatted(CAPACITY);
    for i in 1..=20 {
        let state = if i <= 10 {
            DiskBlockHeader::FullyValid(header(i), i as u32)
        } else {
            DiskBlockHeader::HeadersOnly(header(i), i as u32)
        };
        store.save_header(&state).unwrap();
        store.update_block_index(i as u32, hash(i)).unwrap();
    }
    // a header of a fork at 5, and an invalid one stored without a height
    store
        .save_header(&DiskBlockHeader::InFork(header(30), 5))
        .unwrap();
    store
        .save_header(&DiskBlockHeader::InvalidChain(header(40)))
        .unwrap();
    store
        .save_height(&BestChain {
            best_block: hash(20),
            depth: 20,
            validation_index: hash(10),
            alternative_tips: Vec::new(),
        })
        .unwrap();

    let expected = |i: usize| match i {
        1..=10 => DiskBlockHeader::FullyValid(header(i), i as u32),
        11..=20 => DiskBlockHeader::HeadersOnly(header(i), i as u32),
        30 => DiskBlockHeader::InFork(header(30), 5),
        _ => DiskBlockHeader::InvalidChain(header(40)),
    };
    let check = |store: &FlashChainStore<RamFlash>| {
        for i in (1..=20).chain([30, 40]) {
            assert_eq!(
                store.get_header(&hash(i)).unwrap(),
                Some(expected(i)),
                "{i}"
            );
        }
        assert_eq!(store.get_header(&hash(50)).unwrap(), None);
        assert_eq!(store.get_header_by_height(5).unwrap(), Some(expected(5)));
        assert_eq!(store.get_header_by_height(21).unwrap(), None);
    };
    check(&store);
    check(&reset(store));
}

#[test]
fn a_reorg_overrides_the_index_across_resets() {
    let fixture = fixture();
    let hash = |i: usize| fixture.blocks[i - 1].block_hash();
    let mut store = formatted(CAPACITY);
    for i in 1..=10 {
        let header = DiskBlockHeader::HeadersOnly(fixture.blocks[i - 1].header, i as u32);
        store.save_header(&header).unwrap();
        store.update_block_index(i as u32, hash(i)).unwrap();
    }
    let fork = fixture.blocks[49].header;
    store
        .save_header(&DiskBlockHeader::HeadersOnly(fork, 8))
        .unwrap();
    store.update_block_index(8, hash(50)).unwrap();
    assert_eq!(store.get_block_hash(8).unwrap(), Some(hash(50)));
    let mut store = reset(store);
    assert_eq!(store.get_block_hash(8).unwrap(), Some(hash(50)));
    assert_eq!(
        store.get_header_by_height(8).unwrap(),
        Some(DiskBlockHeader::HeadersOnly(fork, 8))
    );
    // the old header at 8 is now off the index
    assert!(matches!(
        store.get_header(&hash(8)).unwrap(),
        Some(DiskBlockHeader::InFork(_, 8))
    ));
    // and back
    store.update_block_index(8, hash(8)).unwrap();
    let store = reset(store);
    assert_eq!(store.get_block_hash(8).unwrap(), Some(hash(8)));
    assert_eq!(store.get_block_hash(9).unwrap(), Some(hash(9)));
}

#[test]
fn a_header_takes_one_record_and_two_when_its_parent_is_not_stored() {
    let fixture = fixture();
    let genesis = bitcoin::constants::genesis_block(Network::Regtest).header;
    let header = |i: usize| fixture.blocks[i - 1].header;
    let mut store = formatted(CAPACITY);
    store
        .save_header(&DiskBlockHeader::FullyValid(genesis, 0))
        .unwrap();
    for i in 1..=3 {
        store
            .save_header(&DiskBlockHeader::HeadersOnly(header(i), i as u32))
            .unwrap();
    }
    assert_eq!(store.records(), 4);
    // the parent of 10 is not stored: the record before 10's holds its hash
    store
        .save_header(&DiskBlockHeader::HeadersOnly(header(10), 10))
        .unwrap();
    assert_eq!(store.records(), 6);
    // nor is that of 9, while 11's is
    for i in [9, 11] {
        store
            .save_header(&DiskBlockHeader::HeadersOnly(header(i), i as u32))
            .unwrap();
    }
    assert_eq!(store.records(), 9);
    let check = |store: &FlashChainStore<RamFlash>| {
        let stored = store.get_header(&genesis.block_hash()).unwrap().unwrap();
        assert_eq!(*stored, genesis);
        for i in [1, 2, 3, 9, 10, 11] {
            let hash = fixture.blocks[i - 1].block_hash();
            let stored = store.get_header(&hash).unwrap().unwrap();
            assert_eq!(*stored, header(i), "{i}");
            assert_eq!(stored.block_hash(), hash, "{i}");
        }
    };
    check(&store);
    check(&reset(store));
}

#[test]
fn the_header_just_read_is_saved_again_with_its_height_checked() {
    let fixture = fixture();
    let header = |i: usize| fixture.blocks[i - 1].header;
    let hash = |i: usize| fixture.blocks[i - 1].block_hash();
    let mut store = formatted(CAPACITY);
    for i in 1..=5 {
        store
            .save_header(&DiskBlockHeader::HeadersOnly(header(i), i as u32))
            .unwrap();
        store.update_block_index(i as u32, hash(i)).unwrap();
    }
    let records = store.records();
    let read = store.get_header(&hash(3)).unwrap().unwrap();
    store
        .save_header(&DiskBlockHeader::FullyValid(*read, 3))
        .unwrap();
    assert_eq!(
        store.get_header(&hash(3)).unwrap(),
        Some(DiskBlockHeader::FullyValid(header(3), 3))
    );
    let read = store.get_header_by_height(4).unwrap().unwrap();
    assert!(store
        .save_header(&DiskBlockHeader::FullyValid(*read, 7))
        .is_err());
    // another header than the one just read
    store
        .save_header(&DiskBlockHeader::FullyValid(header(5), 5))
        .unwrap();
    assert_eq!(
        store.get_header_by_height(5).unwrap(),
        Some(DiskBlockHeader::FullyValid(header(5), 5))
    );
    assert_eq!(store.records(), records);
}

#[test]
fn an_assumed_accumulator_makes_the_chain_below_it_valid_across_a_reset() {
    let fixture = fixture();
    let hash = |i: usize| fixture.blocks[i - 1].block_hash();
    let flash = Shared::new(CAPACITY);
    let store = FlashChainStore::mount(flash.clone(), Network::Regtest, true)
        .unwrap()
        .0;
    let assumed = Config {
        assume_utreexo: Some(AssumeUtreexoValue {
            block_hash: hash(100),
            height: 100,
            roots: Vec::new(),
            leaves: 0,
        }),
        ..config()
    };
    let node = Node::new(store, assumed, 0).unwrap();
    for block in &fixture.blocks {
        node.chain().accept_header(block.header).unwrap();
    }
    assert!(node.assume_utreexo().unwrap());
    assert_eq!(node.chain().get_validation_index().unwrap(), 100);

    let store = mount(flash.reset());
    for (i, block) in fixture.blocks.iter().enumerate() {
        let height = i as u32 + 1;
        let expected = if height <= 100 {
            DiskBlockHeader::FullyValid(block.header, height)
        } else {
            DiskBlockHeader::HeadersOnly(block.header, height)
        };
        assert_eq!(store.get_header(&hash(i + 1)).unwrap(), Some(expected));
    }
    let node = Node::new(store, config(), 0).unwrap();
    assert_eq!(node.chain().get_validation_index().unwrap(), 100);
}

#[test]
fn the_journal_is_compacted_when_its_bank_is_full() {
    let fixture = fixture();
    let hash = |i: usize| fixture.blocks[i - 1].block_hash();
    let mut store = formatted(CAPACITY);
    for i in 1..=30 {
        let header = DiskBlockHeader::HeadersOnly(fixture.blocks[i - 1].header, i as u32);
        store.save_header(&header).unwrap();
        store.update_block_index(i as u32, hash(i)).unwrap();
    }
    // many times a bank (8 KB) of best chains, with an index override kept throughout
    for n in 0..1000u32 {
        store
            .save_height(&best(hash(n as usize % 30 + 1), n))
            .unwrap();
        if n == 10 {
            let fork = DiskBlockHeader::HeadersOnly(fixture.blocks[49].header, 3);
            store.save_header(&fork).unwrap();
            store.update_block_index(3, hash(50)).unwrap();
        }
    }
    let store = reset(store);
    assert_eq!(
        store.load_height().unwrap(),
        Some(best(hash(999 % 30 + 1), 999))
    );
    assert_eq!(store.get_block_hash(3).unwrap(), Some(hash(50)));
    assert_eq!(store.get_block_hash(4).unwrap(), Some(hash(4)));
}

#[test]
fn the_ring_keeps_the_latest_accumulators() {
    let mut store = formatted(CAPACITY);
    let roots = |height: u32| vec![height as u8; 1000];
    for height in 0..100 {
        store.save_roots_for_block(roots(height), height).unwrap();
    }
    // four blocks of 8 KB, eight accumulators of 1 KB each: the last three blocks and more
    assert!((24..=32).contains(&store.roots_kept()));
    assert_eq!(store.load_roots_for_block(99).unwrap(), Some(roots(99)));
    assert_eq!(store.load_roots_for_block(80).unwrap(), Some(roots(80)));
    assert_eq!(store.load_roots_for_block(0).unwrap(), None);
    let kept = store.roots_kept();
    let mut store = reset(store);
    assert_eq!(store.roots_kept(), kept);
    assert_eq!(store.load_roots_for_block(99).unwrap(), Some(roots(99)));
    // a height saved again (after a reorg) gives its latest accumulator
    store.save_roots_for_block(vec![7; 64], 99).unwrap();
    assert_eq!(store.load_roots_for_block(99).unwrap(), Some(vec![7; 64]));
    assert_eq!(
        store
            .save_roots_for_block(vec![0; 3000], 100)
            .unwrap_err()
            .to_string(),
        ChainstoreError::OversizedAccumulator.to_string()
    );
}

/// A best chain with alternative tips: a few of them fill a journal bank.
fn best_with_tips(hash: BlockHash, depth: u32) -> BestChain {
    BestChain {
        alternative_tips: vec![hash; 8],
        ..best(hash, depth)
    }
}

/// Saves headers 1 to `n` of the fixture, each indexed, with an accumulator and a best chain:
/// with 30 headers, the journal is compacted and the ring wraps.
fn grow<F: Flash>(
    store: &mut FlashChainStore<F>,
    fixture: &Fixture,
    n: usize,
) -> Result<(), ChainstoreError> {
    for (i, block) in fixture.blocks[..n].iter().enumerate() {
        let height = i as u32 + 1;
        let hash = block.block_hash();
        store.save_header(&DiskBlockHeader::HeadersOnly(block.header, height))?;
        store.update_block_index(height, hash)?;
        store.save_roots_for_block(vec![height as u8; 1500], height)?;
        store.save_height(&best_with_tips(hash, height))?;
    }
    Ok(())
}

#[test]
fn after_a_power_cut_anywhere_the_store_mounts_and_resumes() {
    let fixture = fixture();
    let n = 30;
    // the smallest flash with a few blocks to each part
    let capacity = 256 << 10;
    let fresh = formatted(capacity).into_flash();
    let total = {
        let mut store = mount(fresh.clone());
        grow(&mut store, &fixture, n).unwrap();
        store.into_flash().programmed() - fresh.programmed()
    };
    for cut in (0..total).step_by(17) {
        let mut flash = fresh.clone();
        flash.cut_power_after(cut);
        let mut store = mount(flash);
        assert!(grow(&mut store, &fixture, n).is_err(), "cut at {cut}");

        let mut store = reset(store);
        for (i, block) in fixture.blocks[..n].iter().enumerate() {
            let hash = block.block_hash();
            if let Some(header) = store.get_header(&hash).unwrap() {
                assert_eq!(header.block_hash(), hash, "cut at {cut}");
                assert_eq!(header.height(), Some(i as u32 + 1), "cut at {cut}");
            }
        }
        grow(&mut store, &fixture, n).unwrap_or_else(|e| panic!("cut at {cut}: {e}"));
        let store = reset(store);
        for (i, block) in fixture.blocks[..n].iter().enumerate() {
            let height = i as u32 + 1;
            assert_eq!(
                store.get_block_hash(height).unwrap(),
                Some(block.block_hash())
            );
        }
        let tip = fixture.blocks[n - 1].block_hash();
        assert_eq!(
            store.load_height().unwrap(),
            Some(best_with_tips(tip, n as u32))
        );
        let mut store = store;
        assert_eq!(
            store.load_roots_for_block(n as u32).unwrap(),
            Some(vec![n as u8; 1500])
        );
        assert_eq!(store.load_roots_for_block(1).unwrap(), None);
    }
}
