use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::num::NonZeroU32;

use crate::{
    ActionKind, ActionManagerRuntime, ActionRuntime, BoardTransform, Error, FrameSample,
    PreparedSprite, PreparedSpriteInstance, ProcessorKind, RendererEvent, RendererEventOpcode,
    RendererPlan, ReplayArtifact, ResolvedActionNode, ResolvedActionParameter, ResolvedActivation,
    ResolvedScene, ResolvedValue, Result, SceneDisplayEntry, SceneDrawableKind, SceneFrameScratch,
    SceneNodeKey, SceneNodeTemplate, SceneNodeTemplates, SpriteDisplayEntry, SpritePipeline,
    TemporalSpriteBatch, TemporalVectorBatch, Timeline, TimelineEvent,
};

const PROCESSOR_ACTION_GROUP_NAMESPACE: u64 = 1 << 63;

#[derive(Clone, Debug)]
struct ActiveObject {
    object_type: String,
    root_activation: u32,
}

#[derive(Clone, Debug)]
struct ActiveProcessor {
    target_activation: Option<u32>,
    action_group: Option<u64>,
}

#[derive(Clone, Debug)]
struct PendingDisappear {
    entity_id: String,
    root_activation: u32,
    target_activations: Vec<u32>,
    fade: ActionRuntime,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TemporalSceneStats {
    pub frames: u64,
    pub batches: u64,
    pub max_instances_per_view: u32,
    pub max_vectors_per_view: u32,
    pub max_vector_vertices_per_batch: u32,
}

#[derive(Clone, Debug)]
pub struct TemporalSceneBatch {
    pub sprites: TemporalSpriteBatch,
    pub vectors: TemporalVectorBatch,
    pub display_order: Vec<SceneDisplayEntry>,
}

/// Stateful compatibility compiler for the already-lowered generic native
/// scene. It consumes authenticated renderer events and exact timeline advance
/// steps once; render calls only compose the current target values into GPU
/// sprite instances.
pub struct GenericSceneRuntime<'a> {
    artifact: &'a ReplayArtifact,
    plan: &'a RendererPlan,
    templates: &'a SceneNodeTemplates,
    activations: BTreeMap<u32, &'a ResolvedActivation>,
    nodes: BTreeMap<u32, &'a SceneNodeTemplate>,
    active_objects: BTreeMap<String, ActiveObject>,
    active_processors: BTreeMap<(String, String), ActiveProcessor>,
    active_actions: BTreeMap<(String, String), u64>,
    action_manager: ActionManagerRuntime,
    pending_disappears: Vec<PendingDisappear>,
    tick_transition_seconds: f64,
    display_order: Vec<SceneDisplayEntry>,
    sprite_display_order: Vec<SpriteDisplayEntry>,
    display_ranks: HashMap<u32, usize>,
}

impl<'a> GenericSceneRuntime<'a> {
    pub fn new(
        artifact: &'a ReplayArtifact,
        plan: &'a RendererPlan,
        scene: &'a ResolvedScene,
        templates: &'a SceneNodeTemplates,
    ) -> Result<Self> {
        let activations = scene
            .activations
            .iter()
            .map(|activation| (activation_order(activation), activation))
            .collect::<BTreeMap<_, _>>();
        if activations.len() != scene.activations.len() {
            return Err(Error::Invalid(
                "resolved scene repeats a renderer activation order".to_owned(),
            ));
        }
        let nodes = templates
            .nodes
            .iter()
            .map(|node| (node.activation_order, node))
            .collect::<BTreeMap<_, _>>();
        if nodes.len() != templates.nodes.len() {
            return Err(Error::Invalid(
                "scene nodes repeat a renderer activation order".to_owned(),
            ));
        }
        let tick_transition_seconds = crate::Rational::parse_rate(
            artifact
                .replay
                .timeline
                .tick_transition_seconds
                .0
                .as_deref()
                .ok_or_else(|| {
                    Error::Invalid("ReplayIR timeline lacks tickTransitionSeconds".to_owned())
                })?,
            "tickTransitionSeconds",
        )?
        .as_f64();
        Ok(Self {
            artifact,
            plan,
            templates,
            activations,
            nodes,
            active_objects: BTreeMap::new(),
            active_processors: BTreeMap::new(),
            active_actions: BTreeMap::new(),
            action_manager: ActionManagerRuntime::default(),
            pending_disappears: Vec::new(),
            tick_transition_seconds,
            display_order: Vec::new(),
            sprite_display_order: Vec::new(),
            display_ranks: HashMap::new(),
        })
    }

    pub fn apply_tick(&mut self, tick: u32) -> Result<()> {
        let mut display_order_dirty = false;
        for event in self.artifact.events_at(tick)? {
            display_order_dirty |= matches!(
                event.opcode,
                RendererEventOpcode::ObjectCreate
                    | RendererEventOpcode::ObjectRemove
                    | RendererEventOpcode::ProcessorRun
                    | RendererEventOpcode::ProcessorDestruct
            );
            self.apply_event(event)?;
        }
        if display_order_dirty {
            self.rebuild_display_order()?;
        }
        Ok(())
    }

    pub fn advance(&mut self, duration_seconds: f64) -> Result<()> {
        self.action_manager.update(duration_seconds)?;
        let mut completed = Vec::new();
        for (index, pending) in self.pending_disappears.iter_mut().enumerate() {
            let target = self
                .action_manager
                .target_mut(pending.root_activation)
                .ok_or_else(|| {
                    Error::Invalid(format!(
                        "disappearing object {} lost its root target",
                        pending.entity_id
                    ))
                })?;
            if pending.fade.update(target, duration_seconds * 1_000.0)? {
                completed.push(index);
            }
        }
        let display_order_dirty = !completed.is_empty();
        for index in completed.into_iter().rev() {
            let pending = self.pending_disappears.swap_remove(index);
            self.action_manager
                .release_targets(&pending.target_activations);
            for activation in pending.target_activations {
                self.action_manager.cancel_for_target(activation);
                if self.action_manager.target(activation).is_some() {
                    self.action_manager.destroy_target(activation)?;
                }
            }
        }
        if display_order_dirty {
            self.rebuild_display_order()?;
        }
        Ok(())
    }

    pub fn prepare(&self, tick: u32, board: BoardTransform) -> Result<Vec<PreparedSprite>> {
        let mut sprites =
            self.templates
                .prepare_with_action_manager_at(tick, board, &self.action_manager)?;
        sprites.sort_by_key(|sprite| {
            self.display_ranks
                .get(&sprite.activation_order)
                .copied()
                .unwrap_or(usize::MAX)
        });
        Ok(sprites)
    }

