/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Traversals over the DOM and flow trees, running the layout computations.

use log::debug;
use script_layout_interface::wrapper_traits::{LayoutNode, ThreadSafeLayoutNode};
use servo_config::opts;
use style::context::{SharedStyleContext, StyleContext};
use style::data::ElementData;
use style::dom::{NodeInfo, TElement, TNode};
use style::selector_parser::RestyleDamage;
use style::servo::restyle_damage::ServoRestyleDamage;
use style::traversal::{DomTraversal, PerLevelTraversalData, recalc_style_at};

use crate::LayoutData;
use crate::construct::FlowConstructor;
use crate::context::LayoutContext;
use crate::display_list::DisplayListBuildState;
use crate::flow::{Flow, FlowFlags, GetBaseFlow, ImmutableFlowUtils};
use crate::wrapper::ThreadSafeLayoutNodeHelpers;

pub struct RecalcStyleAndConstructFlows<'a> {
    context: LayoutContext<'a>,
}

impl<'a> RecalcStyleAndConstructFlows<'a> {
    /// Creates a traversal context, taking ownership of the shared layout context.
    pub fn new(context: LayoutContext<'a>) -> Self {
        RecalcStyleAndConstructFlows { context }
    }

    pub fn context(&self) -> &LayoutContext<'a> {
        &self.context
    }

    /// Consumes this traversal context, returning ownership of the shared layout
    /// context to the caller.
    pub fn destroy(self) -> LayoutContext<'a> {
        self.context
    }
}

#[allow(unsafe_code)]
impl<'dom, E> DomTraversal<E> for RecalcStyleAndConstructFlows<'_>
where
    E: TElement,
    E::ConcreteNode: LayoutNode<'dom>,
{
    fn process_preorder<F>(
        &self,
        traversal_data: &PerLevelTraversalData,
        context: &mut StyleContext<E>,
        node: E::ConcreteNode,
        note_child: F,
    ) where
        F: FnMut(E::ConcreteNode),
    {
        // FIXME(pcwalton): Stop allocating here. Ideally this should just be
        // done by the HTML parser.
        unsafe { node.initialize_style_and_layout_data::<LayoutData>() };

        if !node.is_text_node() {
            let el = node.as_element().unwrap();
            let mut data = el.mutate_data().unwrap();
            recalc_style_at(self, traversal_data, context, el, &mut data, note_child);
        }
    }

    fn process_postorder(&self, _style_context: &mut StyleContext<E>, node: E::ConcreteNode) {
        construct_flows_at(&self.context, node);
    }

    fn text_node_needs_traversal(node: E::ConcreteNode, parent_data: &ElementData) -> bool {
        // Text nodes never need styling. However, there are two cases they may need
        // flow construction:
        // (1) They child doesn't yet have layout data (preorder traversal initializes it).
        // (2) The parent element has restyle damage (so the text flow also needs fixup).
        node.layout_data().is_none() || !parent_data.damage.is_empty()
    }

    fn shared_context(&self) -> &SharedStyleContext {
        &self.context.style_context
    }
}

/// A top-down traversal.
pub trait PreorderFlowTraversal {
    /// The operation to perform. Return true to continue or false to stop.
    fn process(&self, flow: &mut dyn Flow);

    /// Returns true if this node should be processed and false if neither this node nor its
    /// descendants should be processed.
    fn should_process_subtree(&self, _flow: &mut dyn Flow) -> bool {
        true
    }

    /// Returns true if this node must be processed in-order. If this returns false,
    /// we skip the operation for this node, but continue processing the descendants.
    /// This is called *after* parent nodes are visited.
    fn should_process(&self, _flow: &mut dyn Flow) -> bool {
        true
    }

    /// Traverses the tree in preorder.
    fn traverse(&self, flow: &mut dyn Flow) {
        if !self.should_process_subtree(flow) {
            return;
        }
        if self.should_process(flow) {
            self.process(flow);
        }
        for kid in flow.mut_base().child_iter_mut() {
            self.traverse(kid);
        }
    }

