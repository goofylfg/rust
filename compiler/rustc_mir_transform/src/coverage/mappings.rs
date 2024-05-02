use std::collections::BTreeSet;

use rustc_data_structures::graph::DirectedGraph;
use rustc_index::bit_set::BitSet;
use rustc_index::IndexVec;
use rustc_middle::mir::coverage::{
    BlockMarkerId, BranchSpan, ConditionInfo, CoverageKind, MCDCBranchSpan,
};
use rustc_middle::mir::{self, BasicBlock, StatementKind};
use rustc_span::Span;

use crate::coverage::graph::{BasicCoverageBlock, CoverageGraph, START_BCB};
use crate::coverage::spans::{
    extract_refined_covspans, unexpand_into_body_span_with_visible_macro,
};
use crate::coverage::ExtractedHirInfo;

#[derive(Clone, Debug)]
pub(super) enum BcbMappingKind {
    /// Associates an ordinary executable code span with its corresponding BCB.
    Code(BasicCoverageBlock),

    // Ordinary branch mappings are stored separately, so they don't have a
    // variant in this enum.
    //
    /// Associates a mcdc branch span with condition info besides fields for normal branch.
    MCDCBranch {
        true_bcb: BasicCoverageBlock,
        false_bcb: BasicCoverageBlock,
        /// If `None`, this actually represents a normal branch mapping inserted
        /// for code that was too complex for MC/DC.
        condition_info: Option<ConditionInfo>,
        decision_depth: u16,
    },
}

#[derive(Debug)]
pub(super) struct BcbMapping {
    pub(super) kind: BcbMappingKind,
    pub(super) span: Span,
}

/// This is separate from [`BcbMappingKind`] to help prepare for larger changes
/// that will be needed for improved branch coverage in the future.
/// (See <https://github.com/rust-lang/rust/pull/124217>.)
#[derive(Debug)]
pub(super) struct BcbBranchPair {
    pub(super) span: Span,
    pub(super) true_bcb: BasicCoverageBlock,
    pub(super) false_bcb: BasicCoverageBlock,
}

/// Associates an MC/DC decision with its join BCBs.
#[derive(Debug)]
pub(super) struct MCDCDecision {
    pub(super) span: Span,
    pub(super) end_bcbs: BTreeSet<BasicCoverageBlock>,
    pub(super) bitmap_idx: u32,
    pub(super) conditions_num: u16,
    pub(super) decision_depth: u16,
}

pub(super) struct CoverageSpans {
    bcb_has_mappings: BitSet<BasicCoverageBlock>,
    pub(super) mappings: Vec<BcbMapping>,
    pub(super) branch_pairs: Vec<BcbBranchPair>,
    test_vector_bitmap_bytes: u32,
    pub(super) mcdc_decisions: Vec<MCDCDecision>,
}

impl CoverageSpans {
    pub(super) fn bcb_has_coverage_spans(&self, bcb: BasicCoverageBlock) -> bool {
        self.bcb_has_mappings.contains(bcb)
    }

    pub(super) fn test_vector_bitmap_bytes(&self) -> u32 {
        self.test_vector_bitmap_bytes
    }
}

/// Extracts coverage-relevant spans from MIR, and associates them with
/// their corresponding BCBs.
///
/// Returns `None` if no coverage-relevant spans could be extracted.
pub(super) fn generate_coverage_spans(
    mir_body: &mir::Body<'_>,
    hir_info: &ExtractedHirInfo,
    basic_coverage_blocks: &CoverageGraph,
) -> Option<CoverageSpans> {
    let mut mappings = vec![];
    let mut branch_pairs = vec![];
    let mut mcdc_decisions = vec![];

    if hir_info.is_async_fn {
        // An async function desugars into a function that returns a future,
        // with the user code wrapped in a closure. Any spans in the desugared
        // outer function will be unhelpful, so just keep the signature span
        // and ignore all of the spans in the MIR body.
        if let Some(span) = hir_info.fn_sig_span_extended {
            mappings.push(BcbMapping { kind: BcbMappingKind::Code(START_BCB), span });
        }
    } else {
        extract_refined_covspans(mir_body, hir_info, basic_coverage_blocks, &mut mappings);

        branch_pairs.extend(extract_branch_pairs(mir_body, hir_info, basic_coverage_blocks));

        mappings.extend(extract_mcdc_mappings(
            mir_body,
            hir_info.body_span,
            basic_coverage_blocks,
            &mut mcdc_decisions,
        ));
    }

    if mappings.is_empty() && branch_pairs.is_empty() && mcdc_decisions.is_empty() {
        return None;
    }

    // Identify which BCBs have one or more mappings.
    let mut bcb_has_mappings = BitSet::new_empty(basic_coverage_blocks.num_nodes());
    let mut insert = |bcb| {
        bcb_has_mappings.insert(bcb);
    };

    for BcbMapping { kind, span: _ } in &mappings {
        match *kind {
            BcbMappingKind::Code(bcb) => insert(bcb),
            BcbMappingKind::MCDCBranch { true_bcb, false_bcb, .. } => {
                insert(true_bcb);
                insert(false_bcb);
            }
        }
    }
    for &BcbBranchPair { true_bcb, false_bcb, .. } in &branch_pairs {
        insert(true_bcb);
        insert(false_bcb);
    }

    // Determine the length of the test vector bitmap.
    let test_vector_bitmap_bytes = mcdc_decisions
        .iter()
        .map(|&MCDCDecision { bitmap_idx, conditions_num, .. }| {
            bitmap_idx + (1_u32 << u32::from(conditions_num)).div_ceil(8)
        })
        .max()
        .unwrap_or(0);

    Some(CoverageSpans {
        bcb_has_mappings,
        mappings,
        branch_pairs,
        test_vector_bitmap_bytes,
        mcdc_decisions,
    })
}