    pub fn prepare_vectors(
        &self,
        tick: u32,
        board: BoardTransform,
    ) -> Result<Vec<crate::PreparedVector<'a>>> {
        let mut vectors = self.templates.prepare_vectors_with_action_manager_at(
            tick,
            board,
            &self.action_manager,
        )?;
        vectors.sort_by_key(|vector| {
            self.display_ranks
                .get(&vector.activation_order)
                .copied()
                .unwrap_or(usize::MAX)
        });
        for vector in &mut vectors {
            let rank = self.display_ranks[&vector.activation_order];
            vector.layer_order = self.display_order[rank].layer_order;
        }
        Ok(vectors)
    }

    pub fn display_order(&self) -> &[SceneDisplayEntry] {
        &self.display_order
    }

    pub fn prepare_gpu_instances<'s>(
        &self,
        board: BoardTransform,
        scratch: &'s mut SceneFrameScratch,
    ) -> Result<&'s [PreparedSpriteInstance]> {
        self.templates.prepare_gpu_instances_with_action_manager(
            board,
            &self.action_manager,
            &self.sprite_display_order,
            scratch,
        )
    }

    /// Stream the exact compatibility timeline into bounded multiview-ready
    /// batches without retaining replay-wide frame state.
    pub fn visit_temporal_batches(
        &mut self,
        timeline: Timeline,
        board: BoardTransform,
        views_per_batch: NonZeroU32,
        mut visit: impl FnMut(&[FrameSample], &TemporalSpriteBatch) -> Result<()>,
    ) -> Result<TemporalSceneStats> {
        let capacity = views_per_batch.get() as usize;
        if !(SpritePipeline::MIN_VIEWS_PER_BATCH..=SpritePipeline::MAX_VIEWS_PER_BATCH)
            .contains(&views_per_batch.get())
        {
            return Err(Error::Invalid(
                "temporal scene batch capacity is outside the GPU multiview range".to_owned(),
            ));
        }
        let mut view_scratches = (0..capacity)
            .map(|_| SceneFrameScratch::default())
            .collect::<Vec<_>>();
        let mut frames = Vec::<FrameSample>::with_capacity(capacity);
        let mut batch = TemporalSpriteBatch::empty();
        let mut active_views = 0usize;
        let mut stats = TemporalSceneStats::default();

        for event in timeline.events() {
            match event? {
                TimelineEvent::ApplyTick { tick, .. } => self.apply_tick(tick)?,
                TimelineEvent::Advance(step) => self.advance(step.duration_seconds)?,
                TimelineEvent::Render(frame) => {
                    self.prepare_gpu_instances(board, &mut view_scratches[active_views])?;
                    frames.push(frame);
                    active_views += 1;
                    stats.frames += 1;
                    if active_views == capacity {
                        emit_temporal_batch(
                            views_per_batch,
                            &view_scratches,
                            active_views,
                            &frames,
                            &mut batch,
                            &mut stats,
                            &mut visit,
                        )?;
                        active_views = 0;
                        frames.clear();
                    }
                }
            }
        }
        if active_views != 0 {
            emit_temporal_batch(
                views_per_batch,
                &view_scratches,
                active_views,
                &frames,
                &mut batch,
                &mut stats,
                &mut visit,
            )?;
        }
        Ok(stats)
    }

    /// Stream sprite and vector state in one heterogeneous display order.
    /// Every batch contains the union of drawable activations across its
    /// active views, with invisible padding in views where an activation is
    /// absent.
    pub fn visit_temporal_scene_batches(
        &mut self,
        timeline: Timeline,
        board: BoardTransform,
        views_per_batch: NonZeroU32,
        mut visit: impl FnMut(&[FrameSample], &TemporalSceneBatch) -> Result<()>,
    ) -> Result<TemporalSceneStats> {
        let capacity = views_per_batch.get() as usize;
        if !(SpritePipeline::MIN_VIEWS_PER_BATCH..=SpritePipeline::MAX_VIEWS_PER_BATCH)
            .contains(&views_per_batch.get())
        {
            return Err(Error::Invalid(
                "temporal scene batch capacity is outside the GPU multiview range".to_owned(),
            ));
        }
        let mut sprite_views = (0..capacity)
            .map(|_| SceneFrameScratch::default())
            .collect::<Vec<_>>();
        let mut vector_views = (0..capacity)
            .map(|_| Vec::<crate::PreparedVector<'a>>::new())
            .collect::<Vec<_>>();
        let mut display_views = (0..capacity)
            .map(|_| Vec::<SceneDisplayEntry>::new())
            .collect::<Vec<_>>();
        let mut frames = Vec::<FrameSample>::with_capacity(capacity);
        let mut active_views = 0usize;
        let mut stats = TemporalSceneStats::default();

        for event in timeline.events() {
            match event? {
                TimelineEvent::ApplyTick { tick, .. } => self.apply_tick(tick)?,
                TimelineEvent::Advance(step) => self.advance(step.duration_seconds)?,
                TimelineEvent::Render(frame) => {
                    self.prepare_gpu_instances(board, &mut sprite_views[active_views])?;
                    vector_views[active_views] = self.prepare_vectors(frame.tick, board)?;
                    display_views[active_views].clear();
                    display_views[active_views].extend_from_slice(self.display_order());
                    frames.push(frame);
                    active_views += 1;
                    stats.frames += 1;
                    if active_views == capacity {
                        emit_temporal_scene_batch(
                            views_per_batch,
                            &sprite_views,
                            &vector_views,
                            &display_views,
                            active_views,
                            &frames,
                            &mut stats,
                            &mut visit,
                        )?;
                        active_views = 0;
                        frames.clear();
                    }
                }
            }
        }
        if active_views != 0 {
            emit_temporal_scene_batch(
                views_per_batch,
                &sprite_views,
                &vector_views,
                &display_views,
                active_views,
                &frames,
                &mut stats,
                &mut visit,
            )?;
        }
        Ok(stats)
    }

    pub const fn action_manager(&self) -> &ActionManagerRuntime {
        &self.action_manager
    }

    fn rebuild_display_order(&mut self) -> Result<()> {
        let mut children = HashMap::<u32, Vec<u32>>::new();
        let mut roots = Vec::<u32>::new();
        for activation in self.action_manager.visible_activation_ids() {
            let Some(node) = self.nodes.get(&activation).copied() else {
                continue;
            };
            match self.action_manager.parent_activation(activation) {
                Some(Some(parent)) => children.entry(parent).or_default().push(activation),
                Some(None) if node.node_id == "__root__" && node.parent_id.is_none() => {
                    roots.push(activation);
                }
                Some(None) => {
                    return Err(Error::Invalid(format!(
                        "non-root scene activation {activation} lacks a parent identity"
                    )));
                }
                None => {
                    return Err(Error::Invalid(format!(
                        "scene activation {activation} lacks parent bookkeeping"
                    )));
                }
            }
        }
        // Stage traversal assigns every root an updateOrder before layer
        // subtrees are expanded. Relative root order is the stable stage
        // zIndex/insertion order.
        roots.sort_by(|left, right| {
            let left_node = self.nodes[left];
            let right_node = self.nodes[right];
            left_node
                .z_index
                .total_cmp(&right_node.z_index)
                .then_with(|| left.cmp(right))
        });

        let mut update_orders = HashMap::<u32, u32>::new();
        let mut next_update_order = 0_u32;
        for root in &roots {
            update_orders.insert(*root, next_update_order);
            next_update_order = next_update_order
                .checked_add(1)
                .ok_or(Error::ArithmeticOverflow)?;
        }

        let layer_count = self.plan.layers.len().max(1);
        let mut pending_entries = vec![Vec::<u32>::new(); layer_count];
        for root in roots {
            let layer = root_layer_order(self.plan, self.nodes[&root], root)?;
            pending_entries[layer as usize].push(root);
        }

        // @pixi/layers finalizes source layers in metadata order. Expanding a
        // sorted entry discovers lifted descendants and assigns their
        // updateOrder; only descendants targeting a later layer become future
        // entries. An explicit assignment to the current layer renders in the
        // ordinary subtree, while an earlier target layer has already closed.
        let mut ordered_entries = vec![Vec::<u32>::new(); layer_count];
        for layer in 0..layer_count {
            pending_entries[layer].sort_by(|left, right| {
                let left_node = self.nodes[left];
                let right_node = self.nodes[right];
                left_node
                    .z_index
                    .total_cmp(&right_node.z_index)
                    .then_with(|| update_orders[left].cmp(&update_orders[right]))
            });
            let entries = std::mem::take(&mut pending_entries[layer]);
            for entry in entries {
                ordered_entries[layer].push(entry);
                LayerDiscovery {
                    current_layer: layer as u32,
                    layer_orders: &self.plan.layer_orders,
                    nodes: &self.nodes,
                    children: &children,
                    pending_entries: &mut pending_entries,
                    update_orders: &mut update_orders,
                    next_update_order: &mut next_update_order,
                }
                .visit(entry)?;
            }
        }

        self.display_order.clear();
        let mut emitted = HashSet::<u32>::new();
        for (layer, entries) in ordered_entries.iter().enumerate() {
            for entry in entries {
                LayerEmitter {
                    current_layer: layer as u32,
                    layer_orders: &self.plan.layer_orders,
                    children: &children,
                    nodes: &self.nodes,
                    emitted: &mut emitted,
                    output: &mut self.display_order,
                }
                .visit(*entry)?;
            }
        }
        self.display_ranks.clear();
        self.display_ranks.extend(
            self.display_order
                .iter()
                .enumerate()
                .map(|(rank, entry)| (entry.activation_order, rank)),
        );
        self.sprite_display_order.clear();
        self.sprite_display_order.extend(
            self.display_order
                .iter()
                .filter(|entry| entry.kind == SceneDrawableKind::Sprite)
                .map(|entry| SpriteDisplayEntry {
                    activation_order: entry.activation_order,
                    layer_order: entry.layer_order,
                }),
        );
        Ok(())
    }

    fn apply_event(&mut self, event: RendererEvent<'_>) -> Result<()> {
        match event.opcode {
            RendererEventOpcode::ObjectCreate => self.create_object(event),
            RendererEventOpcode::ObjectRemove => self.remove_object(event),
            RendererEventOpcode::ProcessorRun => self.run_processor(event),
            RendererEventOpcode::ProcessorDestruct => self.destruct_processor(event),
            RendererEventOpcode::ActionRun => self.run_action(event),
            RendererEventOpcode::ActionFinish => self.finish_action(event),
            RendererEventOpcode::ObjectAlpha => self.apply_object_alpha(event),
            // ReplayIR calculations and processor decisions already include
            // preprocessor effects. Dedicated visual preprocessor adapters are
            // compiled separately.
            RendererEventOpcode::PreprocessorRun => Ok(()),
        }
    }

    fn create_object(&mut self, event: RendererEvent<'_>) -> Result<()> {
        let entity_id = required(event.entity_id, "object:create entity")?;
        if self.active_objects.contains_key(entity_id) {
            return Err(Error::Invalid(format!(
                "animated scene creates active object {entity_id}"
            )));
        }
        let activation = self.activation(event.event_index)?;
        let ResolvedActivation::Object {
            entity_id: activation_entity,
            object_type,
            ..
        } = activation
        else {
            return Err(wrong_activation(event));
        };
        if activation_entity != entity_id {
            return Err(wrong_activation(event));
        }
        let node = self.node(event.event_index)?;
        self.action_manager.create_target(
            event.event_index,
            node.key(),
            node.initial_action_target(),
        )?;
        self.active_objects.insert(
            entity_id.to_owned(),
            ActiveObject {
                object_type: object_type.clone(),
                root_activation: event.event_index,
            },
        );
        Ok(())
    }

    fn remove_object(&mut self, event: RendererEvent<'_>) -> Result<()> {
        let entity_id = required(event.entity_id, "object:remove entity")?;
        let object = self.active_objects.remove(entity_id).ok_or_else(|| {
            Error::Invalid(format!(
                "animated scene removes inactive object {entity_id}"
            ))
        })?;

        let disappear = self.plan.objects[&object.object_type].disappear_processor
            == Some(ProcessorKind::Disappear);
        let action_keys = self
            .active_actions
            .keys()
            .filter(|(entity, _)| entity == entity_id)
            .cloned()
            .collect::<Vec<_>>();
        for key in action_keys {
            let group = self
                .active_actions
                .remove(&key)
                .expect("collected active action");
            if disappear {
                self.action_manager.detach_group(group)?;
            } else {
                self.action_manager.cancel_group(group)?;
            }
        }

        let processor_keys = self
            .active_processors
            .keys()
            .filter(|(entity, _)| entity == entity_id)
            .cloned()
            .collect::<Vec<_>>();
        let mut target_activations = if disappear {
            self.action_manager
                .retire_entity_scope(entity_id, object.root_activation)
        } else {
            vec![object.root_activation]
        };
        if disappear && !target_activations.contains(&object.root_activation) {
            return Err(Error::Invalid(format!(
                "disappearing object {entity_id} lacks its root scope target"
            )));
        }
        for key in processor_keys {
            let processor = self
                .active_processors
                .remove(&key)
                .expect("collected active processor");
            if let Some(group) = processor.action_group {
                if disappear {
                    self.action_manager.detach_group(group)?;
                } else {
                    self.action_manager.cancel_group(group)?;
                }
            }
            if let Some(target) = processor.target_activation
                && !disappear
            {
                self.action_manager.cancel_for_target(target);
                if self.action_manager.target(target).is_some() {
                    self.action_manager.destroy_target(target)?;
                }
            }
        }

        if disappear {
            target_activations.sort_unstable();
            target_activations.dedup();
            let fade = ActionRuntime::from_resolved(&ResolvedActionNode {
                kind: ActionKind::FadeOut,
                params: vec![ResolvedActionParameter::Value(ResolvedValue::Number(
                    self.tick_transition_seconds / 2.0,
                ))],
            })?;
            self.pending_disappears.push(PendingDisappear {
                entity_id: entity_id.to_owned(),
                root_activation: object.root_activation,
                target_activations,
                fade,
            });
            return Ok(());
        }
        self.action_manager
            .cancel_for_target(object.root_activation);
        if self.action_manager.target(object.root_activation).is_some() {
            self.action_manager.destroy_target(object.root_activation)?;
        }
        self.action_manager
            .destroy_entity_scope(entity_id, object.root_activation);
        Ok(())
    }

    fn run_processor(&mut self, event: RendererEvent<'_>) -> Result<()> {
        let entity_id = required(event.entity_id, "processor:run entity")?;
        let definition_id = required(event.semantic_id, "processor:run definition")?;
        let activation = self.activation(event.event_index)?;
        let ResolvedActivation::Processor {
            entity_id: activation_entity,
            definition_id: activation_definition,
            scope_id,
            kind,
            node_id,
            target_is_root,
            touches_node,
            temporary_node,
            actions,
            ..
        } = activation
        else {
            return Err(wrong_activation(event));
        };
        if activation_entity != entity_id || activation_definition != definition_id {
            return Err(wrong_activation(event));
        }
        let key = (entity_id.to_owned(), scope_id.clone());
        if let Some(old) = self.active_processors.remove(&key)
            && let Some(group) = old.action_group
        {
            self.action_manager.detach_group(group)?;
        }
        if *touches_node {
            let node_id = node_id.as_ref().ok_or_else(|| {
                Error::Invalid(format!(
                    "generic processor {definition_id} touches an unresolved node ID"
                ))
            })?;
            self.action_manager.destroy_key(&SceneNodeKey {
                entity_id: entity_id.to_owned(),
                node_id: node_id.clone(),
                is_root: false,
            });
        }

        let node = self.nodes.get(&event.event_index).copied();
        if *kind == ProcessorKind::RunAction {
            let target_key = SceneNodeKey {
                entity_id: entity_id.to_owned(),
                node_id: node_id.clone().unwrap_or_else(|| "__root__".to_owned()),
                is_root: *target_is_root,
            };
            let action_group = self
                .action_manager
                .addressable_activation(&target_key)
                .filter(|_| !actions.is_empty())
                .map(|target| {
                    let group = processor_group_id(event.event_index);
                    self.action_manager.start_group(group, target, actions)?;
                    Ok::<u64, Error>(group)
                })
                .transpose()?;
            self.active_processors.insert(
                key,
                ActiveProcessor {
                    target_activation: None,
                    action_group,
                },
            );
            return Ok(());
        }
        if node.is_none()
            && !actions.is_empty()
            && matches!(
                kind,
                ProcessorKind::Circle
                    | ProcessorKind::Container
                    | ProcessorKind::Draw
                    | ProcessorKind::ResourceCircle
                    | ProcessorKind::SiteProgress
                    | ProcessorKind::Sprite
                    | ProcessorKind::UserBadge
            )
        {
            return Err(Error::Invalid(format!(
                "generic processor {definition_id} resolved actions without a result node"
            )));
        }
        if node.is_none()
            && !actions.is_empty()
            && !matches!(
                kind,
                ProcessorKind::Circle
                    | ProcessorKind::Container
                    | ProcessorKind::Draw
                    | ProcessorKind::ResourceCircle
                    | ProcessorKind::SiteProgress
                    | ProcessorKind::Sprite
                    | ProcessorKind::UserBadge
            )
        {
            return Err(Error::Invalid(format!(
                "processor {} requires its dedicated native action target adapter",
                kind.as_str()
            )));
        }

        let mut target_activation = None;
        let mut action_group = None;
        if let Some(node) = node
            && let Some(parent_activation) = self.parent_activation(node)
        {
            if *temporary_node {
                self.action_manager.create_temporary_target_with_parent(
                    event.event_index,
                    self.active_objects[entity_id].root_activation,
                    node.initial_action_target(),
                    parent_activation,
                )?;
            } else {
                self.action_manager.create_target_with_parent(
                    event.event_index,
                    node.key(),
                    node.initial_action_target(),
                    Some(parent_activation),
                )?;
            }
            target_activation = Some(event.event_index);
            if !actions.is_empty() {
                let group = processor_group_id(event.event_index);
                self.action_manager
                    .start_group(group, event.event_index, actions)?;
                action_group = Some(group);
            }
        }
        self.active_processors.insert(
            key,
            ActiveProcessor {
                target_activation,
                action_group,
            },
        );
        Ok(())
    }

    fn destruct_processor(&mut self, event: RendererEvent<'_>) -> Result<()> {
        let entity_id = required(event.entity_id, "processor:destruct entity")?;
        let definition_id = required(event.semantic_id, "processor:destruct definition")?;
        let active_object = self.active_objects.get(entity_id).ok_or_else(|| {
            Error::Invalid(format!(
                "animated scene destructs processor for inactive object {entity_id}"
            ))
        })?;
        let processor = self.plan.objects[&active_object.object_type]
            .processors
            .iter()
            .find(|processor| processor.definition_id == definition_id)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "animated scene references unknown processor {definition_id}"
                ))
            })?;
        let key = (entity_id.to_owned(), processor.scope_id.clone());
        let Some(active) = self.active_processors.remove(&key) else {
            return Ok(());
        };
        if let Some(target) = active.target_activation
            && self.action_manager.target(target).is_some()
        {
            self.action_manager.hide_target(target)?;
        }
        if let Some(group) = active.action_group {
            self.action_manager.finish_group(group)?;
        }
        Ok(())
    }

    fn run_action(&mut self, event: RendererEvent<'_>) -> Result<()> {
        let entity_id = required(event.entity_id, "action:run entity")?;
        let definition_id = required(event.semantic_id, "action:run definition")?;
        let activation = self.activation(event.event_index)?;
        let ResolvedActivation::Action {
            entity_id: activation_entity,
            definition_id: activation_definition,
            target_id,
            actions,
            ..
        } = activation
        else {
            return Err(wrong_activation(event));
        };
        if activation_entity != entity_id || activation_definition != definition_id {
            return Err(wrong_activation(event));
        }
        let target_key = SceneNodeKey {
            entity_id: entity_id.to_owned(),
            node_id: target_id.clone().unwrap_or_else(|| "__root__".to_owned()),
            is_root: target_id.is_none(),
        };
        let target = self
            .action_manager
            .addressable_activation(&target_key)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "action {definition_id} references unavailable target {}",
                    target_key.node_id
                ))
            })?;
        let key = (entity_id.to_owned(), definition_id.to_owned());
        if self.active_actions.contains_key(&key) {
            return Err(Error::Invalid(format!(
                "action {definition_id} is already active for {entity_id}"
            )));
        }
        let group = u64::from(event.event_index);
        self.action_manager.start_group(group, target, actions)?;
        self.active_actions.insert(key, group);
        Ok(())
    }

    fn finish_action(&mut self, event: RendererEvent<'_>) -> Result<()> {
        let entity_id = required(event.entity_id, "action:finish entity")?;
        let definition_id = required(event.semantic_id, "action:finish definition")?;
        let group = self
            .active_actions
            .remove(&(entity_id.to_owned(), definition_id.to_owned()))
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "action {definition_id} is not active for {entity_id}"
                ))
            })?;
        self.action_manager.finish_group(group)
    }

    fn apply_object_alpha(&mut self, event: RendererEvent<'_>) -> Result<()> {
        let entity_id = required(event.entity_id, "object:alpha entity")?;
        let root = self
            .active_objects
            .get(entity_id)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "animated scene changes alpha for inactive object {entity_id}"
                ))
            })?
            .root_activation;
        self.action_manager
            .target_mut(root)
            .ok_or_else(|| Error::Invalid(format!("object {entity_id} lacks a root target")))?
            .alpha = 0.3;
        Ok(())
    }

    fn activation(&self, event_index: u32) -> Result<&'a ResolvedActivation> {
        self.activations.get(&event_index).copied().ok_or_else(|| {
            Error::Invalid(format!(
                "renderer event {event_index} lacks a resolved activation"
            ))
        })
    }

    fn node(&self, event_index: u32) -> Result<&'a SceneNodeTemplate> {
        self.nodes.get(&event_index).copied().ok_or_else(|| {
            Error::Invalid(format!(
                "renderer event {event_index} lacks a generic scene node"
            ))
        })
    }

    fn parent_activation(&self, node: &SceneNodeTemplate) -> Option<u32> {
        let parent = SceneNodeKey {
            entity_id: node.entity_id.clone(),
            node_id: node
                .parent_id
                .clone()
                .unwrap_or_else(|| "__root__".to_owned()),
            is_root: node.parent_id.is_none(),
        };
        self.action_manager.visible_activation(&parent)
    }
}

