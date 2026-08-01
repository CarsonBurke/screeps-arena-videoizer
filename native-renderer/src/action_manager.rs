use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::{ActionRuntime, ActionTarget, Error, ResolvedActionNode, Result, SceneNodeKey};

#[derive(Clone, Debug)]
struct ActiveAction {
    group_id: u64,
    target_activation: u32,
    runtime: ActionRuntime,
}

/// Native equivalent of the renderer's global ActionManager plus the minimum
/// target-identity bookkeeping needed by a temporal scene compiler.
///
/// Handles retain the exact node activation they were started against. If a
/// processor replaces a scope node with the same public ID, old actions keep
/// mutating the detached old object, as they do in Pixi, and cannot leak onto
/// the replacement.
#[derive(Clone, Debug, Default)]
pub struct ActionManagerRuntime {
    targets: HashMap<u32, ActionTarget>,
    parent_activations: HashMap<u32, Option<u32>>,
    temporary_target_roots: HashMap<u32, u32>,
    addressable_targets: BTreeMap<SceneNodeKey, u32>,
    visible_targets: BTreeMap<SceneNodeKey, u32>,
    visible_activations: BTreeSet<u32>,
    pinned_targets: BTreeSet<u32>,
    handles: Vec<ActiveAction>,
    groups: BTreeSet<u64>,
}

impl ActionManagerRuntime {
    pub fn create_target(
        &mut self,
        activation_id: u32,
        key: SceneNodeKey,
        target: ActionTarget,
    ) -> Result<()> {
        self.create_target_with_parent(activation_id, key, target, None)
    }

    pub fn create_target_with_parent(
        &mut self,
        activation_id: u32,
        key: SceneNodeKey,
        target: ActionTarget,
        parent_activation: Option<u32>,
    ) -> Result<()> {
        if self.targets.contains_key(&activation_id) {
            return Err(Error::Invalid(format!(
                "action target activation {activation_id} is duplicated"
            )));
        }
        if parent_activation.is_some_and(|parent| !self.visible_activations.contains(&parent)) {
            return Err(Error::Invalid(format!(
                "action target activation {activation_id} references an unavailable parent"
            )));
        }
        self.targets.insert(activation_id, target);
        self.parent_activations
            .insert(activation_id, parent_activation);
        if let Some(replaced) = self.addressable_targets.insert(key.clone(), activation_id) {
            self.visible_activations.remove(&replaced);
        }
        if let Some(replaced) = self.visible_targets.insert(key, activation_id) {
            self.visible_activations.remove(&replaced);
        }
        self.visible_activations.insert(activation_id);
        self.collect_detached_targets();
        Ok(())
    }

    /// Create a display target in a processor-local identity namespace.
    ///
    /// Retained helpers such as the image branch of `userBadge` create a fresh
    /// local scope and never publish the result into the object's global
    /// evaluator scope. The target must still participate in the display tree
    /// and actions, but no public node ID may address or replace it.
    pub fn create_temporary_target_with_parent(
        &mut self,
        activation_id: u32,
        root_activation: u32,
        target: ActionTarget,
        parent_activation: u32,
    ) -> Result<()> {
        if self.targets.contains_key(&activation_id) {
            return Err(Error::Invalid(format!(
                "action target activation {activation_id} is duplicated"
            )));
        }
        if !self.visible_activations.contains(&parent_activation) {
            return Err(Error::Invalid(format!(
                "action target activation {activation_id} references an unavailable parent"
            )));
        }
        if !self.targets.contains_key(&root_activation) {
            return Err(Error::Invalid(format!(
                "temporary action target activation {activation_id} references missing root {root_activation}"
            )));
        }
        self.targets.insert(activation_id, target);
        self.parent_activations
            .insert(activation_id, Some(parent_activation));
        self.temporary_target_roots
            .insert(activation_id, root_activation);
        self.visible_activations.insert(activation_id);
        self.collect_detached_targets();
        Ok(())
    }

    pub fn destroy_target(&mut self, activation_id: u32) -> Result<()> {
        if !self.targets.contains_key(&activation_id) {
            return Err(Error::Invalid(format!(
                "action target activation {activation_id} does not exist"
            )));
        }
        self.addressable_targets
            .retain(|_, addressable_activation| *addressable_activation != activation_id);
        self.visible_targets
            .retain(|_, visible_activation| *visible_activation != activation_id);
        self.visible_activations.remove(&activation_id);
        self.temporary_target_roots.remove(&activation_id);
        self.collect_detached_targets();
        Ok(())
    }