    /// Traverse the Absolute flow tree in preorder.
    ///
    /// Traverse all your direct absolute descendants, who will then traverse
    /// their direct absolute descendants.
    ///
    /// Return true if the traversal is to continue or false to stop.
    fn traverse_absolute_flows(&self, flow: &mut dyn Flow) {
        if self.should_process(flow) {
            self.process(flow);
        }
        for descendant_link in flow.mut_base().abs_descendants.iter() {
            self.traverse_absolute_flows(descendant_link)
        }
    }
}

/// A bottom-up traversal, with a optional in-order pass.
pub trait PostorderFlowTraversal {
    /// The operation to perform. Return true to continue or false to stop.
    fn process(&self, flow: &mut dyn Flow);

    /// Returns false if this node must be processed in-order. If this returns false, we skip the
    /// operation for this node, but continue processing the ancestors. This is called *after*
    /// child nodes are visited.
    fn should_process(&self, _flow: &mut dyn Flow) -> bool {
        true
    }

    /// Traverses the tree in postorder.
    fn traverse(&self, flow: &mut dyn Flow) {
        for kid in flow.mut_base().child_iter_mut() {
            self.traverse(kid);
        }
        if self.should_process(flow) {
            self.process(flow);
        }
    }
}

/// An in-order (sequential only) traversal.
pub trait InorderFlowTraversal {
    /// The operation to perform. Returns the level of the tree we're at.
    fn process(&mut self, flow: &mut dyn Flow, level: u32);

    /// Returns true if this node should be processed and false if neither this node nor its
    /// descendants should be processed.
    fn should_process_subtree(&mut self, _flow: &mut dyn Flow) -> bool {
        true
    }

    /// Traverses the tree in-order.
    fn traverse(&mut self, flow: &mut dyn Flow, level: u32) {
        if !self.should_process_subtree(flow) {
            return;
        }
        self.process(flow, level);
        for kid in flow.mut_base().child_iter_mut() {
            self.traverse(kid, level + 1);
        }
    }
}

/// A bottom-up, parallelizable traversal.
pub trait PostorderNodeMutTraversal<'dom, ConcreteThreadSafeLayoutNode>
where
    ConcreteThreadSafeLayoutNode: ThreadSafeLayoutNode<'dom>,
{
    /// The operation to perform. Return true to continue or false to stop.
    fn process(&mut self, node: &ConcreteThreadSafeLayoutNode);
}

/// # Safety
///
/// This function modifies the DOM node represented by the `node` argument, so it is imperitive
/// that no other thread is modifying the node at the same time.
#[inline]
#[allow(unsafe_code)]
pub unsafe fn construct_flows_at_ancestors<'dom>(
    context: &LayoutContext,
    mut node: impl LayoutNode<'dom>,
) {
    while let Some(element) = node.traversal_parent() {
        element.set_dirty_descendants();
        node = element.as_node();
        construct_flows_at(context, node);
    }
}

/// The flow construction traversal, which builds flows for styled nodes.
#[inline]
#[allow(unsafe_code)]
fn construct_flows_at<'dom>(context: &LayoutContext, node: impl LayoutNode<'dom>) {
    debug!("construct_flows_at: {:?}", node);

    // Construct flows for this node.
    {
        let tnode = node.to_threadsafe();

        // Always reconstruct if incremental layout is turned off.
        let nonincremental_layout = opts::get().nonincremental_layout;
        if nonincremental_layout ||
            tnode.restyle_damage() != RestyleDamage::empty() ||
            node.as_element()
                .is_some_and(|el| el.has_dirty_descendants())
        {
            let mut flow_constructor = FlowConstructor::new(context);
            if nonincremental_layout || !flow_constructor.repair_if_possible(&tnode) {
                flow_constructor.process(&tnode);
                debug!("Constructed flow for {:?}", tnode,);
            }
        }

        tnode
            .mutate_layout_data()
            .unwrap()
            .flags
            .insert(crate::data::LayoutDataFlags::HAS_BEEN_TRAVERSED);
    }

    if let Some(el) = node.as_element() {
        unsafe {
            el.unset_dirty_descendants();
        }
    }
}

/// The bubble-inline-sizes traversal, the first part of layout computation. This computes
/// preferred and intrinsic inline-sizes and bubbles them up the tree.
pub struct BubbleISizes<'a> {
    pub layout_context: &'a LayoutContext<'a>,
}