fn root_layer_order(plan: &RendererPlan, node: &SceneNodeTemplate, activation: u32) -> Result<u32> {
    let layer_id = node.layer.as_deref().or(plan.default_layer_id.as_deref());
    match layer_id {
        Some(layer_id) => plan.layer_orders.get(layer_id).copied().ok_or_else(|| {
            Error::Invalid(format!(
                "scene activation {activation} references unknown renderer layer {layer_id}"
            ))
        }),
        None if plan.layers.is_empty() => Ok(0),
        None => Err(Error::Invalid(format!(
            "root scene activation {activation} has no renderer layer"
        ))),
    }
}

fn explicit_layer_order(
    layer_orders: &BTreeMap<String, u32>,
    node: &SceneNodeTemplate,
    activation: u32,
) -> Result<Option<u32>> {
    node.layer
        .as_deref()
        .map(|layer_id| {
            layer_orders.get(layer_id).copied().ok_or_else(|| {
                Error::Invalid(format!(
                    "scene activation {activation} references unknown renderer layer {layer_id}"
                ))
            })
        })
        .transpose()
}

struct LayerDiscovery<'a> {
    current_layer: u32,
    layer_orders: &'a BTreeMap<String, u32>,
    nodes: &'a BTreeMap<u32, &'a SceneNodeTemplate>,
    children: &'a HashMap<u32, Vec<u32>>,
    pending_entries: &'a mut [Vec<u32>],
    update_orders: &'a mut HashMap<u32, u32>,
    next_update_order: &'a mut u32,
}