fn resolve_block_markers(
    branch_info: &mir::coverage::BranchInfo,
    mir_body: &mir::Body<'_>,
) -> IndexVec<BlockMarkerId, Option<BasicBlock>> {
    let mut block_markers = IndexVec::<BlockMarkerId, Option<BasicBlock>>::from_elem_n(
        None,
        branch_info.num_block_markers,
    );

    // Fill out the mapping from block marker IDs to their enclosing blocks.
    for (bb, data) in mir_body.basic_blocks.iter_enumerated() {
        for statement in &data.statements {
            if let StatementKind::Coverage(CoverageKind::BlockMarker { id }) = statement.kind {
                block_markers[id] = Some(bb);
            }
        }
    }

    block_markers
}

// FIXME: There is currently a lot of redundancy between
// `extract_branch_pairs` and `extract_mcdc_mappings`. This is needed so
// that they can each be modified without interfering with the other, but in
// the long term we should try to bring them together again when branch coverage
// and MC/DC coverage support are more mature.

pub(super) fn extract_branch_pairs(
    mir_body: &mir::Body<'_>,
    hir_info: &ExtractedHirInfo,
    basic_coverage_blocks: &CoverageGraph,
) -> Vec<BcbBranchPair> {
    let Some(branch_info) = mir_body.coverage_branch_info.as_deref() else { return vec![] };

    let block_markers = resolve_block_markers(branch_info, mir_body);

    branch_info
        .branch_spans
        .iter()
        .filter_map(|&BranchSpan { span: raw_span, true_marker, false_marker }| {
            // For now, ignore any branch span that was introduced by
            // expansion. This makes things like assert macros less noisy.
            if !raw_span.ctxt().outer_expn_data().is_root() {
                return None;
            }
            let (span, _) =
                unexpand_into_body_span_with_visible_macro(raw_span, hir_info.body_span)?;

            let bcb_from_marker =
                |marker: BlockMarkerId| basic_coverage_blocks.bcb_from_bb(block_markers[marker]?);

            let true_bcb = bcb_from_marker(true_marker)?;
            let false_bcb = bcb_from_marker(false_marker)?;

            Some(BcbBranchPair { span, true_bcb, false_bcb })
        })
        .collect::<Vec<_>>()
}

pub(super) fn extract_mcdc_mappings(
    mir_body: &mir::Body<'_>,
    body_span: Span,
    basic_coverage_blocks: &CoverageGraph,
    mcdc_decisions: &mut impl Extend<MCDCDecision>,
) -> Vec<BcbMapping> {
    let Some(branch_info) = mir_body.coverage_branch_info.as_deref() else {
        return vec![];
    };

    let block_markers = resolve_block_markers(branch_info, mir_body);

    let bcb_from_marker =
        |marker: BlockMarkerId| basic_coverage_blocks.bcb_from_bb(block_markers[marker]?);

    let check_branch_bcb =
        |raw_span: Span, true_marker: BlockMarkerId, false_marker: BlockMarkerId| {
            // For now, ignore any branch span that was introduced by
            // expansion. This makes things like assert macros less noisy.
            if !raw_span.ctxt().outer_expn_data().is_root() {
                return None;
            }
            let (span, _) = unexpand_into_body_span_with_visible_macro(raw_span, body_span)?;

            let true_bcb = bcb_from_marker(true_marker)?;
            let false_bcb = bcb_from_marker(false_marker)?;
            Some((span, true_bcb, false_bcb))
        };

    let mcdc_branch_filter_map = |&MCDCBranchSpan {
                                      span: raw_span,
                                      true_marker,
                                      false_marker,
                                      condition_info,
                                      decision_depth,
                                  }| {
        check_branch_bcb(raw_span, true_marker, false_marker).map(|(span, true_bcb, false_bcb)| {
            BcbMapping {
                kind: BcbMappingKind::MCDCBranch {
                    true_bcb,
                    false_bcb,
                    condition_info,
                    decision_depth,
                },
                span,
            }
        })
    };

    let mut next_bitmap_idx = 0;

    mcdc_decisions.extend(branch_info.mcdc_decision_spans.iter().filter_map(
        |decision: &mir::coverage::MCDCDecisionSpan| {
            let (span, _) = unexpand_into_body_span_with_visible_macro(decision.span, body_span)?;

            let end_bcbs = decision
                .end_markers
                .iter()
                .map(|&marker| bcb_from_marker(marker))
                .collect::<Option<_>>()?;

            let bitmap_idx = next_bitmap_idx;
            next_bitmap_idx += (1_u32 << decision.conditions_num).div_ceil(8);

            Some(MCDCDecision {
                span,
                end_bcbs,
                bitmap_idx,
                conditions_num: decision.conditions_num as u16,
                decision_depth: decision.decision_depth,
            })
        },
    ));

    std::iter::empty()
        .chain(branch_info.mcdc_branch_spans.iter().filter_map(mcdc_branch_filter_map))
        .collect::<Vec<_>>()
}
