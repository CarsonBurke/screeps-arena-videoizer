use std::collections::BTreeMap;

use crate::{Entity, Error, Rational, ResolvedValue, Result, Track, TrackValue, ValueContext};

/// Owned expression roots for one entity activation.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityValueRoots {
    pub state: ResolvedValue,
    pub calculations: ResolvedValue,
    pub processor_parameters: ResolvedValue,
}

impl EntityValueRoots {
    pub fn at(entity: &Entity, tick: u32, tick_duration: Rational) -> Result<Self> {
        if !entity.alive_at(tick) {
            return Err(Error::Invalid(format!(
                "cannot resolve renderer values for inactive entity {} at tick {tick}",
                entity.id
            )));
        }
        Ok(Self {
            state: resolve_tracks(&entity.properties, tick)?,
            calculations: resolve_tracks(&entity.calculations, tick)?,
            processor_parameters: ResolvedValue::Object(BTreeMap::from([(
                "tickDuration".to_owned(),
                ResolvedValue::Number(tick_duration.as_f64()),
            )])),
        })
    }

    pub fn context<'a>(&'a self, relative: Option<&'a ResolvedValue>) -> ValueContext<'a> {
        ValueContext {
            state: &self.state,
            calculations: &self.calculations,
            processor_parameters: &self.processor_parameters,
            relative,
        }
    }
}

fn resolve_tracks(tracks: &BTreeMap<String, Track>, tick: u32) -> Result<ResolvedValue> {
    let values = tracks
        .iter()
        .filter_map(|(name, track)| match track.at(tick) {
            None | Some(TrackValue::Absent) => None,
            Some(TrackValue::Undefined) => Some(Ok((name.clone(), ResolvedValue::Undefined))),
            Some(TrackValue::Value(value)) => {
                Some(ResolvedValue::from_json(value).map(|value| (name.clone(), value)))
            }
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok(ResolvedValue::Object(values))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use crate::{Entity, EntityValueRoots, Rational, ResolvedValue, Track};

    #[test]
    fn samples_state_calculations_and_explicit_undefined_at_activation_tick() {
        let entity = Entity {
            id: "unit".to_owned(),
            lifetimes: vec![[1, 3]],
            properties: BTreeMap::from([
                (
                    "x".to_owned(),
                    Track(vec![1, 3], vec![json!(12)], vec![], vec![]),
                ),
                (
                    "gone".to_owned(),
                    Track(vec![1, 3], vec![json!(null)], vec![0], vec![]),
                ),
                (
                    "unknown".to_owned(),
                    Track(vec![1, 3], vec![json!(null)], vec![], vec![0]),
                ),
                (
                    "literal".to_owned(),
                    Track(
                        vec![1, 3],
                        vec![json!({"$undefined": true})],
                        vec![],
                        vec![],
                    ),
                ),
            ]),
            calculations: BTreeMap::from([(
                "height".to_owned(),
                Track(vec![1, 2, 2, 3], vec![json!(4), json!(5)], vec![], vec![]),
            )]),
        };
        let roots = EntityValueRoots::at(&entity, 2, Rational::new(1, 4).unwrap()).unwrap();
        let ResolvedValue::Object(state) = &roots.state else {
            panic!("expected state object")
        };
        assert_eq!(state["x"], ResolvedValue::Number(12.0));
        assert_eq!(state["unknown"], ResolvedValue::Undefined);
        assert!(!state.contains_key("gone"));
        assert_eq!(
            state["literal"],
            ResolvedValue::Object(BTreeMap::from([(
                "$undefined".to_owned(),
                ResolvedValue::Bool(true)
            )]))
        );
        assert_eq!(
            roots.calculations.get("height"),
            Some(&ResolvedValue::Number(5.0))
        );
        assert_eq!(
            roots.processor_parameters.get("tickDuration"),
            Some(&ResolvedValue::Number(0.25))
        );
        assert!(EntityValueRoots::at(&entity, 0, Rational::new(1, 4).unwrap()).is_err());
    }
}