impl LayerDiscovery<'_> {
    fn visit(&mut self, activation: u32) -> Result<()> {
        let child_count = self.children.get(&activation).map_or(0, Vec::len);
        for index in 0..child_count {
            let child = self.children[&activation][index];
            if self
                .update_orders
                .insert(child, *self.next_update_order)
                .is_some()
            {
                return Err(Error::Invalid(
                    "visible scene graph contains a parent cycle".to_owned(),
                ));
            }
            *self.next_update_order = self
                .next_update_order
                .checked_add(1)
                .ok_or(Error::ArithmeticOverflow)?;
            let target_layer = explicit_layer_order(self.layer_orders, self.nodes[&child], child)?
                .unwrap_or(self.current_layer);
            if target_layer == self.current_layer {
                self.visit(child)?;
            } else if target_layer > self.current_layer {
                self.pending_entries[target_layer as usize].push(child);
            }
        }
        Ok(())
    }
}

struct LayerEmitter<'a> {
    current_layer: u32,
    layer_orders: &'a BTreeMap<String, u32>,
    children: &'a HashMap<u32, Vec<u32>>,
    nodes: &'a BTreeMap<u32, &'a SceneNodeTemplate>,
    emitted: &'a mut HashSet<u32>,
    output: &'a mut Vec<SceneDisplayEntry>,
}