impl PostorderFlowTraversal for BubbleISizes<'_> {
    #[inline]
    fn process(&self, flow: &mut dyn Flow) {
        flow.bubble_inline_sizes();
        flow.mut_base()
            .restyle_damage
            .remove(ServoRestyleDamage::BUBBLE_ISIZES);
    }

    #[inline]
    fn should_process(&self, flow: &mut dyn Flow) -> bool {
        flow.base()
            .restyle_damage
            .contains(ServoRestyleDamage::BUBBLE_ISIZES)
    }
}

/// The assign-inline-sizes traversal. In Gecko this corresponds to `Reflow`.
#[derive(Clone, Copy)]
pub struct AssignISizes<'a> {
    pub layout_context: &'a LayoutContext<'a>,
}

impl PreorderFlowTraversal for AssignISizes<'_> {
    #[inline]
    fn process(&self, flow: &mut dyn Flow) {
        flow.assign_inline_sizes(self.layout_context);
    }

    #[inline]
    fn should_process(&self, flow: &mut dyn Flow) -> bool {
        flow.base()
            .restyle_damage
            .intersects(ServoRestyleDamage::REFLOW_OUT_OF_FLOW | ServoRestyleDamage::REFLOW)
    }
}

/// The assign-block-sizes-and-store-overflow traversal, the last (and most expensive) part of
/// layout computation. Determines the final block-sizes for all layout objects and computes
/// positions. In Gecko this corresponds to `Reflow`.
#[derive(Clone, Copy)]
pub struct AssignBSizes<'a> {
    pub layout_context: &'a LayoutContext<'a>,
}

impl PostorderFlowTraversal for AssignBSizes<'_> {
    #[inline]
    fn process(&self, flow: &mut dyn Flow) {
        // Can't do anything with anything that floats might flow through until we reach their
        // inorder parent.
        //
        // NB: We must return without resetting the restyle bits for these, as we haven't actually
        // reflowed anything!
        if flow.floats_might_flow_through() {
            return;
        }

        flow.assign_block_size(self.layout_context);
    }

    #[inline]
    fn should_process(&self, flow: &mut dyn Flow) -> bool {
        let base = flow.base();
        base.restyle_damage.intersects(ServoRestyleDamage::REFLOW_OUT_OF_FLOW | ServoRestyleDamage::REFLOW) &&
        // The fragmentation container is responsible for calling
        // Flow::fragment recursively.
        !base.flags.contains(FlowFlags::CAN_BE_FRAGMENTED)
    }
}

pub struct ComputeStackingRelativePositions<'a> {
    pub layout_context: &'a LayoutContext<'a>,
}

impl PreorderFlowTraversal for ComputeStackingRelativePositions<'_> {
    #[inline]
    fn should_process_subtree(&self, flow: &mut dyn Flow) -> bool {
        flow.base()
            .restyle_damage
            .contains(ServoRestyleDamage::REPOSITION)
    }

    #[inline]
    fn process(&self, flow: &mut dyn Flow) {
        flow.compute_stacking_relative_position(self.layout_context);
        flow.mut_base()
            .restyle_damage
            .remove(ServoRestyleDamage::REPOSITION)
    }
}

pub struct BuildDisplayList<'a> {
    pub state: DisplayListBuildState<'a>,
}

impl BuildDisplayList<'_> {
    #[inline]
    pub fn traverse(&mut self, flow: &mut dyn Flow) {
        if flow.has_non_invertible_transform_or_zero_scale() {
            return;
        }

        let parent_stacking_context_id = self.state.current_stacking_context_id;
        self.state.current_stacking_context_id = flow.base().stacking_context_id;

        let parent_clipping_and_scrolling = self.state.current_clipping_and_scrolling;
        self.state.current_clipping_and_scrolling = flow.clipping_and_scrolling();

        flow.build_display_list(&mut self.state);
        flow.mut_base()
            .restyle_damage
            .remove(ServoRestyleDamage::REPAINT);

        for kid in flow.mut_base().child_iter_mut() {
            self.traverse(kid);
        }

        self.state.current_stacking_context_id = parent_stacking_context_id;
        self.state.current_clipping_and_scrolling = parent_clipping_and_scrolling;
    }
}