    pub fn target(&self, activation_id: u32) -> Option<&ActionTarget> {
        self.targets.get(&activation_id)
    }

    pub fn target_mut(&mut self, activation_id: u32) -> Option<&mut ActionTarget> {
        self.targets.get_mut(&activation_id)
    }

    pub fn parent_activation(&self, activation_id: u32) -> Option<Option<u32>> {
        self.parent_activations.get(&activation_id).copied()
    }

    pub fn visible_activation(&self, key: &SceneNodeKey) -> Option<u32> {
        self.visible_targets.get(key).copied()
    }

    pub fn addressable_activation(&self, key: &SceneNodeKey) -> Option<u32> {
        self.addressable_targets.get(key).copied()
    }

    /// Destroy the display object while retaining the evaluator's global
    /// scope pointer. ReplayIR can legitimately target that stale identity
    /// after a processor-destruct event.
    pub fn hide_target(&mut self, activation_id: u32) -> Result<()> {
        if !self.targets.contains_key(&activation_id) {
            return Err(Error::Invalid(format!(
                "action target activation {activation_id} does not exist"
            )));
        }
        self.visible_targets
            .retain(|_, visible_activation| *visible_activation != activation_id);
        self.visible_activations.remove(&activation_id);
        self.collect_detached_targets();
        Ok(())
    }

    /// Model generic object() deleting `scope[id]` before it decides whether
    /// a replacement object can be constructed.
    pub fn destroy_key(&mut self, key: &SceneNodeKey) {
        if let Some(activation_id) = self.addressable_targets.remove(key) {
            self.visible_targets.remove(key);
            self.visible_activations.remove(&activation_id);
            self.collect_detached_targets();
        }
    }

    pub fn is_visible_activation(&self, activation_id: u32) -> bool {
        self.visible_activations.contains(&activation_id)
    }