impl LayerEmitter<'_> {
    fn visit(&mut self, activation: u32) -> Result<()> {
        if !self.emitted.insert(activation) {
            return Ok(());
        }
        let node = self.nodes[&activation];
        let kind = match node.kind {
            crate::SceneNodeKind::Sprite { .. } => Some(SceneDrawableKind::Sprite),
            crate::SceneNodeKind::Vector { .. } => Some(SceneDrawableKind::Vector),
            crate::SceneNodeKind::Container => None,
        };
        if let Some(kind) = kind {
            self.output.push(SceneDisplayEntry {
                activation_order: activation,
                layer_order: self.current_layer,
                kind,
            });
        }
        let child_count = self.children.get(&activation).map_or(0, Vec::len);
        for index in 0..child_count {
            let child = self.children[&activation][index];
            let target_layer = explicit_layer_order(self.layer_orders, self.nodes[&child], child)?
                .unwrap_or(self.current_layer);
            if target_layer == self.current_layer {
                self.visit(child)?;
            }
        }
        Ok(())
    }
}

fn emit_temporal_batch(
    views_per_batch: NonZeroU32,
    view_scratches: &[SceneFrameScratch],
    active_views: usize,
    frames: &[FrameSample],
    batch: &mut TemporalSpriteBatch,
    stats: &mut TemporalSceneStats,
    visit: &mut impl FnMut(&[FrameSample], &TemporalSpriteBatch) -> Result<()>,
) -> Result<()> {
    if active_views != frames.len() || active_views > view_scratches.len() {
        return Err(Error::Invalid(
            "temporal scene batch bookkeeping is inconsistent".to_owned(),
        ));
    }
    let mut views = [&[][..]; SpritePipeline::MAX_VIEWS_PER_BATCH as usize];
    for (index, scratch) in view_scratches[..active_views].iter().enumerate() {
        views[index] = scratch.instances();
    }
    batch.repack(views_per_batch, &views[..active_views])?;
    stats.batches += 1;
    stats.max_instances_per_view = stats.max_instances_per_view.max(batch.instances_per_view);
    visit(frames, batch)
}

#[allow(clippy::too_many_arguments)]
fn emit_temporal_scene_batch(
    views_per_batch: NonZeroU32,
    sprite_views: &[SceneFrameScratch],
    vector_views: &[Vec<crate::PreparedVector<'_>>],
    display_views: &[Vec<SceneDisplayEntry>],
    active_views: usize,
    frames: &[FrameSample],
    stats: &mut TemporalSceneStats,
    visit: &mut impl FnMut(&[FrameSample], &TemporalSceneBatch) -> Result<()>,
) -> Result<()> {
    if active_views == 0
        || active_views != frames.len()
        || active_views > sprite_views.len()
        || active_views > vector_views.len()
        || active_views > display_views.len()
    {
        return Err(Error::Invalid(
            "heterogeneous temporal scene batch bookkeeping is inconsistent".to_owned(),
        ));
    }
    let sprite_slices = sprite_views[..active_views]
        .iter()
        .map(SceneFrameScratch::instances)
        .collect::<Vec<_>>();
    let vector_slices = vector_views[..active_views]
        .iter()
        .map(Vec::as_slice)
        .collect::<Vec<_>>();
    let display_slices = display_views[..active_views]
        .iter()
        .map(Vec::as_slice)
        .collect::<Vec<_>>();
    let sprites = TemporalSpriteBatch::pack(views_per_batch, &sprite_slices)?;
    let vectors = TemporalVectorBatch::pack(views_per_batch, &vector_slices)?;
    let display_order = merge_temporal_display_order(&display_slices)?;

    // Slot/order agreement is an internal invariant of pack + merge. Keep the
    // O(n log n) set equality check on debug builds only; release hot path pays
    // pack/merge only.
    #[cfg(debug_assertions)]
    {
        let expected_sprites = display_order
            .iter()
            .filter(|entry| entry.kind == SceneDrawableKind::Sprite)
            .map(|entry| entry.activation_order)
            .collect::<BTreeSet<_>>();
        let actual_sprites = sprites
            .slot_activations
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let expected_vectors = display_order
            .iter()
            .filter(|entry| entry.kind == SceneDrawableKind::Vector)
            .map(|entry| entry.activation_order)
            .collect::<BTreeSet<_>>();
        let actual_vectors = vectors
            .slot_activations
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        if expected_sprites != actual_sprites || expected_vectors != actual_vectors {
            return Err(Error::Invalid(
                "heterogeneous display order and packed drawable slots disagree".to_owned(),
            ));
        }
    }

    stats.batches += 1;
    stats.max_instances_per_view = stats.max_instances_per_view.max(sprites.instances_per_view);
    stats.max_vectors_per_view = stats.max_vectors_per_view.max(vectors.instances_per_view);
    stats.max_vector_vertices_per_batch = stats
        .max_vector_vertices_per_batch
        .max(vectors.referenced_vertex_count());
    visit(
        frames,
        &TemporalSceneBatch {
            sprites,
            vectors,
            display_order,
        },
    )
}

fn merge_temporal_display_order(views: &[&[SceneDisplayEntry]]) -> Result<Vec<SceneDisplayEntry>> {
    let mut identities = BTreeMap::<u32, SceneDisplayEntry>::new();
    let mut edges = BTreeMap::<u32, BTreeSet<u32>>::new();
    let mut indegrees = BTreeMap::<u32, u32>::new();
    let mut seen = BTreeSet::new();
    for view in views {
        seen.clear();
        for entry in *view {
            if !seen.insert(entry.activation_order) {
                return Err(Error::Invalid(
                    "temporal display view repeats a drawable activation".to_owned(),
                ));
            }
            if let Some(existing) = identities.insert(entry.activation_order, *entry)
                && existing != *entry
            {
                return Err(Error::Invalid(format!(
                    "drawable activation {} changes kind or layer across temporal views",
                    entry.activation_order
                )));
            }
            edges.entry(entry.activation_order).or_default();
            indegrees.entry(entry.activation_order).or_default();
        }
        for adjacent in view.windows(2) {
            let before = adjacent[0].activation_order;
            let after = adjacent[1].activation_order;
            if edges.entry(before).or_default().insert(after) {
                let indegree = indegrees.entry(after).or_default();
                *indegree = indegree.checked_add(1).ok_or(Error::ArithmeticOverflow)?;
            }
        }
    }

    let mut ready = indegrees
        .iter()
        .filter_map(|(activation, indegree)| (*indegree == 0).then_some(*activation))
        .collect::<BTreeSet<_>>();
    let mut output = Vec::with_capacity(identities.len());
    while let Some(activation) = ready.pop_first() {
        output.push(identities[&activation]);
        for after in &edges[&activation] {
            let indegree = indegrees
                .get_mut(after)
                .expect("edge target has an indegree");
            *indegree -= 1;
            if *indegree == 0 {
                ready.insert(*after);
            }
        }
    }
    if output.len() != identities.len() {
        return Err(Error::Invalid(
            "heterogeneous display order changes incompatibly across temporal views".to_owned(),
        ));
    }
    Ok(output)
}

fn activation_order(activation: &ResolvedActivation) -> u32 {
    match activation {
        ResolvedActivation::Object {
            activation_order, ..
        }
        | ResolvedActivation::Processor {
            activation_order, ..
        }
        | ResolvedActivation::Action {
            activation_order, ..
        } => *activation_order,
    }
}

fn processor_group_id(event_index: u32) -> u64 {
    PROCESSOR_ACTION_GROUP_NAMESPACE | u64::from(event_index)
}

fn required<'a>(value: Option<&'a str>, label: &str) -> Result<&'a str> {
    value.ok_or_else(|| Error::Invalid(format!("renderer event lacks {label}")))
}

