use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use bytemuck::Zeroable;

use crate::{
    Error, PreparedSpriteInstance, Result, SpriteBlendMode, SpriteDrawRun, SpriteInstance,
    SpritePipeline,
};

#[derive(Clone, Debug)]
pub struct TemporalSpriteBatch {
    pub active_views: NonZeroU32,
    pub instances_per_view: u32,
    pub instances: Vec<SpriteInstance>,
    pub draw_runs: Vec<SpriteDrawRun>,
    pub(crate) slot_activations: Vec<u32>,
    validation_activations: BTreeSet<u32>,
}

impl TemporalSpriteBatch {
    pub(crate) fn empty() -> Self {
        Self {
            active_views: NonZeroU32::MIN,
            instances_per_view: 0,
            instances: Vec::new(),
            draw_runs: Vec::new(),
            slot_activations: Vec::new(),
            validation_activations: BTreeSet::new(),
        }
    }

    /// Pack independently prepared timestamps into the fixed view-major layout
    /// consumed by the multiview shader. Slots are the union of activations in
    /// this microbatch; absent and inactive-view slots are zeroed.
    pub fn pack(views_per_batch: NonZeroU32, views: &[&[PreparedSpriteInstance]]) -> Result<Self> {
        let mut batch = Self::empty();
        batch.repack(views_per_batch, views)?;
        Ok(batch)
    }

    /// Repack into retained instance/run storage. This is the streaming hot
    /// path; capacities survive across temporal microbatches.
    pub fn repack(
        &mut self,
        views_per_batch: NonZeroU32,
        views: &[&[PreparedSpriteInstance]],
    ) -> Result<()> {
        let capacity = views_per_batch.get();
        if !(SpritePipeline::MIN_VIEWS_PER_BATCH..=SpritePipeline::MAX_VIEWS_PER_BATCH)
            .contains(&capacity)
        {
            return Err(Error::Invalid(format!(
                "temporal sprite batches require {} to {} configured views",
                SpritePipeline::MIN_VIEWS_PER_BATCH,
                SpritePipeline::MAX_VIEWS_PER_BATCH
            )));
        }
        let active_views =
            NonZeroU32::new(u32::try_from(views.len()).map_err(|_| Error::ArithmeticOverflow)?)
                .ok_or_else(|| {
                    Error::Invalid(
                        "temporal sprite batch requires at least one active view".to_owned(),
                    )
                })?;
        if active_views.get() > capacity {
            return Err(Error::Invalid(
                "active temporal sprite views exceed configured capacity".to_owned(),
            ));
        }

        let mut same_layout = true;
        for view in views {
            for (index, prepared) in view.iter().enumerate() {
                if let Some(reference) = views[0].get(index) {
                    same_layout &= reference.activation_order == prepared.activation_order
                        && reference.layer_order == prepared.layer_order
                        && reference.blend_mode == prepared.blend_mode
                        && reference.instance.has_blur_filter == prepared.instance.has_blur_filter;
                } else {
                    same_layout = false;
                }
            }
            same_layout &= view.len() == views[0].len();
        }
        if same_layout {
            validate_unique_activations(&mut self.validation_activations, views[0])?;
            return repack_same_layout(self, views_per_batch, active_views, views);
        }

        let mut slots = BTreeMap::<u32, (u32, SpriteBlendMode, bool)>::new();
        for view in views {
            validate_unique_activations(&mut self.validation_activations, view)?;
            for prepared in *view {
                let identity = (
                    prepared.layer_order,
                    prepared.blend_mode,
                    prepared.instance.has_blur_filter != 0,
                );
                if let Some(existing) = slots.insert(prepared.activation_order, identity)
                    && existing != identity
                {
                    return Err(Error::Invalid(format!(
                        "sprite activation {} changes layer, blend mode, or filter identity across temporal views",
                        prepared.activation_order
                    )));
                }
            }
        }
        let mut edges = slots
            .keys()
            .map(|activation| (*activation, BTreeSet::<u32>::new()))
            .collect::<BTreeMap<_, _>>();
        let mut indegrees = slots
            .keys()
            .map(|activation| (*activation, 0_u32))
            .collect::<BTreeMap<_, _>>();
        for view in views {
            for adjacent in view.windows(2) {
                let before = adjacent[0].activation_order;
                let after = adjacent[1].activation_order;
                if edges
                    .get_mut(&before)
                    .expect("union contains every view activation")
                    .insert(after)
                {
                    let indegree = indegrees
                        .get_mut(&after)
                        .expect("union contains every view activation");
                    *indegree = indegree.checked_add(1).ok_or(Error::ArithmeticOverflow)?;
                }
            }
        }
        let mut ready = indegrees
            .iter()
            .filter_map(|(activation, indegree)| (*indegree == 0).then_some(*activation))
            .collect::<BTreeSet<_>>();
        let mut slot_order = Vec::with_capacity(slots.len());
        while let Some(activation) = ready.pop_first() {
            slot_order.push(activation);
            for after in &edges[&activation] {
                let indegree = indegrees
                    .get_mut(after)
                    .expect("union contains every display-order edge");
                *indegree -= 1;
                if *indegree == 0 {
                    ready.insert(*after);
                }
            }
        }
        if slot_order.len() != slots.len() {
            return Err(Error::Invalid(
                "sprite display order changes incompatibly across temporal views".to_owned(),
            ));
        }

        let slot_count = u32::try_from(slot_order.len()).map_err(|_| Error::ArithmeticOverflow)?;
        let total_instances = (capacity as usize)
            .checked_mul(slot_order.len())
            .ok_or(Error::ArithmeticOverflow)?;
        self.instances.clear();
        self.instances.reserve(total_instances);
        for view_index in 0..capacity as usize {
            let view = views.get(view_index).copied().unwrap_or_default();
            let prepared = view
                .iter()
                .map(|sprite| (sprite.activation_order, sprite.instance))
                .collect::<BTreeMap<_, _>>();
            for activation in &slot_order {
                let instance = prepared
                    .get(activation)
                    .copied()
                    .unwrap_or_else(SpriteInstance::zeroed);
                self.instances.push(instance);
            }
        }

        compile_draw_runs_into(
            &mut self.draw_runs,
            slot_order.iter().map(|activation| slots[activation]),
        )?;
        self.slot_activations.clear();
        self.slot_activations.extend(slot_order);
        self.active_views = active_views;
        self.instances_per_view = slot_count;
        Ok(())
    }
}