    pub fn visible_activation_ids(&self) -> impl ExactSizeIterator<Item = u32> + '_ {
        self.visible_activations.iter().copied()
    }

    pub fn destroy_entity_scope(&mut self, entity_id: &str, root_activation: u32) {
        self.addressable_targets
            .retain(|key, _| key.entity_id != entity_id);
        self.visible_targets
            .retain(|key, _| key.entity_id != entity_id);
        let temporary = self
            .temporary_target_roots
            .iter()
            .filter_map(|(activation, root)| (*root == root_activation).then_some(*activation))
            .collect::<Vec<_>>();
        for activation in temporary {
            self.temporary_target_roots.remove(&activation);
            self.visible_activations.remove(&activation);
        }
        let public_visible = self
            .visible_targets
            .values()
            .copied()
            .collect::<BTreeSet<_>>();
        self.visible_activations.retain(|activation| {
            public_visible.contains(activation)
                || self.temporary_target_roots.contains_key(activation)
        });
        self.collect_detached_targets();
    }

    /// Retire one GameObject generation's evaluator scope while leaving its
    /// display subtree alive for a disappear animation. The returned
    /// activation identities belong only to that generation, so delayed
    /// cleanup cannot erase targets created later for a reused entity ID.
    pub fn retire_entity_scope(&mut self, entity_id: &str, root_activation: u32) -> Vec<u32> {
        let mut activations = BTreeSet::from([root_activation]);
        loop {
            let before = activations.len();
            for (activation, parent) in &self.parent_activations {
                if parent.is_some_and(|parent| activations.contains(&parent)) {
                    activations.insert(*activation);
                }
            }
            if activations.len() == before {
                break;
            }
        }
        self.addressable_targets
            .retain(|key, _| key.entity_id != entity_id);
        self.pinned_targets.extend(activations.iter().copied());
        activations.into_iter().collect()
    }

    pub fn release_targets(&mut self, activation_ids: &[u32]) {
        for activation_id in activation_ids {
            self.pinned_targets.remove(activation_id);
        }
        self.collect_detached_targets();
    }

    pub fn start_group(
        &mut self,
        group_id: u64,
        target_activation: u32,
        actions: &[ResolvedActionNode],
    ) -> Result<()> {
        if self.groups.contains(&group_id) {
            return Err(Error::Invalid(format!(
                "action group {group_id} is already active"
            )));
        }
        if !self.targets.contains_key(&target_activation) {
            return Err(Error::Invalid(format!(
                "action group {group_id} references missing target activation {target_activation}"
            )));
        }
        let runtimes = actions
            .iter()
            .map(ActionRuntime::from_resolved)
            .collect::<Result<Vec<_>>>()?;
        for runtime in runtimes {
            self.handles.push(ActiveAction {
                group_id,
                target_activation,
                runtime,
            });
        }
        self.groups.insert(group_id);
        Ok(())
    }

    /// Finish active handles in the same creation order as World.finishActions.
    pub fn finish_group(&mut self, group_id: u64) -> Result<()> {
        if !self.groups.remove(&group_id) {
            return Err(Error::Invalid(format!(
                "action group {group_id} does not exist"
            )));
        }
        for handle in &mut self.handles {
            if handle.group_id != group_id {
                continue;
            }
            let target = self
                .targets
                .get_mut(&handle.target_activation)
                .expect("target activations outlive their action handles");
            handle.runtime.finish(target)?;
        }
        self.handles.retain(|handle| handle.group_id != group_id);
        self.collect_detached_targets();
        Ok(())
    }

    pub fn cancel_group(&mut self, group_id: u64) -> Result<()> {
        if !self.groups.remove(&group_id) {
            return Err(Error::Invalid(format!(
                "action group {group_id} does not exist"
            )));
        }
        self.handles.retain(|handle| handle.group_id != group_id);
        self.collect_detached_targets();
        Ok(())
    }

    /// Drop lifecycle ownership while allowing already-started handles to run
    /// to natural completion. Generic processor replacement overwrites its
    /// processor scope this way without cancelling actions on the detached
    /// Pixi object.
    pub fn detach_group(&mut self, group_id: u64) -> Result<()> {
        if !self.groups.remove(&group_id) {
            return Err(Error::Invalid(format!(
                "action group {group_id} does not exist"
            )));
        }
        Ok(())
    }

    /// Object destruction recursively cancels handles without applying final
    /// values. Processor node destruction deliberately does not call this:
    /// generic helper actions remain attached to the detached old Pixi object.
    pub fn cancel_for_target(&mut self, target_activation: u32) {
        self.handles
            .retain(|handle| handle.target_activation != target_activation);
        self.collect_detached_targets();
    }

    /// Update a stable snapshot of all active handles in insertion order, then
    /// retire naturally completed handles.
    pub fn update(&mut self, delta_seconds: f64) -> Result<()> {
        let delta_ms = delta_seconds * 1_000.0;
        if !delta_ms.is_finite() || delta_ms < 0.0 {
            return Err(Error::Invalid(
                "action-manager delta must be a nonnegative finite number".to_owned(),
            ));
        }
        let mut update_error = None;
        let handle_count_before = self.handles.len();
        self.handles.retain_mut(|handle| {
            let target = self
                .targets
                .get_mut(&handle.target_activation)
                .expect("target activations outlive their action handles");
            match handle.runtime.update(target, delta_ms) {
                Ok(ended) => !ended,
                Err(error) => {
                    update_error = Some(error);
                    false
                }
            }
        });
        // Detached-target GC is only needed when handle ownership changes. Create,
        // destroy, pin, and replace paths already call collect_detached_targets.
        // Avoid rebuilding the retained set on every substep of a long timeline.
        if self.handles.len() != handle_count_before {
            self.collect_detached_targets();
        }
        if let Some(error) = update_error {
            return Err(error);
        }
        Ok(())
    }

    pub fn visible_targets(&self) -> BTreeMap<SceneNodeKey, ActionTarget> {
        self.visible_targets
            .iter()
            .map(|(key, activation)| (key.clone(), self.targets[activation].clone()))
            .collect()
    }

    pub fn active_handle_count(&self) -> usize {
        self.handles.len()
    }

    fn collect_detached_targets(&mut self) {
        let retained = self
            .addressable_targets
            .values()
            .copied()
            .chain(self.visible_targets.values().copied())
            .chain(self.visible_activations.iter().copied())
            .chain(self.pinned_targets.iter().copied())
            .chain(self.handles.iter().map(|handle| handle.target_activation))
            .collect::<BTreeSet<_>>();
        self.targets
            .retain(|activation, _| retained.contains(activation));
        self.parent_activations
            .retain(|activation, _| retained.contains(activation));
        self.temporary_target_roots
            .retain(|activation, _| retained.contains(activation));
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        ActionKind, ActionManagerRuntime, ActionTarget, ResolvedActionNode,
        ResolvedActionParameter as Parameter, ResolvedValue, SceneNodeKey,
    };

    fn value(value: f64) -> Parameter {
        Parameter::Value(ResolvedValue::Number(value))
    }

    fn text(value: &str) -> Parameter {
        Parameter::Value(ResolvedValue::String(value.to_owned()))
    }

    fn action(kind: ActionKind, params: Vec<Parameter>) -> ResolvedActionNode {
        ResolvedActionNode { kind, params }
    }

    fn nested(node: ResolvedActionNode) -> Parameter {
        Parameter::Action(Box::new(node))
    }

    fn key(node_id: &str) -> SceneNodeKey {
        SceneNodeKey {
            entity_id: "one".to_owned(),
            node_id: node_id.to_owned(),
            is_root: false,
        }
    }

    fn root_key() -> SceneNodeKey {
        SceneNodeKey {
            entity_id: "one".to_owned(),
            node_id: "__root__".to_owned(),
            is_root: true,
        }
    }

    #[test]
    fn updates_handles_in_global_insertion_order() {
        let mut manager = ActionManagerRuntime::default();
        manager
            .create_target(1, key("body"), ActionTarget::default())
            .unwrap();
        manager
            .start_group(
                10,
                1,
                &[
                    action(ActionKind::AlphaTo, vec![value(0.0), value(1.0)]),
                    action(ActionKind::AlphaTo, vec![value(1.0), value(1.0)]),
                ],
            )
            .unwrap();
        manager.update(0.5).unwrap();
        assert_eq!(manager.target(1).unwrap().alpha, 0.75);
        assert_eq!(manager.active_handle_count(), 2);
    }

    #[test]
    fn replacement_keeps_old_handles_bound_to_the_detached_activation() {
        let mut manager = ActionManagerRuntime::default();
        manager
            .create_target(1, key("body"), ActionTarget::default())
            .unwrap();
        manager
            .start_group(
                10,
                1,
                &[action(ActionKind::AlphaTo, vec![value(0.0), value(1.0)])],
            )
            .unwrap();
        manager
            .create_target(2, key("body"), ActionTarget::default())
            .unwrap();
        manager.update(0.5).unwrap();

        assert_eq!(manager.target(1).unwrap().alpha, 0.5);
        assert_eq!(manager.target(2).unwrap().alpha, 1.0);
        assert_eq!(manager.visible_activation(&key("body")), Some(2));
        assert_eq!(manager.visible_targets()[&key("body")].alpha, 1.0);

        manager.update(0.5).unwrap();
        assert_eq!(manager.active_handle_count(), 0);
        assert!(manager.target(1).is_none());
        // The official processor scope can still later finish an array whose
        // handles have already left ActionManager; that is a no-op.
        manager.finish_group(10).unwrap();
    }

    #[test]
    fn finish_and_cancel_follow_action_manager_semantics() {
        let mut manager = ActionManagerRuntime::default();
        manager
            .create_target(1, key("body"), ActionTarget::default())
            .unwrap();
        manager
            .start_group(
                10,
                1,
                &[action(
                    ActionKind::MoveTo,
                    vec![value(5.0), value(7.0), value(2.0)],
                )],
            )
            .unwrap();
        manager.finish_group(10).unwrap();
        assert_eq!(
            [manager.target(1).unwrap().x, manager.target(1).unwrap().y],
            [5.0, 7.0]
        );
        assert_eq!(manager.active_handle_count(), 0);

        manager
            .start_group(
                11,
                1,
                &[action(ActionKind::AlphaTo, vec![value(0.0), value(1.0)])],
            )
            .unwrap();
        manager.cancel_group(11).unwrap();
        manager.update(1.0).unwrap();
        assert_eq!(manager.target(1).unwrap().alpha, 1.0);
    }

    #[test]
    fn failed_creation_and_group_instantiation_are_atomic() {
        let mut manager = ActionManagerRuntime::default();
        manager
            .create_target(1, key("body"), ActionTarget::default())
            .unwrap();
        let replacement = ActionTarget {
            alpha: 0.2,
            ..ActionTarget::default()
        };
        assert!(manager.create_target(1, key("other"), replacement).is_err());
        assert_eq!(manager.target(1).unwrap().alpha, 1.0);
        assert_eq!(manager.visible_activation(&key("body")), Some(1));

        let invalid_ease = action(
            ActionKind::Ease,
            vec![nested(action(
                ActionKind::Sequence,
                vec![Parameter::Array(Vec::new())],
            ))],
        );
        assert!(manager.start_group(10, 1, &[invalid_ease]).is_err());
        assert_eq!(manager.active_handle_count(), 0);
        manager.start_group(10, 1, &[]).unwrap();
    }

    #[test]
    fn failed_handle_is_retired_after_later_handles_update() {
        let mut manager = ActionManagerRuntime::default();
        manager
            .create_target(1, key("body"), ActionTarget::default())
            .unwrap();
        manager
            .start_group(
                10,
                1,
                &[
                    action(
                        ActionKind::FilterTo,
                        vec![value(0.0), text("blur"), value(4.0), value(1.0)],
                    ),
                    action(ActionKind::AlphaTo, vec![value(0.0), value(1.0)]),
                ],
            )
            .unwrap();

        assert!(manager.update(0.5).is_err());
        assert_eq!(manager.target(1).unwrap().alpha, 0.5);
        assert_eq!(manager.active_handle_count(), 1);
        manager.update(0.5).unwrap();
        assert_eq!(manager.target(1).unwrap().alpha, 0.0);
        assert_eq!(manager.active_handle_count(), 0);
        manager.finish_group(10).unwrap();
    }

    #[test]
    fn display_visibility_and_scope_addressability_are_independent() {
        let mut manager = ActionManagerRuntime::default();
        manager
            .create_target(1, key("body"), ActionTarget::default())
            .unwrap();
        manager.hide_target(1).unwrap();
        assert_eq!(manager.visible_activation(&key("body")), None);
        assert_eq!(manager.addressable_activation(&key("body")), Some(1));
        assert!(manager.target(1).is_some());

        manager
            .start_group(
                10,
                1,
                &[action(ActionKind::AlphaTo, vec![value(0.0), value(1.0)])],
            )
            .unwrap();
        manager.destroy_key(&key("body"));
        assert_eq!(manager.addressable_activation(&key("body")), None);
        assert!(manager.target(1).is_some());
        manager.update(1.0).unwrap();
        assert!(manager.target(1).is_none());
    }

    #[test]
    fn temporary_targets_are_visible_but_never_publicly_addressable() {
        let mut manager = ActionManagerRuntime::default();
        manager
            .create_target(1, root_key(), ActionTarget::default())
            .unwrap();
        manager
            .create_temporary_target_with_parent(
                2,
                1,
                ActionTarget {
                    alpha: 0.25,
                    ..ActionTarget::default()
                },
                1,
            )
            .unwrap();
        manager
            .create_target_with_parent(
                3,
                key("$temporary.processor.2"),
                ActionTarget {
                    alpha: 0.75,
                    ..ActionTarget::default()
                },
                Some(1),
            )
            .unwrap();

        assert!(manager.is_visible_activation(2));
        assert!(manager.is_visible_activation(3));
        assert_eq!(
            manager.addressable_activation(&key("$temporary.processor.2")),
            Some(3)
        );
        assert_eq!(
            manager.visible_activation(&key("$temporary.processor.2")),
            Some(3)
        );
        assert_eq!(manager.target(2).unwrap().alpha, 0.25);

        manager
            .start_group(10, 2, &[action(ActionKind::DelayTime, vec![value(1.0)])])
            .unwrap();
        manager.hide_target(2).unwrap();
        assert!(manager.target(2).is_some());
        manager.finish_group(10).unwrap();
        assert!(manager.target(2).is_none());

        manager.destroy_entity_scope("one", 1);
        assert!(!manager.is_visible_activation(2));
        assert!(!manager.is_visible_activation(3));
        assert!(manager.target(2).is_none());
    }

    #[test]
    fn retiring_an_entity_scope_pins_detached_generation_descendants() {
        let mut manager = ActionManagerRuntime::default();
        manager
            .create_target(1, root_key(), ActionTarget::default())
            .unwrap();
        manager
            .create_target_with_parent(2, key("body"), ActionTarget::default(), Some(1))
            .unwrap();
        manager
            .start_group(7, 2, &[action(ActionKind::DelayTime, vec![value(1.0)])])
            .unwrap();
        manager
            .create_target_with_parent(3, key("body"), ActionTarget::default(), Some(1))
            .unwrap();

        let retired = manager.retire_entity_scope("one", 1);
        assert_eq!(retired, [1, 2, 3]);
        assert_eq!(manager.addressable_activation(&root_key()), None);
        assert_eq!(manager.addressable_activation(&key("body")), None);
        assert!(manager.target(2).is_some());
        assert!(manager.target(3).is_some());

        manager.release_targets(&retired);
        assert!(manager.target(2).is_some());
        assert!(manager.target(3).is_some());
        for activation in retired {
            manager.cancel_for_target(activation);
            if manager.target(activation).is_some() {
                manager.destroy_target(activation).unwrap();
            }
        }
        assert!(manager.target(2).is_none());
        assert!(manager.target(3).is_none());
    }
}