fn wrong_activation(event: RendererEvent<'_>) -> Error {
    Error::Invalid(format!(
        "renderer event {} {:?} disagrees with its resolved activation",
        event.event_index, event.opcode
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;

    use serde_json::{Value, json};

    use crate::artifact::tests::{artifact_json, signed};
    use crate::{
        AtlasEntry, BoardTransform, GenericSceneRuntime, RendererPlan, ReplayArtifact,
        ResolvedScene, SceneDisplayEntry, SceneDrawableKind, SceneNodeTemplates, SceneSchedule,
        TextureAtlas, TextureAtlasPage, Timeline, TimelineEvent,
    };

    use super::merge_temporal_display_order;

    #[test]
    fn temporal_display_union_preserves_cross_kind_edges_and_rejects_cycles() {
        let sprite_a = SceneDisplayEntry {
            activation_order: 1,
            layer_order: 0,
            kind: SceneDrawableKind::Sprite,
        };
        let vector = SceneDisplayEntry {
            activation_order: 2,
            layer_order: 0,
            kind: SceneDrawableKind::Vector,
        };
        let sprite_b = SceneDisplayEntry {
            activation_order: 3,
            layer_order: 0,
            kind: SceneDrawableKind::Sprite,
        };
        let first = [sprite_a, vector, sprite_b];
        let second = [sprite_a, sprite_b];
        assert_eq!(
            merge_temporal_display_order(&[&first, &second]).unwrap(),
            first
        );

        let reversed = [sprite_b, vector, sprite_a];
        assert!(merge_temporal_display_order(&[&first, &reversed]).is_err());
    }

    fn unit_atlas() -> TextureAtlas {
        TextureAtlas {
            entries: BTreeMap::from([(
                "unit".to_owned(),
                AtlasEntry {
                    page: 0,
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    logical_width: 100.0,
                    logical_height: 100.0,
                    u_min: 0.0,
                    v_min: 0.0,
                    u_max: 1.0,
                    v_max: 1.0,
                },
            )]),
            pages: vec![TextureAtlasPage {
                width: 1,
                height: 1,
                rgba: vec![255; 4],
            }],
            padding: 1,
        }
    }

    fn board() -> BoardTransform {
        BoardTransform {
            zoom: 1.0,
            position: [0.0, 0.0],
            pivot: [0.0, 0.0],
        }
    }

    #[test]
    fn lifecycle_and_fixed_steps_drive_generic_sprite_actions_into_frames() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"] = json!({
            "actions": [],
            "calculations": [],
            "data": {},
            "processors": [{
                "actions": [{"action": "AlphaTo", "params": [0, 1]}],
                "id": "body",
                "payload": {"texture": "unit"},
                "type": "sprite"
            }],
            "texture": "unit"
        });
        root["rendererContract"]["inventory"]["actionTypes"] = json!(["AlphaTo"]);
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["sprite"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 2], ["unit"], [], []]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [[0, 0], [3, 7], [-1, 0], [-1, -1]],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 2, 2],
            "payloads": [],
            "semanticIds": ["auto:$.objects.unit.processors[0]"]
        });
        root["replay"]["timeline"] = json!({
            "framesPerSecond": "2",
            "substepsPerSecond": "2",
            "tickTransitionSeconds": "1",
            "ticksPerSecond": "1"
        });
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        let schedule = SceneSchedule::compile(&artifact, &plan).unwrap();
        let scene = ResolvedScene::compile(&artifact, &plan, &schedule).unwrap();
        let atlas = unit_atlas();
        let templates = SceneNodeTemplates::compile(&scene, &atlas).unwrap();
        let timeline = Timeline::from_replay(&artifact.replay).unwrap();
        let mut runtime = GenericSceneRuntime::new(&artifact, &plan, &scene, &templates).unwrap();
        let mut alphas = Vec::new();
        for event in timeline.events() {
            match event.unwrap() {
                TimelineEvent::ApplyTick { tick, .. } => runtime.apply_tick(tick).unwrap(),
                TimelineEvent::Advance(step) => runtime.advance(step.duration_seconds).unwrap(),
                TimelineEvent::Render(frame) => {
                    let sprites = runtime.prepare(frame.tick, board()).unwrap();
                    assert_eq!(sprites.len(), 1);
                    alphas.push(sprites[0].alpha);
                }
            }
        }
        assert_eq!(alphas, [1.0, 0.5, 0.0]);

        let mut runtime = GenericSceneRuntime::new(&artifact, &plan, &scene, &templates).unwrap();
        let mut packed_alphas = Vec::new();
        let stats = runtime
            .visit_temporal_batches(
                timeline,
                board(),
                NonZeroU32::new(2).unwrap(),
                |frames, batch| {
                    assert_eq!(frames.len(), batch.active_views.get() as usize);
                    for view in 0..frames.len() {
                        packed_alphas.push(
                            batch.instances[view * batch.instances_per_view as usize].tint_alpha[3],
                        );
                    }
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(packed_alphas, [1.0, 0.5, 0.0]);
        assert_eq!(stats.frames, 3);
        assert_eq!(stats.batches, 2);
        assert_eq!(stats.max_instances_per_view, 1);
    }

    #[test]
    fn run_action_processor_targets_an_existing_scope_node_without_an_adapter() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"] = json!({
            "actions": [],
            "calculations": [],
            "data": {},
            "processors": [{
                "id": "body",
                "payload": {"id": 7, "texture": "unit"},
                "type": "sprite"
            }, {
                "actions": [{"action": "AlphaTo", "params": [0, 1]}],
                "id": "body-fade",
                "payload": {"id": 7},
                "type": "runAction"
            }],
            "texture": "unit"
        });
        root["rendererContract"]["inventory"]["actionTypes"] = json!(["AlphaTo"]);
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["runAction", "sprite"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 2], ["unit"], [], []]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [[0, 0, 0], [3, 7, 7], [-1, 0, 1], [-1, -1, -1]],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 3, 3],
            "payloads": [],
            "semanticIds": [
                "auto:$.objects.unit.processors[0]",
                "auto:$.objects.unit.processors[1]"
            ]
        });
        root["replay"]["timeline"] = json!({
            "framesPerSecond": "2",
            "substepsPerSecond": "2",
            "tickTransitionSeconds": "1",
            "ticksPerSecond": "1"
        });
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        let schedule = SceneSchedule::compile(&artifact, &plan).unwrap();
        let scene = ResolvedScene::compile(&artifact, &plan, &schedule).unwrap();
        let atlas = unit_atlas();
        let templates = SceneNodeTemplates::compile(&scene, &atlas).unwrap();
        let timeline = Timeline::from_replay(&artifact.replay).unwrap();
        let mut runtime = GenericSceneRuntime::new(&artifact, &plan, &scene, &templates).unwrap();
        let mut alphas = Vec::new();
        for event in timeline.events() {
            match event.unwrap() {
                TimelineEvent::ApplyTick { tick, .. } => runtime.apply_tick(tick).unwrap(),
                TimelineEvent::Advance(step) => runtime.advance(step.duration_seconds).unwrap(),
                TimelineEvent::Render(frame) => {
                    let sprites = runtime.prepare(frame.tick, board()).unwrap();
                    assert_eq!(sprites.len(), 1);
                    alphas.push(sprites[0].alpha);
                }
            }
        }
        assert_eq!(alphas, [1.0, 0.5, 0.0]);
    }

    #[test]
    fn disappear_cleanup_does_not_leak_a_destructed_scope_into_a_reused_entity_id() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"] = json!({
            "actions": [],
            "calculations": [],
            "data": {},
            "disappearProcessor": {"type": "disappear"},
            "processors": [{
                "id": "body",
                "payload": {"texture": "unit"},
                "type": "sprite"
            }, {
                "actions": [{"action": "AlphaTo", "params": [0, 1]}],
                "id": "body-fade",
                "payload": {"id": "body"},
                "type": "runAction"
            }],
            "texture": "unit"
        });
        root["rendererContract"]["inventory"]["actionTypes"] = json!(["AlphaTo"]);
        root["rendererContract"]["inventory"]["processorTypes"] =
            json!(["disappear", "runAction", "sprite"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();
        root["replay"]["totalTicks"] = json!(2);
        root["replay"]["entities"][0]["lifetimes"] = json!([[0, 1], [2, 3]]);
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 3], ["unit"], [], []]);
        root["replay"]["objectOrder"] = json!([[0, 3], [["one"]], [], []]);
        root["replay"]["visualOverlay"]["states"] = json!([[0, 3], [[]], [], []]);
        root["replay"]["timeline"]["tickTransitionSeconds"] = json!("1");
        root["replay"]["rendererGraph"] = json!({
            "columns": [
                [0, 0, 0, 0, 0, 0],
                [3, 7, 6, 4, 3, 7],
                [-1, 0, 0, -1, -1, 1],
                [-1, -1, -1, -1, -1, -1]
            ],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 2, 4, 6],
            "payloads": [],
            "semanticIds": [
                "auto:$.objects.unit.processors[0]",
                "auto:$.objects.unit.processors[1]"
            ]
        });
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        let schedule = SceneSchedule::compile(&artifact, &plan).unwrap();
        let scene = ResolvedScene::compile(&artifact, &plan, &schedule).unwrap();
        let templates = SceneNodeTemplates::compile(&scene, &unit_atlas()).unwrap();
        let mut runtime = GenericSceneRuntime::new(&artifact, &plan, &scene, &templates).unwrap();
        let body_key = crate::SceneNodeKey {
            entity_id: "one".to_owned(),
            node_id: "body".to_owned(),
            is_root: false,
        };
        let root_key = crate::SceneNodeKey {
            entity_id: "one".to_owned(),
            node_id: "__root__".to_owned(),
            is_root: true,
        };

        runtime.apply_tick(0).unwrap();
        assert!(
            runtime
                .action_manager()
                .addressable_activation(&body_key)
                .is_some()
        );
        runtime.apply_tick(1).unwrap();
        assert_eq!(
            runtime.action_manager().addressable_activation(&body_key),
            None
        );
        runtime.apply_tick(2).unwrap();
        assert_eq!(
            runtime.action_manager().addressable_activation(&root_key),
            Some(4)
        );
        runtime.advance(0.5).unwrap();
        assert_eq!(
            runtime.action_manager().addressable_activation(&root_key),
            Some(4)
        );
        assert_eq!(
            runtime.action_manager().addressable_activation(&body_key),
            None
        );
    }

    #[test]
    fn disappear_processor_fades_the_retained_subtree_before_destroying_it() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"] = json!({
            "actions": [],
            "calculations": [],
            "data": {},
            "disappearProcessor": {"type": "disappear"},
            "processors": [{
                "id": "body",
                "payload": {"texture": "unit"},
                "type": "sprite"
            }],
            "texture": "unit"
        });
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["disappear", "sprite"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();
        root["replay"]["entities"][0]["lifetimes"] = json!([[0, 1]]);
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 1], ["unit"], [], []]);
        root["replay"]["timeline"]["tickTransitionSeconds"] = json!("1");
        root["replay"]["rendererGraph"] = json!({
            "columns": [[0, 0, 0], [3, 7, 4], [-1, 0, -1], [-1, -1, -1]],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 2, 3],
            "payloads": [],
            "semanticIds": ["auto:$.objects.unit.processors[0]"]
        });
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        let schedule = SceneSchedule::compile(&artifact, &plan).unwrap();
        let scene = ResolvedScene::compile(&artifact, &plan, &schedule).unwrap();
        let atlas = unit_atlas();
        let templates = SceneNodeTemplates::compile(&scene, &atlas).unwrap();
        let mut runtime = GenericSceneRuntime::new(&artifact, &plan, &scene, &templates).unwrap();

        runtime.apply_tick(0).unwrap();
        assert_eq!(runtime.prepare(0, board()).unwrap()[0].alpha, 1.0);
        runtime.apply_tick(1).unwrap();
        assert_eq!(runtime.prepare(1, board()).unwrap()[0].alpha, 1.0);
        runtime.advance(0.25).unwrap();
        assert_eq!(runtime.prepare(1, board()).unwrap()[0].alpha, 0.5);
        runtime.advance(0.25).unwrap();
        assert!(runtime.prepare(1, board()).unwrap().is_empty());
        assert!(
            runtime.display_order().is_empty(),
            "completed disappear cleanup must invalidate cached temporal display order"
        );
    }

    #[test]
    fn generic_helper_deletes_a_shared_node_across_processor_scopes() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["objects"]["unit"] = json!({
            "actions": [],
            "calculations": [],
            "data": {},
            "processors": [{
                "id": "first-scope",
                "payload": {"id": "body", "texture": "unit"},
                "type": "sprite"
            }, {
                "id": "second-scope",
                "payload": {
                    "id": "body",
                    "shouldCreate": false,
                    "texture": "unit"
                },
                "type": "sprite"
            }]
        });
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["sprite"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 2], ["unit"], [], []]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [[0, 0, 0], [3, 7, 7], [-1, 0, 1], [-1, -1, -1]],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 2, 3],
            "payloads": [],
            "semanticIds": [
                "auto:$.objects.unit.processors[0]",
                "auto:$.objects.unit.processors[1]"
            ]
        });
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        let schedule = SceneSchedule::compile(&artifact, &plan).unwrap();
        let scene = ResolvedScene::compile(&artifact, &plan, &schedule).unwrap();
        let templates = SceneNodeTemplates::compile(&scene, &unit_atlas()).unwrap();
        let mut runtime = GenericSceneRuntime::new(&artifact, &plan, &scene, &templates).unwrap();

        runtime.apply_tick(0).unwrap();
        assert_eq!(runtime.prepare(0, board()).unwrap().len(), 1);
        runtime.apply_tick(1).unwrap();
        assert!(runtime.prepare(1, board()).unwrap().is_empty());
    }

    #[test]
    fn mixed_display_order_matches_layer_z_and_unlifted_child_insertion() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["layers"] = json!([
            {"id": "objects", "isDefault": true},
            {"id": "effects"}
        ]);
        root["rendererContract"]["metadata"]["objects"]["unit"] = json!({
            "actions": [],
            "calculations": [],
            "data": {},
            "processors": [{
                "id": "child-a",
                "payload": {"texture": "unit", "tint": 1},
                "type": "sprite",
                "zIndex": 99
            }, {
                "id": "effect-low",
                "layer": "effects",
                "payload": {"texture": "unit", "tint": 2},
                "type": "sprite",
                "zIndex": 1
            }, {
                "id": "child-b",
                "payload": {
                    "drawings": [{
                        "method": "beginFill",
                        "params": [16777215]
                    }, {
                        "method": "drawRect",
                        "params": [0, 0, 10, 10]
                    }],
                    "tint": 3
                },
                "type": "draw",
                "zIndex": -99
            }, {
                "id": "effect-high",
                "layer": "effects",
                "payload": {"texture": "unit", "tint": 4},
                "type": "sprite",
                "zIndex": 0
            }]
        });
        root["rendererContract"]["inventory"]["layerIds"] = json!(["effects", "objects"]);
        root["rendererContract"]["inventory"]["drawingMethods"] = json!(["beginFill", "drawRect"]);
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["draw", "sprite"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();
        root["replay"]["entities"][0]["properties"]["type"] = json!([[0, 2], ["unit"], [], []]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [
                [0, 0, 0, 0, 0],
                [3, 7, 7, 7, 7],
                [-1, 0, 1, 2, 3],
                [-1, -1, -1, -1, -1]
            ],
            "enabled": true,
            "entityIds": ["one"],
            "offsets": [0, 5, 5],
            "payloads": [],
            "semanticIds": [
                "auto:$.objects.unit.processors[0]",
                "auto:$.objects.unit.processors[1]",
                "auto:$.objects.unit.processors[2]",
                "auto:$.objects.unit.processors[3]"
            ]
        });
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        let schedule = SceneSchedule::compile(&artifact, &plan).unwrap();
        let scene = ResolvedScene::compile(&artifact, &plan, &schedule).unwrap();
        let templates = SceneNodeTemplates::compile(&scene, &unit_atlas()).unwrap();
        let mut runtime = GenericSceneRuntime::new(&artifact, &plan, &scene, &templates).unwrap();

        runtime.apply_tick(0).unwrap();
        let tints = runtime
            .prepare(0, board())
            .unwrap()
            .iter()
            .map(|sprite| sprite.tint)
            .collect::<Vec<_>>();
        assert_eq!(tints, [1, 4, 2]);
        assert_eq!(
            runtime
                .prepare_vectors(0, board())
                .unwrap()
                .iter()
                .map(|vector| vector.tint)
                .collect::<Vec<_>>(),
            [3]
        );
        assert_eq!(
            runtime
                .display_order()
                .iter()
                .map(|entry| (entry.activation_order, entry.kind))
                .collect::<Vec<_>>(),
            [
                (1, SceneDrawableKind::Sprite),
                (3, SceneDrawableKind::Vector),
                (4, SceneDrawableKind::Sprite),
                (2, SceneDrawableKind::Sprite),
            ]
        );

        let mut streamed = GenericSceneRuntime::new(&artifact, &plan, &scene, &templates).unwrap();
        let mut batch_count = 0;
        streamed
            .visit_temporal_scene_batches(
                Timeline::from_replay(&artifact.replay).unwrap(),
                board(),
                NonZeroU32::new(2).unwrap(),
                |_frames, batch| {
                    batch_count += 1;
                    assert_eq!(batch.sprites.instances_per_view, 3);
                    assert_eq!(batch.vectors.instances_per_view, 1);
                    assert_eq!(
                        batch
                            .display_order
                            .iter()
                            .map(|entry| (entry.activation_order, entry.kind))
                            .collect::<Vec<_>>(),
                        [
                            (1, SceneDrawableKind::Sprite),
                            (3, SceneDrawableKind::Vector),
                            (4, SceneDrawableKind::Sprite),
                            (2, SceneDrawableKind::Sprite),
                        ]
                    );
                    Ok(())
                },
            )
            .unwrap();
        assert!(batch_count > 0);
    }

    #[test]
    fn lifted_ties_follow_source_layer_discovery_order() {
        let mut root: Value = serde_json::from_slice(&artifact_json()).unwrap();
        root["rendererContract"]["metadata"]["layers"] = json!([
            {"id": "terrain"},
            {"id": "objects", "isDefault": true},
            {"id": "effects"}
        ]);
        root["rendererContract"]["metadata"]["objects"] = json!({
            "a": {
                "actions": [],
                "calculations": [],
                "data": {},
                "layer": "terrain",
                "processors": [{
                    "id": "effect",
                    "layer": "effects",
                    "payload": {"texture": "unit", "tint": 1},
                    "type": "sprite",
                    "zIndex": 0
                }]
            },
            "b": {
                "actions": [],
                "calculations": [],
                "data": {},
                "processors": [{
                    "id": "effect",
                    "layer": "effects",
                    "payload": {"texture": "unit", "tint": 2},
                    "type": "sprite",
                    "zIndex": 0
                }]
            }
        });
        root["rendererContract"]["inventory"]["objectTypes"] = json!(["a", "b"]);
        root["rendererContract"]["inventory"]["layerIds"] =
            json!(["effects", "objects", "terrain"]);
        root["rendererContract"]["inventory"]["processorTypes"] = json!(["sprite"]);
        root["rendererContract"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["rendererContract"] = signed(root["rendererContract"].take());
        root["replay"]["rendererContractFingerprint"] =
            root["rendererContract"]["fingerprint"].clone();

        let mut first = root["replay"]["entities"][0].take();
        first["properties"]["type"] = json!([[0, 2], ["b"], [], []]);
        let mut second = first.clone();
        second["id"] = json!("two");
        second["properties"]["type"] = json!([[0, 2], ["a"], [], []]);
        root["replay"]["entities"] = Value::Array(vec![first, second]);
        root["replay"]["objectOrder"] = json!([[0, 2], [["one", "two"]], [], []]);
        root["replay"]["rendererGraph"] = json!({
            "columns": [
                [0, 0, 1, 1],
                [3, 7, 3, 7],
                [-1, 1, -1, 0],
                [-1, -1, -1, -1]
            ],
            "enabled": true,
            "entityIds": ["one", "two"],
            "offsets": [0, 4, 4],
            "payloads": [],
            "semanticIds": [
                "auto:$.objects.a.processors[0]",
                "auto:$.objects.b.processors[0]"
            ]
        });
        root["replay"]
            .as_object_mut()
            .unwrap()
            .remove("fingerprint");
        root["replay"] = signed(root["replay"].take());

        let artifact = ReplayArtifact::from_slice(&serde_json::to_vec(&root).unwrap()).unwrap();
        let plan = RendererPlan::compile(&artifact.renderer_contract).unwrap();
        let schedule = SceneSchedule::compile(&artifact, &plan).unwrap();
        let scene = ResolvedScene::compile(&artifact, &plan, &schedule).unwrap();
        let templates = SceneNodeTemplates::compile(&scene, &unit_atlas()).unwrap();
        let mut runtime = GenericSceneRuntime::new(&artifact, &plan, &scene, &templates).unwrap();

        runtime.apply_tick(0).unwrap();
        let tints = runtime
            .prepare(0, board())
            .unwrap()
            .iter()
            .map(|sprite| sprite.tint)
            .collect::<Vec<_>>();
        assert_eq!(tints, [1, 2]);
    }
}