fn validate_unique_activations(
    seen: &mut BTreeSet<u32>,
    view: &[PreparedSpriteInstance],
) -> Result<()> {
    seen.clear();
    for prepared in view {
        if !seen.insert(prepared.activation_order) {
            return Err(Error::Invalid(
                "prepared sprite view repeats an activation".to_owned(),
            ));
        }
    }
    Ok(())
}

fn repack_same_layout(
    output: &mut TemporalSpriteBatch,
    views_per_batch: NonZeroU32,
    active_views: NonZeroU32,
    views: &[&[PreparedSpriteInstance]],
) -> Result<()> {
    let slots = views[0];
    let instances_per_view = u32::try_from(slots.len()).map_err(|_| Error::ArithmeticOverflow)?;
    let total_instances = (views_per_batch.get() as usize)
        .checked_mul(slots.len())
        .ok_or(Error::ArithmeticOverflow)?;
    output.instances.clear();
    output.instances.reserve(total_instances);
    for view in views {
        output
            .instances
            .extend(view.iter().map(|prepared| prepared.instance));
    }
    output
        .instances
        .resize(total_instances, SpriteInstance::zeroed());
    compile_draw_runs_into(
        &mut output.draw_runs,
        slots.iter().map(|prepared| {
            (
                prepared.layer_order,
                prepared.blend_mode,
                prepared.instance.has_blur_filter != 0,
            )
        }),
    )?;
    output.slot_activations.clear();
    output
        .slot_activations
        .extend(slots.iter().map(|prepared| prepared.activation_order));
    output.active_views = active_views;
    output.instances_per_view = instances_per_view;
    Ok(())
}

