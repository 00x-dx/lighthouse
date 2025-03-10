//! Provides `SignedBeaconBlock` verification logic.
//!
//! Specifically, it provides the following:
//!
//! - Verification for gossip blocks (i.e., should we gossip some block from the network).
//! - Verification for normal blocks (e.g., some block received on the RPC during a parent lookup).
//! - Verification for chain segments (e.g., some chain of blocks received on the RPC during a
//!    sync).
//!
//! The primary source of complexity here is that we wish to avoid doing duplicate work as a block
//! moves through the verification process. For example, if some block is verified for gossip, we
//! do not wish to re-verify the block proposal signature or re-hash the block. Or, if we've
//! verified the signatures of a block during a chain segment import, we do not wish to verify each
//! signature individually again.
//!
//! The incremental processing steps (e.g., signatures verified but not the state transition) is
//! represented as a sequence of wrapper-types around the block. There is a linear progression of
//! types, starting at a `SignedBeaconBlock` and finishing with a `Fully VerifiedBlock` (see
//! diagram below).
//!
//! ```ignore
//!           START
//!             |
//!             ▼
//!     SignedBeaconBlock
//!             |---------------
//!             |              |
//!             |              ▼
//!             |      GossipVerifiedBlock
//!             |              |
//!             |---------------
//!             |
//!             ▼
//!    SignatureVerifiedBlock
//!             |
//!             ▼
//!    ExecutionPendingBlock
//!             |
//!           await
//!             |
//!             ▼
//!            END
//!
//! ```

// Ignore this lint for `BlockSlashInfo` which is of comparable size to the non-error types it is
// returned alongside.
#![allow(clippy::result_large_err)]

use crate::eth1_finalization_cache::Eth1FinalizationData;
use crate::execution_payload::{
    is_optimistic_candidate_block, validate_execution_payload_for_gossip, validate_merge_block,
    AllowOptimisticImport, NotifyExecutionLayer, PayloadNotifier,
};
use crate::observed_block_producers::SeenBlock;
use crate::snapshot_cache::PreProcessingSnapshot;
use crate::validator_monitor::HISTORIC_EPOCHS as VALIDATOR_MONITOR_HISTORIC_EPOCHS;
use crate::validator_pubkey_cache::ValidatorPubkeyCache;
use crate::{
    beacon_chain::{
        BeaconForkChoice, ForkChoiceError, BLOCK_PROCESSING_CACHE_LOCK_TIMEOUT,
        VALIDATOR_PUBKEY_CACHE_LOCK_TIMEOUT,
    },
    metrics, BeaconChain, BeaconChainError, BeaconChainTypes,
};
use derivative::Derivative;
use eth2::types::EventKind;
use execution_layer::PayloadStatus;
use fork_choice::{AttestationFromBlock, PayloadVerificationStatus};
use parking_lot::RwLockReadGuard;
use proto_array::Block as ProtoBlock;
use safe_arith::ArithError;
use slog::{debug, error, warn, Logger};
use slot_clock::SlotClock;
use ssz::Encode;
use state_processing::per_block_processing::{errors::IntoWithIndex, is_merge_transition_block};
use state_processing::{
    block_signature_verifier::{BlockSignatureVerifier, Error as BlockSignatureVerifierError},
    per_block_processing, per_slot_processing,
    state_advance::partial_state_advance,
    BlockProcessingError, BlockSignatureStrategy, ConsensusContext, SlotProcessingError,
    StateProcessingStrategy, VerifyBlockRoot,
};
use std::borrow::Cow;
use std::fs;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use store::{Error as DBError, HotStateSummary, KeyValueStore, StoreOp};
use task_executor::JoinHandle;
use tree_hash::TreeHash;
use types::ExecPayload;
use types::{
    BeaconBlockRef, BeaconState, BeaconStateError, BlindedPayload, ChainSpec, CloneConfig, Epoch,
    EthSpec, ExecutionBlockHash, Hash256, InconsistentFork, PublicKey, PublicKeyBytes,
    RelativeEpoch, SignedBeaconBlock, SignedBeaconBlockHeader, Slot,
};

