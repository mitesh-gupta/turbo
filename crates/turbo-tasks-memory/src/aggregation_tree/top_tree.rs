use std::{mem::transmute, ops::ControlFlow, sync::Arc};

use parking_lot::{Mutex, MutexGuard};

use super::{inner_refs::TopRef, leaf::top_tree, AggregationContext};
use crate::count_hash_set::CountHashSet;

pub struct TopTree<T> {
    pub depth: u8,
    state: Mutex<TopTreeState<T>>,
}

struct TopTreeState<T> {
    data: T,
    upper: CountHashSet<TopRef<T>>,
}

impl<T: Default> TopTree<T> {
    pub fn new(depth: u8) -> Self {
        Self {
            depth,
            state: Mutex::new(TopTreeState {
                data: T::default(),
                upper: CountHashSet::new(),
            }),
        }
    }
}

impl<T> TopTree<T> {
    pub(super) fn add_children_of_child<C: AggregationContext<Info = T>>(
        self: &Arc<Self>,
        context: &C,
        children: &[(u32, &C::ItemRef)],
    ) {
        for (_, child) in children {
            top_tree(context, child, self.depth + 1).add_upper(context, self);
        }
    }

    pub(super) fn add_child_of_child<C: AggregationContext<Info = T>>(
        self: &Arc<Self>,
        context: &C,
        child_of_child: &C::ItemRef,
    ) {
        top_tree(context, child_of_child, self.depth + 1).add_upper(context, self);
    }

    pub(super) fn remove_child_of_child<C: AggregationContext<Info = T>>(
        self: &Arc<Self>,
        context: &C,
        child_of_child: &C::ItemRef,
    ) {
        top_tree(context, child_of_child, self.depth + 1).remove_upper(context, self);
    }

    pub(super) fn add_upper<C: AggregationContext<Info = T>>(
        &self,
        context: &C,
        upper: &Arc<TopTree<T>>,
    ) {
        let mut state = self.state.lock();
        if state.upper.add(TopRef {
            upper: upper.clone(),
        }) {
            if let Some(change) = context.info_to_add_change(&state.data) {
                upper.child_change(context, &change);
            }
        }
    }

    pub(super) fn remove_upper<C: AggregationContext<Info = T>>(
        &self,
        context: &C,
        upper: &Arc<TopTree<T>>,
    ) {
        let mut state = self.state.lock();
        if state.upper.remove(TopRef {
            upper: upper.clone(),
        }) {
            if let Some(change) = context.info_to_remove_change(&state.data) {
                upper.child_change(context, &change);
            }
        }
    }

    pub(super) fn child_change<C: AggregationContext<Info = T>>(
        &self,
        context: &C,
        change: &C::ItemChange,
    ) {
        let mut state = self.state.lock();
        let change = context.apply_change(&mut state.data, change);
        propagate_change_to_upper(&mut state, context, change);
    }

    pub fn get_root_info<C: AggregationContext<Info = T>>(
        &self,
        context: &C,
        root_info_type: &C::RootInfoType,
    ) -> C::RootInfo {
        let state = self.state.lock();
        if self.depth == 0 {
            // This is the root
            context.info_to_root_info(&state.data, root_info_type)
        } else {
            let mut result = context.new_root_info(root_info_type);
            for TopRef { upper } in state.upper.iter() {
                let info = upper.get_root_info(context, root_info_type);
                if context.merge_root_info(&mut result, info) == ControlFlow::Break(()) {
                    break;
                }
            }
            result
        }
    }

    pub(super) fn lock_info(self: &Arc<Self>) -> AggregationInfoGuard<T> {
        AggregationInfoGuard {
            // SAFETY: We can cast the lifetime as we keep a strong reference to the tree.
            // The order of the field in the struct is important to drop guard before tree.
            guard: unsafe { transmute(self.state.lock()) },
            tree: self.clone(),
        }
    }
}

fn propagate_change_to_upper<C: AggregationContext>(
    state: &mut MutexGuard<TopTreeState<C::Info>>,
    context: &C,
    change: Option<C::ItemChange>,
) {
    let Some(change) = change else {
        return;
    };
    for TopRef { upper } in state.upper.iter() {
        upper.child_change(context, &change);
    }
}

pub struct AggregationInfoGuard<T: 'static> {
    guard: MutexGuard<'static, TopTreeState<T>>,
    #[allow(dead_code, reason = "need to stay alive until the guard is dropped")]
    tree: Arc<TopTree<T>>,
}

impl<T> std::ops::Deref for AggregationInfoGuard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard.data
    }
}

impl<T> std::ops::DerefMut for AggregationInfoGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard.data
    }
}