fn compile_draw_runs_into(
    draw_runs: &mut Vec<SpriteDrawRun>,
    run_identities: impl Iterator<Item = (u32, SpriteBlendMode, bool)>,
) -> Result<()> {
    draw_runs.clear();
    for (index, (layer_order, blend_mode, has_blur_filter)) in run_identities.enumerate() {
        let index = u32::try_from(index).map_err(|_| Error::ArithmeticOverflow)?;
        if let Some(run) = draw_runs.last_mut()
            && !has_blur_filter
            && !run.has_blur_filter
            && run.layer_order == layer_order
            && run.blend_mode == blend_mode
        {
            run.instances.end = index + 1;
        } else {
            draw_runs.push(SpriteDrawRun {
                layer_order,
                blend_mode,
                has_blur_filter,
                instances: index..index + 1,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use bytemuck::Zeroable;

    use crate::{PreparedSpriteInstance, SpriteBlendMode, SpriteInstance, TemporalSpriteBatch};

    fn prepared(
        activation_order: u32,
        layer_order: u32,
        blend_mode: SpriteBlendMode,
        marker: f32,
    ) -> PreparedSpriteInstance {
        let mut instance = SpriteInstance::zeroed();
        instance.transform_x[0] = marker;
        instance.visible = 1;
        PreparedSpriteInstance {
            activation_order,
            layer_order,
            blend_mode,
            instance,
        }
    }

    #[test]
    fn packs_union_slots_and_pads_every_configured_view() {
        let first = [prepared(20, 0, SpriteBlendMode::Normal, 20.0)];
        let second = [
            prepared(20, 0, SpriteBlendMode::Normal, 21.0),
            prepared(3, 1, SpriteBlendMode::Add, 30.0),
        ];
        let batch = TemporalSpriteBatch::pack(
            NonZeroU32::new(3).unwrap(),
            &[first.as_slice(), second.as_slice()],
        )
        .unwrap();

        assert_eq!(batch.active_views.get(), 2);
        assert_eq!(batch.instances_per_view, 2);
        assert_eq!(batch.instances.len(), 6);
        assert_eq!(batch.instances[0].transform_x[0], 20.0);
        assert_eq!(batch.instances[1], SpriteInstance::zeroed());
        assert_eq!(batch.instances[2].transform_x[0], 21.0);
        assert_eq!(batch.instances[3].transform_x[0], 30.0);
        assert_eq!(batch.instances[4], SpriteInstance::zeroed());
        assert_eq!(batch.instances[5], SpriteInstance::zeroed());
        assert_eq!(batch.draw_runs.len(), 2);
        assert_eq!(batch.draw_runs[0].layer_order, 0);
        assert_eq!(batch.draw_runs[0].instances, 0..1);
        assert_eq!(batch.draw_runs[1].layer_order, 1);
        assert_eq!(batch.draw_runs[1].instances, 1..2);
    }

    #[test]
    fn rejects_invalid_view_count_order_and_blend_identity() {
        let one = [prepared(1, 0, SpriteBlendMode::Normal, 1.0)];
        assert!(TemporalSpriteBatch::pack(NonZeroU32::new(1).unwrap(), &[&one]).is_err());
        assert!(
            TemporalSpriteBatch::pack(NonZeroU32::new(2).unwrap(), &[&one, &one, &one],).is_err()
        );
        let duplicate = [
            prepared(1, 0, SpriteBlendMode::Normal, 1.0),
            prepared(1, 0, SpriteBlendMode::Normal, 2.0),
        ];
        assert!(TemporalSpriteBatch::pack(NonZeroU32::new(2).unwrap(), &[&duplicate]).is_err());
        let changed = [prepared(1, 0, SpriteBlendMode::Add, 1.0)];
        assert!(TemporalSpriteBatch::pack(NonZeroU32::new(2).unwrap(), &[&one, &changed]).is_err());
        let moved = [prepared(1, 1, SpriteBlendMode::Normal, 1.0)];
        assert!(TemporalSpriteBatch::pack(NonZeroU32::new(2).unwrap(), &[&one, &moved]).is_err());
    }

    #[test]
    fn keeps_layer_boundaries_between_identical_blend_modes() {
        let view = [
            prepared(1, 0, SpriteBlendMode::Normal, 1.0),
            prepared(2, 1, SpriteBlendMode::Normal, 2.0),
        ];
        let batch =
            TemporalSpriteBatch::pack(NonZeroU32::new(2).unwrap(), &[view.as_slice()]).unwrap();
        assert_eq!(batch.draw_runs.len(), 2);
        assert_eq!(batch.draw_runs[0].layer_order, 0);
        assert_eq!(batch.draw_runs[0].instances, 0..1);
        assert_eq!(batch.draw_runs[1].layer_order, 1);
        assert_eq!(batch.draw_runs[1].instances, 1..2);
    }

    #[test]
    fn isolates_blur_filters_and_rejects_filter_identity_changes() {
        let mut first = prepared(1, 0, SpriteBlendMode::Add, 1.0);
        let mut second = prepared(2, 0, SpriteBlendMode::Add, 2.0);
        let third = prepared(3, 0, SpriteBlendMode::Add, 3.0);
        first.instance.has_blur_filter = 1;
        first.instance.blur = 30.0;
        second.instance.has_blur_filter = 1;
        second.instance.blur = 15.0;
        let view = [first, second, third];
        let batch =
            TemporalSpriteBatch::pack(NonZeroU32::new(2).unwrap(), &[view.as_slice()]).unwrap();
        assert_eq!(batch.draw_runs.len(), 3);
        assert!(batch.draw_runs[0].has_blur_filter);
        assert_eq!(batch.draw_runs[0].instances, 0..1);
        assert!(batch.draw_runs[1].has_blur_filter);
        assert_eq!(batch.draw_runs[1].instances, 1..2);
        assert!(!batch.draw_runs[2].has_blur_filter);

        let mut changed = first;
        changed.instance.has_blur_filter = 0;
        assert!(
            TemporalSpriteBatch::pack(
                NonZeroU32::new(2).unwrap(),
                &[view.as_slice(), &[changed, second, third]],
            )
            .is_err()
        );
    }
}