pub const POS_PANDA_BANNER: &str = r#"
    ,,,         ,,,                                               ,,,         ,,,
  ;"   ^;     ;'   ",                                           ;"   ^;     ;'   ",
  ;    s$$$$$$$s     ;                                          ;    s$$$$$$$s     ;
  ,  ss$$$$$$$$$$s  ,'  ooooooooo.    .oooooo.   .oooooo..o     ,  ss$$$$$$$$$$s  ,'
  ;s$$$$$$$$$$$$$$$     `888   `Y88. d8P'  `Y8b d8P'    `Y8     ;s$$$$$$$$$$$$$$$
  $$$$$$$$$$$$$$$$$$     888   .d88'888      888Y88bo.          $$$$$$$$$$$$$$$$$$
 $$$$P""Y$$$Y""W$$$$$    888ooo88P' 888      888 `"Y8888o.     $$$$P""Y$$$Y""W$$$$$
 $$$$  p"LFG"q  $$$$$    888        888      888     `"Y88b    $$$$  p"LFG"q  $$$$$
 $$$$  .$$$$$.  $$$$     888        `88b    d88'oo     .d8P    $$$$  .$$$$$.  $$$$
  $$DcaU$$$$$$$$$$      o888o        `Y8bood8P' 8""88888P'      $$DcaU$$$$$$$$$$
    "Y$$$"*"$$$Y"                                                 "Y$$$"*"$$$Y"
        "$b.$$"                                                       "$b.$$"

       .o.                   .   o8o                         .                 .o8
      .888.                .o8   `"'                       .o8                "888
     .8"888.     .ooooo. .o888oooooo oooo    ooo .oooo.  .o888oo .ooooo.  .oooo888
    .8' `888.   d88' `"Y8  888  `888  `88.  .8' `P  )88b   888  d88' `88bd88' `888
   .88ooo8888.  888        888   888   `88..8'   .oP"888   888  888ooo888888   888
  .8'     `888. 888   .o8  888 . 888    `888'   d8(  888   888 .888    .o888   888
 o88o     o8888o`Y8bod8P'  "888"o888o    `8'    `Y888""8o  "888"`Y8bod8P'`Y8bod88P"

"#;

/// Maximum block slot number. Block with slots bigger than this constant will NOT be processed.
const MAXIMUM_BLOCK_SLOT_NUMBER: u64 = 4_294_967_296; // 2^32

/// If true, everytime a block is processed the pre-state, post-state and block are written to SSZ
/// files in the temp directory.
///
/// Only useful for testing.
const WRITE_BLOCK_PROCESSING_SSZ: bool = cfg!(feature = "write_ssz_files");

/// Returned when a block was not verified. A block is not verified for two reasons:
///
/// - The block is malformed/invalid (indicated by all results other than `BeaconChainError`.
/// - We encountered an error whilst trying to verify the block (a `BeaconChainError`).
#[derive(Debug)]
pub enum BlockError<T: EthSpec> {
    /// The parent block was unknown.
    ///
    /// ## Peer scoring
    ///
    /// It's unclear if this block is valid, but it cannot be processed without already knowing
    /// its parent.
    ParentUnknown(Arc<SignedBeaconBlock<T>>),
    /// The block slot is greater than the present slot.
    ///
    /// ## Peer scoring
    ///
    /// Assuming the local clock is correct, the peer has sent an invalid message.
    FutureSlot {
        present_slot: Slot,
        block_slot: Slot,
    },
    /// The block state_root does not match the generated state.
    ///
    /// ## Peer scoring
    ///
    /// The peer has incompatible state transition logic and is faulty.
    StateRootMismatch { block: Hash256, local: Hash256 },
    /// The block was a genesis block, these blocks cannot be re-imported.
    GenesisBlock,
    /// The slot is finalized, no need to import.
    ///
    /// ## Peer scoring
    ///
    /// It's unclear if this block is valid, but this block is for a finalized slot and is
    /// therefore useless to us.
    WouldRevertFinalizedSlot {
        block_slot: Slot,
        finalized_slot: Slot,
    },
    /// The block conflicts with finalization, no need to propagate.
    ///
    /// ## Peer scoring
    ///
    /// It's unclear if this block is valid, but it conflicts with finality and shouldn't be
    /// imported.
    NotFinalizedDescendant { block_parent_root: Hash256 },
    /// Block is already known, no need to re-import.
    ///
    /// ## Peer scoring
    ///
    /// The block is valid and we have already imported a block with this hash.
    BlockIsAlreadyKnown,
    /// The block slot exceeds the MAXIMUM_BLOCK_SLOT_NUMBER.
    ///
    /// ## Peer scoring
    ///
    /// We set a very, very high maximum slot number and this block exceeds it. There's no good
    /// reason to be sending these blocks, they're from future slots.
    ///
    /// The block is invalid and the peer is faulty.
    BlockSlotLimitReached,
    /// The `BeaconBlock` has a `proposer_index` that does not match the index we computed locally.
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer is faulty.
    IncorrectBlockProposer { block: u64, local_shuffling: u64 },
    /// The proposal signature in invalid.
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer is faulty.
    ProposalSignatureInvalid,
    /// The `block.proposal_index` is not known.
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer is faulty.
    UnknownValidator(u64),
    /// A signature in the block is invalid (exactly which is unknown).
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer is faulty.
    InvalidSignature,
    /// The provided block is from an later slot than its parent.
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer is faulty.
    BlockIsNotLaterThanParent { block_slot: Slot, parent_slot: Slot },
    /// At least one block in the chain segment did not have it's parent root set to the root of
    /// the prior block.
    ///
    /// ## Peer scoring
    ///
    /// The chain of blocks is invalid and the peer is faulty.
    NonLinearParentRoots,
    /// The slots of the blocks in the chain segment were not strictly increasing. I.e., a child
    /// had lower slot than a parent.
    ///
    /// ## Peer scoring
    ///
    /// The chain of blocks is invalid and the peer is faulty.
    NonLinearSlots,
    /// The block failed the specification's `per_block_processing` function, it is invalid.
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer is faulty.
    PerBlockProcessingError(BlockProcessingError),
    /// There was an error whilst processing the block. It is not necessarily invalid.
    ///
    /// ## Peer scoring
    ///
    /// We were unable to process this block due to an internal error. It's unclear if the block is
    /// valid.
    BeaconChainError(BeaconChainError),
    /// There was an error whilst verifying weak subjectivity. This block conflicts with the
    /// configured weak subjectivity checkpoint and was not imported.
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer is faulty.
    WeakSubjectivityConflict,
    /// The block has the wrong structure for the fork at `block.slot`.
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer is faulty.
    InconsistentFork(InconsistentFork),
    /// There was an error while validating the ExecutionPayload
    ///
    /// ## Peer scoring
    ///
    /// See `ExecutionPayloadError` for scoring information
    ExecutionPayloadError(ExecutionPayloadError),
    /// The block references an parent block which has an execution payload which was found to be
    /// invalid.
    ///
    /// ## Peer scoring
    ///
    /// The peer sent us an invalid block, we must penalise harshly.
    /// If it's actually our fault (e.g. our execution node database is corrupt) we have bigger
    /// problems to worry about than losing peers, and we're doing the network a favour by
    /// disconnecting.
    ParentExecutionPayloadInvalid { parent_root: Hash256 },
    /// The block is a slashable equivocation from the proposer.
    ///
    /// ## Peer scoring
    ///
    /// Honest peers shouldn't forward more than 1 equivocating block from the same proposer, so
    /// we penalise them with a mid-tolerance error.
    Slashable,
}

/// Returned when block validation failed due to some issue verifying
/// the execution payload.
#[derive(Debug)]
pub enum ExecutionPayloadError {
    /// There's no eth1 connection (mandatory after merge)
    ///
    /// ## Peer scoring
    ///
    /// As this is our fault, do not penalize the peer
    NoExecutionConnection,
    /// Error occurred during engine_executePayload
    ///
    /// ## Peer scoring
    ///
    /// Some issue with our configuration, do not penalize peer
    RequestFailed(execution_layer::Error),
    /// The execution engine returned INVALID for the payload
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer is faulty
    RejectedByExecutionEngine { status: PayloadStatus },
    /// The execution payload timestamp does not match the slot
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer is faulty
    InvalidPayloadTimestamp { expected: u64, found: u64 },
    /// The execution payload references an execution block that cannot trigger the merge.
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer sent us a block that passes gossip propagation conditions,
    /// but is invalid upon further verification.
    InvalidTerminalPoWBlock { parent_hash: ExecutionBlockHash },
    /// The `TERMINAL_BLOCK_HASH` is set, but the block has not reached the
    /// `TERMINAL_BLOCK_HASH_ACTIVATION_EPOCH`.
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer sent us a block that passes gossip propagation conditions,
    /// but is invalid upon further verification.
    InvalidActivationEpoch {
        activation_epoch: Epoch,
        epoch: Epoch,
    },
    /// The `TERMINAL_BLOCK_HASH` is set, but does not match the value specified by the block.
    ///
    /// ## Peer scoring
    ///
    /// The block is invalid and the peer sent us a block that passes gossip propagation conditions,
    /// but is invalid upon further verification.
    InvalidTerminalBlockHash {
        terminal_block_hash: ExecutionBlockHash,
        payload_parent_hash: ExecutionBlockHash,
    },
    /// The execution node is syncing but we fail the conditions for optimistic sync
    ///
    /// ## Peer scoring
    ///
    /// The peer is not necessarily invalid.
    UnverifiedNonOptimisticCandidate,
}

impl ExecutionPayloadError {
    pub fn penalize_peer(&self) -> bool {
        // This match statement should never have a default case so that we are
        // always forced to consider here whether or not to penalize a peer when
        // we add a new error condition.
        match self {
            // The peer has nothing to do with this error, do not penalize them.
            ExecutionPayloadError::NoExecutionConnection => false,
            // The peer has nothing to do with this error, do not penalize them.
            ExecutionPayloadError::RequestFailed(_) => false,
            // An honest optimistic node may propagate blocks which are rejected by an EE, do not
            // penalize them.
            ExecutionPayloadError::RejectedByExecutionEngine { .. } => false,
            // This is a trivial gossip validation condition, there is no reason for an honest peer
            // to propagate a block with an invalid payload time stamp.
            ExecutionPayloadError::InvalidPayloadTimestamp { .. } => true,
            // An honest optimistic node may propagate blocks with an invalid terminal PoW block, we
            // should not penalized them.
            ExecutionPayloadError::InvalidTerminalPoWBlock { .. } => false,
            // This condition is checked *after* gossip propagation, therefore penalizing gossip
            // peers for this block would be unfair. There may be an argument to penalize RPC
            // blocks, since even an optimistic node shouldn't verify this block. We will remove the
            // penalties for all block imports to keep things simple.
            ExecutionPayloadError::InvalidActivationEpoch { .. } => false,
            // As per `Self::InvalidActivationEpoch`.
            ExecutionPayloadError::InvalidTerminalBlockHash { .. } => false,
            // Do not penalize the peer since it's not their fault that *we're* optimistic.
            ExecutionPayloadError::UnverifiedNonOptimisticCandidate => false,
        }
    }
}

impl From<execution_layer::Error> for ExecutionPayloadError {
    fn from(e: execution_layer::Error) -> Self {
        ExecutionPayloadError::RequestFailed(e)
    }
}

impl<T: EthSpec> From<ExecutionPayloadError> for BlockError<T> {
    fn from(e: ExecutionPayloadError) -> Self {
        BlockError::ExecutionPayloadError(e)
    }
}

impl<T: EthSpec> From<InconsistentFork> for BlockError<T> {
    fn from(e: InconsistentFork) -> Self {
        BlockError::InconsistentFork(e)
    }
}

impl<T: EthSpec> std::fmt::Display for BlockError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockError::ParentUnknown(block) => {
                write!(f, "ParentUnknown(parent_root:{})", block.parent_root())
            }
            other => write!(f, "{:?}", other),
        }
    }
}

impl<T: EthSpec> From<BlockSignatureVerifierError> for BlockError<T> {
    fn from(e: BlockSignatureVerifierError) -> Self {
        match e {
            // Make a special distinction for `IncorrectBlockProposer` since it indicates an
            // invalid block, not an internal error.
            BlockSignatureVerifierError::IncorrectBlockProposer {
                block,
                local_shuffling,
            } => BlockError::IncorrectBlockProposer {
                block,
                local_shuffling,
            },
            e => BlockError::BeaconChainError(BeaconChainError::BlockSignatureVerifierError(e)),
        }
    }
}

impl<T: EthSpec> From<BeaconChainError> for BlockError<T> {
    fn from(e: BeaconChainError) -> Self {
        BlockError::BeaconChainError(e)
    }
}

impl<T: EthSpec> From<BeaconStateError> for BlockError<T> {
    fn from(e: BeaconStateError) -> Self {
        BlockError::BeaconChainError(BeaconChainError::BeaconStateError(e))
    }
}

impl<T: EthSpec> From<SlotProcessingError> for BlockError<T> {
    fn from(e: SlotProcessingError) -> Self {
        BlockError::BeaconChainError(BeaconChainError::SlotProcessingError(e))
    }
}

impl<T: EthSpec> From<DBError> for BlockError<T> {
    fn from(e: DBError) -> Self {
        BlockError::BeaconChainError(BeaconChainError::DBError(e))
    }
}

impl<T: EthSpec> From<ArithError> for BlockError<T> {
    fn from(e: ArithError) -> Self {
        BlockError::BeaconChainError(BeaconChainError::ArithError(e))
    }
}

/// Stores information about verifying a payload against an execution engine.
pub struct PayloadVerificationOutcome {
    pub payload_verification_status: PayloadVerificationStatus,
    pub is_valid_merge_transition_block: bool,
}

/// Information about invalid blocks which might still be slashable despite being invalid.
#[allow(clippy::enum_variant_names)]
pub enum BlockSlashInfo<TErr> {
    /// The block is invalid, but its proposer signature wasn't checked.
    SignatureNotChecked(SignedBeaconBlockHeader, TErr),
    /// The block's proposer signature is invalid, so it will never be slashable.
    SignatureInvalid(TErr),
    /// The signature is valid but the attestation is invalid in some other way.
    SignatureValid(SignedBeaconBlockHeader, TErr),
}

impl<E: EthSpec> BlockSlashInfo<BlockError<E>> {
    pub fn from_early_error(header: SignedBeaconBlockHeader, e: BlockError<E>) -> Self {
        match e {
            BlockError::ProposalSignatureInvalid => BlockSlashInfo::SignatureInvalid(e),
            // `InvalidSignature` could indicate any signature in the block, so we want
            // to recheck the proposer signature alone.
            _ => BlockSlashInfo::SignatureNotChecked(header, e),
        }
    }
}

/// Process invalid blocks to see if they are suitable for the slasher.
///
/// If no slasher is configured, this is a no-op.
fn process_block_slash_info<T: BeaconChainTypes>(
    chain: &BeaconChain<T>,
    slash_info: BlockSlashInfo<BlockError<T::EthSpec>>,
) -> BlockError<T::EthSpec> {
    if let Some(slasher) = chain.slasher.as_ref() {
        let (verified_header, error) = match slash_info {
            BlockSlashInfo::SignatureNotChecked(header, e) => {
                if verify_header_signature(chain, &header).is_ok() {
                    (header, e)
                } else {
                    return e;
                }
            }
            BlockSlashInfo::SignatureInvalid(e) => return e,
            BlockSlashInfo::SignatureValid(header, e) => (header, e),
        };

        slasher.accept_block_header(verified_header);
        error
    } else {
        match slash_info {
            BlockSlashInfo::SignatureNotChecked(_, e)
            | BlockSlashInfo::SignatureInvalid(e)
            | BlockSlashInfo::SignatureValid(_, e) => e,
        }
    }
}

/// Verify all signatures (except deposit signatures) on all blocks in the `chain_segment`. If all
/// signatures are valid, the `chain_segment` is mapped to a `Vec<SignatureVerifiedBlock>` that can
/// later be transformed into a `ExecutionPendingBlock` without re-checking the signatures. If any
/// signature in the block is invalid, an `Err` is returned (it is not possible to known _which_
/// signature was invalid).
///
/// ## Errors
///
/// The given `chain_segment` must contain only blocks from the same epoch, otherwise an error
/// will be returned.
pub fn signature_verify_chain_segment<T: BeaconChainTypes>(
    mut chain_segment: Vec<(Hash256, Arc<SignedBeaconBlock<T::EthSpec>>)>,
    chain: &BeaconChain<T>,
) -> Result<Vec<SignatureVerifiedBlock<T>>, BlockError<T::EthSpec>> {
    if chain_segment.is_empty() {
        return Ok(vec![]);
    }

    let (first_root, first_block) = chain_segment.remove(0);
    let (mut parent, first_block) = load_parent(first_root, first_block, chain)?;
    let slot = first_block.slot();
    chain_segment.insert(0, (first_root, first_block));

    let highest_slot = chain_segment
        .last()
        .map(|(_, block)| block.slot())
        .unwrap_or_else(|| slot);

    let state = cheap_state_advance_to_obtain_committees(
        &mut parent.pre_state,
        parent.beacon_state_root,
        highest_slot,
        &chain.spec,
    )?;

    let pubkey_cache = get_validator_pubkey_cache(chain)?;
    let mut signature_verifier = get_signature_verifier(&state, &pubkey_cache, &chain.spec);

    let mut signature_verified_blocks = Vec::with_capacity(chain_segment.len());

    for (block_root, block) in &chain_segment {
        let mut consensus_context =
            ConsensusContext::new(block.slot()).set_current_block_root(*block_root);

        signature_verifier.include_all_signatures(block, &mut consensus_context)?;

        // Save the block and its consensus context. The context will have had its proposer index
        // and attesting indices filled in, which can be used to accelerate later block processing.
        signature_verified_blocks.push(SignatureVerifiedBlock {
            block: block.clone(),
            block_root: *block_root,
            parent: None,
            consensus_context,
        });
    }

    if signature_verifier.verify().is_err() {
        return Err(BlockError::InvalidSignature);
    }

    drop(pubkey_cache);

    if let Some(signature_verified_block) = signature_verified_blocks.first_mut() {
        signature_verified_block.parent = Some(parent);
    }

    Ok(signature_verified_blocks)
}

/// A wrapper around a `SignedBeaconBlock` that indicates it has been approved for re-gossiping on
/// the p2p network.
#[derive(Derivative)]
#[derivative(Debug(bound = "T: BeaconChainTypes"))]
pub struct GossipVerifiedBlock<T: BeaconChainTypes> {
    pub block: Arc<SignedBeaconBlock<T::EthSpec>>,
    pub block_root: Hash256,
    parent: Option<PreProcessingSnapshot<T::EthSpec>>,
    consensus_context: ConsensusContext<T::EthSpec>,
}

/// A wrapper around a `SignedBeaconBlock` that indicates that all signatures (except the deposit
/// signatures) have been verified.
pub struct SignatureVerifiedBlock<T: BeaconChainTypes> {
    block: Arc<SignedBeaconBlock<T::EthSpec>>,
    block_root: Hash256,
    parent: Option<PreProcessingSnapshot<T::EthSpec>>,
    consensus_context: ConsensusContext<T::EthSpec>,
}

/// Used to await the result of executing payload with a remote EE.
type PayloadVerificationHandle<E> =
    JoinHandle<Option<Result<PayloadVerificationOutcome, BlockError<E>>>>;

/// A wrapper around a `SignedBeaconBlock` that indicates that this block is fully verified and
/// ready to import into the `BeaconChain`. The validation includes:
///
/// - Parent is known
/// - Signatures
/// - State root check
/// - Per block processing
///
/// Note: a `ExecutionPendingBlock` is not _forever_ valid to be imported, it may later become invalid
/// due to finality or some other event. A `ExecutionPendingBlock` should be imported into the
/// `BeaconChain` immediately after it is instantiated.
pub struct ExecutionPendingBlock<T: BeaconChainTypes> {
    pub block: Arc<SignedBeaconBlock<T::EthSpec>>,
    pub block_root: Hash256,
    pub state: BeaconState<T::EthSpec>,
    pub parent_block: SignedBeaconBlock<T::EthSpec, BlindedPayload<T::EthSpec>>,
    pub parent_eth1_finalization_data: Eth1FinalizationData,
    pub confirmed_state_roots: Vec<Hash256>,
    pub consensus_context: ConsensusContext<T::EthSpec>,
    pub payload_verification_handle: PayloadVerificationHandle<T::EthSpec>,
}

pub trait IntoGossipVerifiedBlock<T: BeaconChainTypes>: Sized {
    fn into_gossip_verified_block(
        self,
        chain: &BeaconChain<T>,
    ) -> Result<GossipVerifiedBlock<T>, BlockError<T::EthSpec>>;
    fn inner(&self) -> Arc<SignedBeaconBlock<T::EthSpec>>;
}

impl<T: BeaconChainTypes> IntoGossipVerifiedBlock<T> for GossipVerifiedBlock<T> {
    fn into_gossip_verified_block(
        self,
        _chain: &BeaconChain<T>,
    ) -> Result<GossipVerifiedBlock<T>, BlockError<T::EthSpec>> {
        Ok(self)
    }

    fn inner(&self) -> Arc<SignedBeaconBlock<T::EthSpec>> {
        self.block.clone()
    }
}

impl<T: BeaconChainTypes> IntoGossipVerifiedBlock<T> for Arc<SignedBeaconBlock<T::EthSpec>> {
    fn into_gossip_verified_block(
        self,
        chain: &BeaconChain<T>,
    ) -> Result<GossipVerifiedBlock<T>, BlockError<T::EthSpec>> {
        GossipVerifiedBlock::new(self, chain)
    }

    fn inner(&self) -> Arc<SignedBeaconBlock<T::EthSpec>> {
        self.clone()
    }
}

/// Implemented on types that can be converted into a `ExecutionPendingBlock`.
///
/// Used to allow functions to accept blocks at various stages of verification.
pub trait IntoExecutionPendingBlock<T: BeaconChainTypes>: Sized {
    fn into_execution_pending_block(
        self,
        block_root: Hash256,
        chain: &Arc<BeaconChain<T>>,
        notify_execution_layer: NotifyExecutionLayer,
    ) -> Result<ExecutionPendingBlock<T>, BlockError<T::EthSpec>> {
        self.into_execution_pending_block_slashable(block_root, chain, notify_execution_layer)
            .map(|execution_pending| {
                // Supply valid block to slasher.
                if let Some(slasher) = chain.slasher.as_ref() {
                    slasher.accept_block_header(execution_pending.block.signed_block_header());
                }
                execution_pending
            })
            .map_err(|slash_info| process_block_slash_info(chain, slash_info))
    }

    /// Convert the block to fully-verified form while producing data to aid checking slashability.
    fn into_execution_pending_block_slashable(
        self,
        block_root: Hash256,
        chain: &Arc<BeaconChain<T>>,
        notify_execution_layer: NotifyExecutionLayer,
    ) -> Result<ExecutionPendingBlock<T>, BlockSlashInfo<BlockError<T::EthSpec>>>;

    fn block(&self) -> &SignedBeaconBlock<T::EthSpec>;
}

impl<T: BeaconChainTypes> GossipVerifiedBlock<T> {
    /// Instantiates `Self`, a wrapper that indicates the given `block` is safe to be re-gossiped
    /// on the p2p network.
    ///
    /// Returns an error if the block is invalid, or if the block was unable to be verified.
    pub fn new(
        block: Arc<SignedBeaconBlock<T::EthSpec>>,
        chain: &BeaconChain<T>,
    ) -> Result<Self, BlockError<T::EthSpec>> {
        // If the block is valid for gossip we don't supply it to the slasher here because
        // we assume it will be transformed into a fully verified block. We *do* need to supply
        // it to the slasher if an error occurs, because that's the end of this block's journey,
        // and it could be a repeat proposal (a likely cause for slashing!).
        let header = block.signed_block_header();
        Self::new_without_slasher_checks(block, chain).map_err(|e| {
            process_block_slash_info(chain, BlockSlashInfo::from_early_error(header, e))
        })
    }

    /// As for new, but doesn't pass the block to the slasher.
    fn new_without_slasher_checks(
        block: Arc<SignedBeaconBlock<T::EthSpec>>,
        chain: &BeaconChain<T>,
    ) -> Result<Self, BlockError<T::EthSpec>> {
        // Ensure the block is the correct structure for the fork at `block.slot()`.
        block
            .fork_name(&chain.spec)
            .map_err(BlockError::InconsistentFork)?;

        // Do not gossip or process blocks from future slots.
        let present_slot_with_tolerance = chain
            .slot_clock
            .now_with_future_tolerance(chain.spec.maximum_gossip_clock_disparity())
            .ok_or(BeaconChainError::UnableToReadSlot)?;
        if block.slot() > present_slot_with_tolerance {
            return Err(BlockError::FutureSlot {
                present_slot: present_slot_with_tolerance,
                block_slot: block.slot(),
            });
        }

        let block_root = get_block_root(&block);

        // Disallow blocks that conflict with the anchor (weak subjectivity checkpoint), if any.
        check_block_against_anchor_slot(block.message(), chain)?;

        // Do not gossip a block from a finalized slot.
        check_block_against_finalized_slot(block.message(), block_root, chain)?;

        // Check if the block is already known. We know it is post-finalization, so it is
        // sufficient to check the fork choice.
        //
        // In normal operation this isn't necessary, however it is useful immediately after a
        // reboot if the `observed_block_producers` cache is empty. In that case, without this
        // check, we will load the parent and state from disk only to find out later that we
        // already know this block.
        let fork_choice_read_lock = chain.canonical_head.fork_choice_read_lock();
        if fork_choice_read_lock.contains_block(&block_root) {
            return Err(BlockError::BlockIsAlreadyKnown);
        }

        // Do not process a block that doesn't descend from the finalized root.
        //
        // We check this *before* we load the parent so that we can return a more detailed error.
        check_block_is_finalized_checkpoint_or_descendant(chain, &fork_choice_read_lock, &block)?;
        drop(fork_choice_read_lock);

        let block_epoch = block.slot().epoch(T::EthSpec::slots_per_epoch());
        let (parent_block, block) = verify_parent_block_is_known(chain, block)?;

        // Track the number of skip slots between the block and its parent.
        metrics::set_gauge(
            &metrics::GOSSIP_BEACON_BLOCK_SKIPPED_SLOTS,
            block
                .slot()
                .as_u64()
                .saturating_sub(1)
                .saturating_sub(parent_block.slot.into()) as i64,
        );

        // Paranoid check to prevent propagation of blocks that don't form a legitimate chain.
        //
        // This is not in the spec, but @protolambda tells me that the majority of other clients are
        // already doing it. For reference:
        //
        // https://github.com/ethereum/eth2.0-specs/pull/2196
        if parent_block.slot >= block.slot() {
            return Err(BlockError::BlockIsNotLaterThanParent {
                block_slot: block.slot(),
                parent_slot: parent_block.slot,
            });
        }

        let proposer_shuffling_decision_block =
            if parent_block.slot.epoch(T::EthSpec::slots_per_epoch()) == block_epoch {
                parent_block
                    .next_epoch_shuffling_id
                    .shuffling_decision_block
            } else {
                parent_block.root
            };

        // We assign to a variable instead of using `if let Some` directly to ensure we drop the
        // write lock before trying to acquire it again in the `else` clause.
        let proposer_opt = chain
            .beacon_proposer_cache
            .lock()
            .get_slot::<T::EthSpec>(proposer_shuffling_decision_block, block.slot());
        let (expected_proposer, fork, parent, block) = if let Some(proposer) = proposer_opt {
            // The proposer index was cached and we can return it without needing to load the
            // parent.
            (proposer.index, proposer.fork, None, block)
        } else {
            // The proposer index was *not* cached and we must load the parent in order to determine
            // the proposer index.
            let (mut parent, block) = load_parent(block_root, block, chain)?;

            debug!(
                chain.log,
                "Proposer shuffling cache miss";
                "parent_root" => ?parent.beacon_block_root,
                "parent_slot" => parent.beacon_block.slot(),
                "block_root" => ?block_root,
                "block_slot" => block.slot(),
            );

            // The state produced is only valid for determining proposer/attester shuffling indices.
            let state = cheap_state_advance_to_obtain_committees(
                &mut parent.pre_state,
                parent.beacon_state_root,
                block.slot(),
                &chain.spec,
            )?;

            let proposers = state.get_beacon_proposer_indices(&chain.spec)?;
            let proposer_index = *proposers
                .get(block.slot().as_usize() % T::EthSpec::slots_per_epoch() as usize)
                .ok_or_else(|| BeaconChainError::NoProposerForSlot(block.slot()))?;

            // Prime the proposer shuffling cache with the newly-learned value.
            chain.beacon_proposer_cache.lock().insert(
                block_epoch,
                proposer_shuffling_decision_block,
                proposers,
                state.fork(),
            )?;

            (proposer_index, state.fork(), Some(parent), block)
        };

        let signature_is_valid = {
            let pubkey_cache = get_validator_pubkey_cache(chain)?;
            let pubkey = pubkey_cache
                .get(block.message().proposer_index() as usize)
                .ok_or_else(|| BlockError::UnknownValidator(block.message().proposer_index()))?;
            block.verify_signature(
                Some(block_root),
                pubkey,
                &fork,
                chain.genesis_validators_root,
                &chain.spec,
            )
        };

        if !signature_is_valid {
            return Err(BlockError::ProposalSignatureInvalid);
        }

        // Now the signature is valid, store the proposal so we don't accept another from this
        // validator and slot.
        //
        // It's important to double-check that the proposer still hasn't been observed so we don't
        // have a race-condition when verifying two blocks simultaneously.
        match chain
            .observed_block_producers
            .write()
            .observe_proposal(block_root, block.message())
            .map_err(|e| BlockError::BeaconChainError(e.into()))?
        {
            SeenBlock::Slashable => return Err(BlockError::Slashable),
            SeenBlock::Duplicate => return Err(BlockError::BlockIsAlreadyKnown),
            SeenBlock::UniqueNonSlashable => {}
        };

        if block.message().proposer_index() != expected_proposer as u64 {
            return Err(BlockError::IncorrectBlockProposer {
                block: block.message().proposer_index(),
                local_shuffling: expected_proposer as u64,
            });
        }

        // Validate the block's execution_payload (if any).
        validate_execution_payload_for_gossip(&parent_block, block.message(), chain)?;

        // Having checked the proposer index and the block root we can cache them.
        let consensus_context = ConsensusContext::new(block.slot())
            .set_current_block_root(block_root)
            .set_proposer_index(block.message().proposer_index());

        Ok(Self {
            block,
            block_root,
            parent,
            consensus_context,
        })
    }

    pub fn block_root(&self) -> Hash256 {
        self.block_root
    }
}

impl<T: BeaconChainTypes> IntoExecutionPendingBlock<T> for GossipVerifiedBlock<T> {
    /// Completes verification of the wrapped `block`.
    fn into_execution_pending_block_slashable(
        self,
        block_root: Hash256,
        chain: &Arc<BeaconChain<T>>,
        notify_execution_layer: NotifyExecutionLayer,
    ) -> Result<ExecutionPendingBlock<T>, BlockSlashInfo<BlockError<T::EthSpec>>> {
        let execution_pending =
            SignatureVerifiedBlock::from_gossip_verified_block_check_slashable(self, chain)?;
        execution_pending.into_execution_pending_block_slashable(
            block_root,
            chain,
            notify_execution_layer,
        )
    }

    fn block(&self) -> &SignedBeaconBlock<T::EthSpec> {
        &self.block
    }
}

impl<T: BeaconChainTypes> SignatureVerifiedBlock<T> {
    /// Instantiates `Self`, a wrapper that indicates that all signatures (except the deposit
    /// signatures) are valid  (i.e., signed by the correct public keys).
    ///
    /// Returns an error if the block is invalid, or if the block was unable to be verified.
    pub fn new(
        block: Arc<SignedBeaconBlock<T::EthSpec>>,
        block_root: Hash256,
        chain: &BeaconChain<T>,
    ) -> Result<Self, BlockError<T::EthSpec>> {
        // Ensure the block is the correct structure for the fork at `block.slot()`.
        block
            .fork_name(&chain.spec)
            .map_err(BlockError::InconsistentFork)?;

        // Check the anchor slot before loading the parent, to avoid spurious lookups.
        check_block_against_anchor_slot(block.message(), chain)?;

        let (mut parent, block) = load_parent(block_root, block, chain)?;

        let state = cheap_state_advance_to_obtain_committees(
            &mut parent.pre_state,
            parent.beacon_state_root,
            block.slot(),
            &chain.spec,
        )?;

        let pubkey_cache = get_validator_pubkey_cache(chain)?;

        let mut signature_verifier = get_signature_verifier(&state, &pubkey_cache, &chain.spec);

        let mut consensus_context =
            ConsensusContext::new(block.slot()).set_current_block_root(block_root);

        signature_verifier.include_all_signatures(&block, &mut consensus_context)?;

        if signature_verifier.verify().is_ok() {
            Ok(Self {
                consensus_context,
                block,
                block_root,
                parent: Some(parent),
            })
        } else {
            Err(BlockError::InvalidSignature)
        }
    }

    /// As for `new` above but producing `BlockSlashInfo`.
    pub fn check_slashable(
        block: Arc<SignedBeaconBlock<T::EthSpec>>,
        block_root: Hash256,
        chain: &BeaconChain<T>,
    ) -> Result<Self, BlockSlashInfo<BlockError<T::EthSpec>>> {
        let header = block.signed_block_header();
        Self::new(block, block_root, chain).map_err(|e| BlockSlashInfo::from_early_error(header, e))
    }

    /// Finishes signature verification on the provided `GossipVerifedBlock`. Does not re-verify
    /// the proposer signature.
    pub fn from_gossip_verified_block(
        from: GossipVerifiedBlock<T>,
        chain: &BeaconChain<T>,
    ) -> Result<Self, BlockError<T::EthSpec>> {
        let (mut parent, block) = if let Some(parent) = from.parent {
            (parent, from.block)
        } else {
            load_parent(from.block_root, from.block, chain)?
        };

        let state = cheap_state_advance_to_obtain_committees(
            &mut parent.pre_state,
            parent.beacon_state_root,
            block.slot(),
            &chain.spec,
        )?;

        let pubkey_cache = get_validator_pubkey_cache(chain)?;

        let mut signature_verifier = get_signature_verifier(&state, &pubkey_cache, &chain.spec);

        // Gossip verification has already checked the proposer index. Use it to check the RANDAO
        // signature.
        let mut consensus_context = from.consensus_context;
        signature_verifier
            .include_all_signatures_except_proposal(&block, &mut consensus_context)?;

        if signature_verifier.verify().is_ok() {
            Ok(Self {
                block,
                block_root: from.block_root,
                parent: Some(parent),
                consensus_context,
            })
        } else {
            Err(BlockError::InvalidSignature)
        }
    }

    /// Same as `from_gossip_verified_block` but producing slashing-relevant data as well.
    pub fn from_gossip_verified_block_check_slashable(
        from: GossipVerifiedBlock<T>,
        chain: &BeaconChain<T>,
    ) -> Result<Self, BlockSlashInfo<BlockError<T::EthSpec>>> {
        let header = from.block.signed_block_header();
        Self::from_gossip_verified_block(from, chain)
            .map_err(|e| BlockSlashInfo::from_early_error(header, e))
    }

    pub fn block_root(&self) -> Hash256 {
        self.block_root
    }
}

impl<T: BeaconChainTypes> IntoExecutionPendingBlock<T> for SignatureVerifiedBlock<T> {
    /// Completes verification of the wrapped `block`.
    fn into_execution_pending_block_slashable(
        self,
        block_root: Hash256,
        chain: &Arc<BeaconChain<T>>,
        notify_execution_layer: NotifyExecutionLayer,
    ) -> Result<ExecutionPendingBlock<T>, BlockSlashInfo<BlockError<T::EthSpec>>> {
        let header = self.block.signed_block_header();
        let (parent, block) = if let Some(parent) = self.parent {
            (parent, self.block)
        } else {
            load_parent(self.block_root, self.block, chain)
                .map_err(|e| BlockSlashInfo::SignatureValid(header.clone(), e))?
        };

        ExecutionPendingBlock::from_signature_verified_components(
            block,
            block_root,
            parent,
            self.consensus_context,
            chain,
            notify_execution_layer,
        )
        .map_err(|e| BlockSlashInfo::SignatureValid(header, e))
    }

    fn block(&self) -> &SignedBeaconBlock<T::EthSpec> {
        &self.block
    }
}

impl<T: BeaconChainTypes> IntoExecutionPendingBlock<T> for Arc<SignedBeaconBlock<T::EthSpec>> {
    /// Verifies the `SignedBeaconBlock` by first transforming it into a `SignatureVerifiedBlock`
    /// and then using that implementation of `IntoExecutionPendingBlock` to complete verification.
    fn into_execution_pending_block_slashable(
        self,
        block_root: Hash256,
        chain: &Arc<BeaconChain<T>>,
        notify_execution_layer: NotifyExecutionLayer,
    ) -> Result<ExecutionPendingBlock<T>, BlockSlashInfo<BlockError<T::EthSpec>>> {
        // Perform an early check to prevent wasting time on irrelevant blocks.
        let block_root = check_block_relevancy(&self, block_root, chain)
            .map_err(|e| BlockSlashInfo::SignatureNotChecked(self.signed_block_header(), e))?;

        SignatureVerifiedBlock::check_slashable(self, block_root, chain)?
            .into_execution_pending_block_slashable(block_root, chain, notify_execution_layer)
    }

    fn block(&self) -> &SignedBeaconBlock<T::EthSpec> {
        self
    }
}

impl<T: BeaconChainTypes> ExecutionPendingBlock<T> {
    /// Instantiates `Self`, a wrapper that indicates that the given `block` is fully valid. See
    /// the struct-level documentation for more information.
    ///
    /// Note: this function does not verify block signatures, it assumes they are valid. Signature
    /// verification must be done upstream (e.g., via a `SignatureVerifiedBlock`
    ///
    /// Returns an error if the block is invalid, or if the block was unable to be verified.
    pub fn from_signature_verified_components(
        block: Arc<SignedBeaconBlock<T::EthSpec>>,
        block_root: Hash256,
        parent: PreProcessingSnapshot<T::EthSpec>,
        mut consensus_context: ConsensusContext<T::EthSpec>,
        chain: &Arc<BeaconChain<T>>,
        notify_execution_layer: NotifyExecutionLayer,
    ) -> Result<Self, BlockError<T::EthSpec>> {
        chain
            .observed_block_producers
            .write()
            .observe_proposal(block_root, block.message())
            .map_err(|e| BlockError::BeaconChainError(e.into()))?;

        if let Some(parent) = chain
            .canonical_head
            .fork_choice_read_lock()
            .get_block(&block.parent_root())
        {
            // Reject any block where the parent has an invalid payload. It's impossible for a valid
            // block to descend from an invalid parent.
            if parent.execution_status.is_invalid() {
                return Err(BlockError::ParentExecutionPayloadInvalid {
                    parent_root: block.parent_root(),
                });
            }
        } else {
            // Reject any block if its parent is not known to fork choice.
            //
            // A block that is not in fork choice is either:
            //
            //  - Not yet imported: we should reject this block because we should only import a child
            //  after its parent has been fully imported.
            //  - Pre-finalized: if the parent block is _prior_ to finalization, we should ignore it
            //  because it will revert finalization. Note that the finalized block is stored in fork
            //  choice, so we will not reject any child of the finalized block (this is relevant during
            //  genesis).
            return Err(BlockError::ParentUnknown(block));
        }

        /*
         *  Perform cursory checks to see if the block is even worth processing.
         */

        check_block_relevancy(&block, block_root, chain)?;

        // Define a future that will verify the execution payload with an execution engine.
        //
        // We do this as early as possible so that later parts of this function can run in parallel
        // with the payload verification.
        let payload_notifier = PayloadNotifier::new(
            chain.clone(),
            block.clone(),
            &parent.pre_state,
            notify_execution_layer,
        )?;
        let is_valid_merge_transition_block =
            is_merge_transition_block(&parent.pre_state, block.message().body());
        let payload_verification_future = async move {
            let chain = payload_notifier.chain.clone();
            let block = payload_notifier.block.clone();

            // If this block triggers the merge, check to ensure that it references valid execution
            // blocks.
            //
            // The specification defines this check inside `on_block` in the fork-choice specification,
            // however we perform the check here for two reasons:
            //
            // - There's no point in importing a block that will fail fork choice, so it's best to fail
            //   early.
            // - Doing the check here means we can keep our fork-choice implementation "pure". I.e., no
            //   calls to remote servers.
            if is_valid_merge_transition_block {
                validate_merge_block(&chain, block.message(), AllowOptimisticImport::Yes).await?;
            };

            // The specification declares that this should be run *inside* `per_block_processing`,
            // however we run it here to keep `per_block_processing` pure (i.e., no calls to external
            // servers).
            let payload_verification_status = payload_notifier.notify_new_payload().await?;

            // If the payload did not validate or invalidate the block, check to see if this block is
            // valid for optimistic import.
            if payload_verification_status.is_optimistic() {
                let block_hash_opt = block
                    .message()
                    .body()
                    .execution_payload()
                    .map(|full_payload| full_payload.block_hash());

                // Ensure the block is a candidate for optimistic import.
                if !is_optimistic_candidate_block(&chain, block.slot(), block.parent_root()).await?
                {
                    warn!(
                        chain.log,
                        "Rejecting optimistic block";
                        "block_hash" => ?block_hash_opt,
                        "msg" => "the execution engine is not synced"
                    );
                    return Err(ExecutionPayloadError::UnverifiedNonOptimisticCandidate.into());
                }
            }

            Ok(PayloadVerificationOutcome {
                payload_verification_status,
                is_valid_merge_transition_block,
            })
        };
        // Spawn the payload verification future as a new task, but don't wait for it to complete.
        // The `payload_verification_future` will be awaited later to ensure verification completed
        // successfully.
        let payload_verification_handle = chain
            .task_executor
            .spawn_handle(
                payload_verification_future,
                "execution_payload_verification",
            )
            .ok_or(BeaconChainError::RuntimeShutdown)?;

        /*
         * Advance the given `parent.beacon_state` to the slot of the given `block`.
         */

        let catchup_timer = metrics::start_timer(&metrics::BLOCK_PROCESSING_CATCHUP_STATE);

        // Stage a batch of operations to be completed atomically if this block is imported
        // successfully.
        let mut confirmed_state_roots = vec![];

        // The block must have a higher slot than its parent.
        if block.slot() <= parent.beacon_block.slot() {
            return Err(BlockError::BlockIsNotLaterThanParent {
                block_slot: block.slot(),
                parent_slot: parent.beacon_block.slot(),
            });
        }

        let mut summaries = vec![];

        // Transition the parent state to the block slot.
        //
        // It is important to note that we're using a "pre-state" here, one that has potentially
        // been advanced one slot forward from `parent.beacon_block.slot`.
        let mut state = parent.pre_state;

        // Perform a sanity check on the pre-state.
        let parent_slot = parent.beacon_block.slot();
        if state.slot() < parent_slot || state.slot() > block.slot() {
            return Err(BeaconChainError::BadPreState {
                parent_root: parent.beacon_block_root,
                parent_slot,
                block_root,
                block_slot: block.slot(),
                state_slot: state.slot(),
            }
            .into());
        }

        let parent_eth1_finalization_data = Eth1FinalizationData {
            eth1_data: state.eth1_data().clone(),
            eth1_deposit_index: state.eth1_deposit_index(),
        };

        let distance = block.slot().as_u64().saturating_sub(state.slot().as_u64());
        for _ in 0..distance {
            let state_root = if parent.beacon_block.slot() == state.slot() {
                // If it happens that `pre_state` has *not* already been advanced forward a single
                // slot, then there is no need to compute the state root for this
                // `per_slot_processing` call since that state root is already stored in the parent
                // block.
                parent.beacon_block.state_root()
            } else {
                // This is a new state we've reached, so stage it for storage in the DB.
                // Computing the state root here is time-equivalent to computing it during slot
                // processing, but we get early access to it.
                let state_root = state.update_tree_hash_cache()?;

                // Store the state immediately, marking it as temporary, and staging the deletion
                // of its temporary status as part of the larger atomic operation.
                let txn_lock = chain.store.hot_db.begin_rw_transaction();
                let state_already_exists =
                    chain.store.load_hot_state_summary(&state_root)?.is_some();

                let state_batch = if state_already_exists {
                    // If the state exists, it could be temporary or permanent, but in neither case
                    // should we rewrite it or store a new temporary flag for it. We *will* stage
                    // the temporary flag for deletion because it's OK to double-delete the flag,
                    // and we don't mind if another thread gets there first.
                    vec![]
                } else {
                    vec![
                        if state.slot() % T::EthSpec::slots_per_epoch() == 0 {
                            StoreOp::PutState(state_root, &state)
                        } else {
                            StoreOp::PutStateSummary(
                                state_root,
                                HotStateSummary::new(&state_root, &state)?,
                            )
                        },
                        StoreOp::PutStateTemporaryFlag(state_root),
                    ]
                };
                chain.store.do_atomically(state_batch)?;
                drop(txn_lock);

                confirmed_state_roots.push(state_root);

                state_root
            };

            if let Some(summary) = per_slot_processing(&mut state, Some(state_root), &chain.spec)? {
                // Expose Prometheus metrics.
                if let Err(e) = summary.observe_metrics() {
                    error!(
                        chain.log,
                        "Failed to observe epoch summary metrics";
                        "src" => "block_verification",
                        "error" => ?e
                    );
                }
                summaries.push(summary);
            }
        }
        metrics::stop_timer(catchup_timer);

        let block_slot = block.slot();
        let state_current_epoch = state.current_epoch();

        // If the block is sufficiently recent, notify the validator monitor.
        if let Some(slot) = chain.slot_clock.now() {
            let epoch = slot.epoch(T::EthSpec::slots_per_epoch());
            if block_slot.epoch(T::EthSpec::slots_per_epoch())
                + VALIDATOR_MONITOR_HISTORIC_EPOCHS as u64
                >= epoch
            {
                let validator_monitor = chain.validator_monitor.read();
                // Update the summaries in a separate loop to `per_slot_processing`. This protects
                // the `validator_monitor` lock from being bounced or held for a long time whilst
                // performing `per_slot_processing`.
                for (i, summary) in summaries.iter().enumerate() {
                    let epoch = state_current_epoch - Epoch::from(summaries.len() - i);
                    if let Err(e) =
                        validator_monitor.process_validator_statuses(epoch, summary, &chain.spec)
                    {
                        error!(
                            chain.log,
                            "Failed to process validator statuses";
                            "error" => ?e
                        );
                    }
                }
            }
        }

        /*
         * Build the committee caches on the state.
         */

        let committee_timer = metrics::start_timer(&metrics::BLOCK_PROCESSING_COMMITTEE);

        state.build_committee_cache(RelativeEpoch::Previous, &chain.spec)?;
        state.build_committee_cache(RelativeEpoch::Current, &chain.spec)?;

        metrics::stop_timer(committee_timer);

        /*
         * If we have block reward listeners, compute the block reward and push it to the
         * event handler.
         */
        if let Some(ref event_handler) = chain.event_handler {
            if event_handler.has_block_reward_subscribers() {
                let mut reward_cache = Default::default();
                let block_reward = chain.compute_block_reward(
                    block.message(),
                    block_root,
                    &state,
                    &mut reward_cache,
                    true,
                )?;
                event_handler.register(EventKind::BlockReward(block_reward));
            }
        }

        /*
         * Perform `per_block_processing` on the block and state, returning early if the block is
         * invalid.
         */

        write_state(
            &format!("state_pre_block_{}", block_root),
            &state,
            &chain.log,
        );
        write_block(&block, block_root, &chain.log);

        let core_timer = metrics::start_timer(&metrics::BLOCK_PROCESSING_CORE);

        if let Err(err) = per_block_processing(
            &mut state,
            &block,
            // Signatures were verified earlier in this function.
            BlockSignatureStrategy::NoVerification,
            StateProcessingStrategy::Accurate,
            VerifyBlockRoot::True,
            &mut consensus_context,
            &chain.spec,
        ) {
            match err {
                // Capture `BeaconStateError` so that we can easily distinguish between a block
                // that's invalid and one that caused an internal error.
                BlockProcessingError::BeaconStateError(e) => return Err(e.into()),
                other => return Err(BlockError::PerBlockProcessingError(other)),
            }
        };

        metrics::stop_timer(core_timer);

        /*
         * Calculate the state root of the newly modified state
         */

        let state_root_timer = metrics::start_timer(&metrics::BLOCK_PROCESSING_STATE_ROOT);

        let state_root = state.update_tree_hash_cache()?;

        metrics::stop_timer(state_root_timer);

        write_state(
            &format!("state_post_block_{}", block_root),
            &state,
            &chain.log,
        );

        /*
         * Check to ensure the state root on the block matches the one we have calculated.
         */

        if block.state_root() != state_root {
            return Err(BlockError::StateRootMismatch {
                block: block.state_root(),
                local: state_root,
            });
        }

        /*
         * Apply the block's attestations to fork choice.
         *
         * We're running in parallel with the payload verification at this point, so this is
         * free real estate.
         */
        let current_slot = chain.slot()?;
        let mut fork_choice = chain.canonical_head.fork_choice_write_lock();

        // Register each attester slashing in the block with fork choice.
        for attester_slashing in block.message().body().attester_slashings() {
            fork_choice.on_attester_slashing(attester_slashing);
        }

        // Register each attestation in the block with fork choice.
        for (i, attestation) in block.message().body().attestations().iter().enumerate() {
            let _fork_choice_attestation_timer =
                metrics::start_timer(&metrics::FORK_CHOICE_PROCESS_ATTESTATION_TIMES);

            let indexed_attestation = consensus_context
                .get_indexed_attestation(&state, attestation)
                .map_err(|e| BlockError::PerBlockProcessingError(e.into_with_index(i)))?;

            match fork_choice.on_attestation(
                current_slot,
                indexed_attestation,
                AttestationFromBlock::True,
            ) {
                Ok(()) => Ok(()),
                // Ignore invalid attestations whilst importing attestations from a block. The
                // block might be very old and therefore the attestations useless to fork choice.
                Err(ForkChoiceError::InvalidAttestation(_)) => Ok(()),
                Err(e) => Err(BlockError::BeaconChainError(e.into())),
            }?;
        }
        drop(fork_choice);

        Ok(Self {
            block,
            block_root,
            state,
            parent_block: parent.beacon_block,
            parent_eth1_finalization_data,
            confirmed_state_roots,
            consensus_context,
            payload_verification_handle,
        })
    }
}

/// Returns `Ok(())` if the block's slot is greater than the anchor block's slot (if any).
fn check_block_against_anchor_slot<T: BeaconChainTypes>(
    block: BeaconBlockRef<'_, T::EthSpec>,
    chain: &BeaconChain<T>,
) -> Result<(), BlockError<T::EthSpec>> {
    if let Some(anchor_slot) = chain.store.get_anchor_slot() {
        if block.slot() <= anchor_slot {
            return Err(BlockError::WeakSubjectivityConflict);
        }
    }
    Ok(())
}

/// Returns `Ok(())` if the block is later than the finalized slot on `chain`.
///
/// Returns an error if the block is earlier or equal to the finalized slot, or there was an error
/// verifying that condition.
fn check_block_against_finalized_slot<T: BeaconChainTypes>(
    block: BeaconBlockRef<'_, T::EthSpec>,
    block_root: Hash256,
    chain: &BeaconChain<T>,
) -> Result<(), BlockError<T::EthSpec>> {
    // The finalized checkpoint is being read from fork choice, rather than the cached head.
    //
    // Fork choice has the most up-to-date view of finalization and there's no point importing a
    // block which conflicts with the fork-choice view of finalization.
    let finalized_slot = chain
        .canonical_head
        .cached_head()
        .finalized_checkpoint()
        .epoch
        .start_slot(T::EthSpec::slots_per_epoch());

    if block.slot() <= finalized_slot {
        chain.pre_finalization_block_rejected(block_root);
        Err(BlockError::WouldRevertFinalizedSlot {
            block_slot: block.slot(),
            finalized_slot,
        })
    } else {
        Ok(())
    }
}

/// Returns `Ok(block)` if the block descends from the finalized root.
///
/// ## Warning
///
/// Taking a lock on the `chain.canonical_head.fork_choice` might cause a deadlock here.
pub fn check_block_is_finalized_checkpoint_or_descendant<T: BeaconChainTypes>(
    chain: &BeaconChain<T>,
    fork_choice: &BeaconForkChoice<T>,
    block: &Arc<SignedBeaconBlock<T::EthSpec>>,
) -> Result<(), BlockError<T::EthSpec>> {
    if fork_choice.is_finalized_checkpoint_or_descendant(block.parent_root()) {
        Ok(())
    } else {
        // If fork choice does *not* consider the parent to be a descendant of the finalized block,
        // then there are two more cases:
        //
        // 1. We have the parent stored in our database. Because fork-choice has confirmed the
        //    parent is *not* in our post-finalization DAG, all other blocks must be either
        //    pre-finalization or conflicting with finalization.
        // 2. The parent is unknown to us, we probably want to download it since it might actually
        //    descend from the finalized root.
        if chain
            .store
            .block_exists(&block.parent_root())
            .map_err(|e| BlockError::BeaconChainError(e.into()))?
        {
            Err(BlockError::NotFinalizedDescendant {
                block_parent_root: block.parent_root(),
            })
        } else {
            Err(BlockError::ParentUnknown(block.clone()))
        }
    }
}

/// Performs simple, cheap checks to ensure that the block is relevant to be imported.
///
/// `Ok(block_root)` is returned if the block passes these checks and should progress with
/// verification (viz., it is relevant).
///
/// Returns an error if the block fails one of these checks (viz., is not relevant) or an error is
/// experienced whilst attempting to verify.
pub fn check_block_relevancy<T: BeaconChainTypes>(
    signed_block: &SignedBeaconBlock<T::EthSpec>,
    block_root: Hash256,
    chain: &BeaconChain<T>,
) -> Result<Hash256, BlockError<T::EthSpec>> {
    let block = signed_block.message();

    // Do not process blocks from the future.
    if block.slot() > chain.slot()? {
        return Err(BlockError::FutureSlot {
            present_slot: chain.slot()?,
            block_slot: block.slot(),
        });
    }

    // Do not re-process the genesis block.
    if block.slot() == 0 {
        return Err(BlockError::GenesisBlock);
    }

    // This is an artificial (non-spec) restriction that provides some protection from overflow
    // abuses.
    if block.slot() >= MAXIMUM_BLOCK_SLOT_NUMBER {
        return Err(BlockError::BlockSlotLimitReached);
    }

    // Do not process a block from a finalized slot.
    check_block_against_finalized_slot(block, block_root, chain)?;

    // Check if the block is already known. We know it is post-finalization, so it is
    // sufficient to check the fork choice.
    if chain
        .canonical_head
        .fork_choice_read_lock()
        .contains_block(&block_root)
    {
        return Err(BlockError::BlockIsAlreadyKnown);
    }

    Ok(block_root)
}

/// Returns the canonical root of the given `block`.
///
/// Use this function to ensure that we report the block hashing time Prometheus metric.
pub fn get_block_root<E: EthSpec>(block: &SignedBeaconBlock<E>) -> Hash256 {
    let block_root_timer = metrics::start_timer(&metrics::BLOCK_PROCESSING_BLOCK_ROOT);

    let block_root = block.canonical_root();

    metrics::stop_timer(block_root_timer);

    block_root
}

/// Verify the parent of `block` is known, returning some information about the parent block from
/// fork choice.
#[allow(clippy::type_complexity)]
fn verify_parent_block_is_known<T: BeaconChainTypes>(
    chain: &BeaconChain<T>,
    block: Arc<SignedBeaconBlock<T::EthSpec>>,
) -> Result<(ProtoBlock, Arc<SignedBeaconBlock<T::EthSpec>>), BlockError<T::EthSpec>> {
    if let Some(proto_block) = chain
        .canonical_head
        .fork_choice_read_lock()
        .get_block(&block.message().parent_root())
    {
        Ok((proto_block, block))
    } else {
        Err(BlockError::ParentUnknown(block))
    }
}

/// Load the parent snapshot (block and state) of the given `block`.
///
/// Returns `Err(BlockError::ParentUnknown)` if the parent is not found, or if an error occurs
/// whilst attempting the operation.
#[allow(clippy::type_complexity)]
fn load_parent<T: BeaconChainTypes>(
    block_root: Hash256,
    block: Arc<SignedBeaconBlock<T::EthSpec>>,
    chain: &BeaconChain<T>,
) -> Result<
    (
        PreProcessingSnapshot<T::EthSpec>,
        Arc<SignedBeaconBlock<T::EthSpec>>,
    ),
    BlockError<T::EthSpec>,
> {
    let spec = &chain.spec;

    // Reject any block if its parent is not known to fork choice.
    //
    // A block that is not in fork choice is either:
    //
    //  - Not yet imported: we should reject this block because we should only import a child
    //  after its parent has been fully imported.
    //  - Pre-finalized: if the parent block is _prior_ to finalization, we should ignore it
    //  because it will revert finalization. Note that the finalized block is stored in fork
    //  choice, so we will not reject any child of the finalized block (this is relevant during
    //  genesis).
    if !chain
        .canonical_head
        .fork_choice_read_lock()
        .contains_block(&block.parent_root())
    {
        return Err(BlockError::ParentUnknown(block));
    }

    let block_delay = chain
        .block_times_cache
        .read()
        .get_block_delays(
            block_root,
            chain
                .slot_clock
                .start_of(block.slot())
                .unwrap_or_else(|| Duration::from_secs(0)),
        )
        .observed;

    let db_read_timer = metrics::start_timer(&metrics::BLOCK_PROCESSING_DB_READ);

    let result = if let Some((snapshot, cloned)) = chain
        .snapshot_cache
        .try_write_for(BLOCK_PROCESSING_CACHE_LOCK_TIMEOUT)
        .and_then(|mut snapshot_cache| {
            snapshot_cache.get_state_for_block_processing(
                block.parent_root(),
                block.slot(),
                block_delay,
                spec,
            )
        }) {
        if cloned {
            metrics::inc_counter(&metrics::BLOCK_PROCESSING_SNAPSHOT_CACHE_CLONES);
            debug!(
                chain.log,
                "Cloned snapshot for late block/skipped slot";
                "slot" => %block.slot(),
                "parent_slot" => %snapshot.beacon_block.slot(),
                "parent_root" => ?block.parent_root(),
                "block_delay" => ?block_delay,
            );
        }
        Ok((snapshot, block))
    } else {
        // Load the blocks parent block from the database, returning invalid if that block is not
        // found.
        //
        // We don't return a DBInconsistent error here since it's possible for a block to
        // exist in fork choice but not in the database yet. In such a case we simply
        // indicate that we don't yet know the parent.
        let root = block.parent_root();
        let parent_block = chain
            .get_blinded_block(&block.parent_root())
            .map_err(BlockError::BeaconChainError)?
            .ok_or_else(|| {
                // Return a `MissingBeaconBlock` error instead of a `ParentUnknown` error since
                // we've already checked fork choice for this block.
                //
                // It's an internal error if the block exists in fork choice but not in the
                // database.
                BlockError::from(BeaconChainError::MissingBeaconBlock(block.parent_root()))
            })?;

        // Load the parent block's state from the database, returning an error if it is not found.
        // It is an error because if we know the parent block we should also know the parent state.
        // Retrieve any state that is advanced through to at most `block.slot()`: this is
        // particularly important if `block` descends from the finalized/split block, but at a slot
        // prior to the finalized slot (which is invalid and inaccessible in our DB schema).
        let (parent_state_root, parent_state) = chain
            .store
            .get_advanced_hot_state(root, block.slot(), parent_block.state_root())?
            .ok_or_else(|| {
                BeaconChainError::DBInconsistent(
                    format!("Missing state for parent block {root:?}",),
                )
            })?;

        metrics::inc_counter(&metrics::BLOCK_PROCESSING_SNAPSHOT_CACHE_MISSES);
        debug!(
            chain.log,
            "Missed snapshot cache";
            "slot" => block.slot(),
            "parent_slot" => parent_block.slot(),
            "parent_root" => ?block.parent_root(),
            "block_delay" => ?block_delay,
        );

        Ok((
            PreProcessingSnapshot {
                beacon_block: parent_block,
                beacon_block_root: root,
                pre_state: parent_state,
                beacon_state_root: Some(parent_state_root),
            },
            block,
        ))
    };

    metrics::stop_timer(db_read_timer);

    result
}

/// Performs a cheap (time-efficient) state advancement so the committees and proposer shuffling for
/// `slot` can be obtained from `state`.
///
/// The state advancement is "cheap" since it does not generate state roots. As a result, the
/// returned state might be holistically invalid but the committees/proposers will be correct (since
/// they do not rely upon state roots).
///
/// If the given `state` can already serve the `slot`, the committees will be built on the `state`
/// and `Cow::Borrowed(state)` will be returned. Otherwise, the state will be cloned, cheaply
/// advanced and then returned as a `Cow::Owned`. The end result is that the given `state` is never
/// mutated to be invalid (in fact, it is never changed beyond a simple committee cache build).
fn cheap_state_advance_to_obtain_committees<'a, E: EthSpec>(
    state: &'a mut BeaconState<E>,
    state_root_opt: Option<Hash256>,
    block_slot: Slot,
    spec: &ChainSpec,
) -> Result<Cow<'a, BeaconState<E>>, BlockError<E>> {
    let block_epoch = block_slot.epoch(E::slots_per_epoch());

    if state.current_epoch() == block_epoch {
        // Build both the current and previous epoch caches, as the previous epoch caches are
        // useful for verifying attestations in blocks from the current epoch.
        state.build_committee_cache(RelativeEpoch::Previous, spec)?;
        state.build_committee_cache(RelativeEpoch::Current, spec)?;

        Ok(Cow::Borrowed(state))
    } else if state.slot() > block_slot {
        Err(BlockError::BlockIsNotLaterThanParent {
            block_slot,
            parent_slot: state.slot(),
        })
    } else {
        let mut state = state.clone_with(CloneConfig::committee_caches_only());
        let target_slot = block_epoch.start_slot(E::slots_per_epoch());

        // Advance the state into the same epoch as the block. Use the "partial" method since state
        // roots are not important for proposer/attester shuffling.
        partial_state_advance(&mut state, state_root_opt, target_slot, spec)
            .map_err(|e| BlockError::BeaconChainError(BeaconChainError::from(e)))?;

        state.build_committee_cache(RelativeEpoch::Previous, spec)?;
        state.build_committee_cache(RelativeEpoch::Current, spec)?;

        Ok(Cow::Owned(state))
    }
}

/// Obtains a read-locked `ValidatorPubkeyCache` from the `chain`.
pub fn get_validator_pubkey_cache<T: BeaconChainTypes>(
    chain: &BeaconChain<T>,
) -> Result<RwLockReadGuard<ValidatorPubkeyCache<T>>, BlockError<T::EthSpec>> {
    chain
        .validator_pubkey_cache
        .try_read_for(VALIDATOR_PUBKEY_CACHE_LOCK_TIMEOUT)
        .ok_or(BeaconChainError::ValidatorPubkeyCacheLockTimeout)
        .map_err(BlockError::BeaconChainError)
}

/// Produces an _empty_ `BlockSignatureVerifier`.
///
/// The signature verifier is empty because it does not yet have any of this block's signatures
/// added to it. Use `Self::apply_to_signature_verifier` to apply the signatures.
fn get_signature_verifier<'a, T: BeaconChainTypes>(
    state: &'a BeaconState<T::EthSpec>,
    validator_pubkey_cache: &'a ValidatorPubkeyCache<T>,
    spec: &'a ChainSpec,
) -> BlockSignatureVerifier<
    'a,
    T::EthSpec,
    impl Fn(usize) -> Option<Cow<'a, PublicKey>> + Clone,
    impl Fn(&'a PublicKeyBytes) -> Option<Cow<'a, PublicKey>>,
> {
    let get_pubkey = move |validator_index| {
        // Disallow access to any validator pubkeys that are not in the current beacon state.
        if validator_index < state.validators().len() {
            validator_pubkey_cache
                .get(validator_index)
                .map(Cow::Borrowed)
        } else {
            None
        }
    };

    let decompressor = move |pk_bytes| {
        // Map compressed pubkey to validator index.
        let validator_index = validator_pubkey_cache.get_index(pk_bytes)?;
        // Map validator index to pubkey (respecting guard on unknown validators).
        get_pubkey(validator_index)
    };

    BlockSignatureVerifier::new(state, get_pubkey, decompressor, spec)
}

/// Verify that `header` was signed with a valid signature from its proposer.
///
/// Return `Ok(())` if the signature is valid, and an `Err` otherwise.
fn verify_header_signature<T: BeaconChainTypes>(
    chain: &BeaconChain<T>,
    header: &SignedBeaconBlockHeader,
) -> Result<(), BlockError<T::EthSpec>> {
    let proposer_pubkey = get_validator_pubkey_cache(chain)?
        .get(header.message.proposer_index as usize)
        .cloned()
        .ok_or(BlockError::UnknownValidator(header.message.proposer_index))?;
    let head_fork = chain.canonical_head.cached_head().head_fork();

    if header.verify_signature::<T::EthSpec>(
        &proposer_pubkey,
        &head_fork,
        chain.genesis_validators_root,
        &chain.spec,
    ) {
        Ok(())
    } else {
        Err(BlockError::ProposalSignatureInvalid)
    }
}

fn write_state<T: EthSpec>(prefix: &str, state: &BeaconState<T>, log: &Logger) {
    if WRITE_BLOCK_PROCESSING_SSZ {
        let root = state.tree_hash_root();
        let filename = format!("{}_slot_{}_root_{}.ssz", prefix, state.slot(), root);
        let mut path = std::env::temp_dir().join("lighthouse");
        let _ = fs::create_dir_all(path.clone());
        path = path.join(filename);

        match fs::File::create(path.clone()) {
            Ok(mut file) => {
                let _ = file.write_all(&state.as_ssz_bytes());
            }
            Err(e) => error!(
                log,
                "Failed to log state";
                "path" => format!("{:?}", path),
                "error" => format!("{:?}", e)
            ),
        }
    }
}

fn write_block<T: EthSpec>(block: &SignedBeaconBlock<T>, root: Hash256, log: &Logger) {
    if WRITE_BLOCK_PROCESSING_SSZ {
        let filename = format!("block_slot_{}_root{}.ssz", block.slot(), root);
        let mut path = std::env::temp_dir().join("lighthouse");
        let _ = fs::create_dir_all(path.clone());
        path = path.join(filename);

        match fs::File::create(path.clone()) {
            Ok(mut file) => {
                let _ = file.write_all(&block.as_ssz_bytes());
            }
            Err(e) => error!(
                log,
                "Failed to log block";
                "path" => format!("{:?}", path),
                "error" => format!("{:?}", e)
            ),
        }
    }
}
