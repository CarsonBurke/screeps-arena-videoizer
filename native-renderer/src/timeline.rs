use crate::{Error, Rational, ReplayIr, Result};

const MICROSECONDS_PER_SECOND: u128 = 1_000_000;

/// One independently addressable output timestamp.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameSample {
    pub frame: u64,
    pub tick: u32,
    pub time: Rational,
    pub timestamp_us: u64,
    pub duration_us: u64,
    /// Progress across the whole replay tick interval, in `[0, 1]`.
    pub tick_progress: f32,
    /// Progress across the renderer's configured state transition, in `[0, 1]`.
    pub transition_progress: f32,
}

#[derive(Debug)]
pub struct FrameBatch {
    pub index: u64,
    pub frame_start: u64,
    pub frames: Vec<FrameSample>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AdvanceStep {
    /// The transition target currently installed in the renderer.
    pub tick: u32,
    pub from: Rational,
    pub to: Rational,
    pub duration_seconds: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TimelineEvent {
    ApplyTick { tick: u32, time: Rational },
    Advance(AdvanceStep),
    Render(FrameSample),
}

#[derive(Clone, Copy, Debug)]
pub struct Timeline {
    total_ticks: u32,
    frames_per_second: Rational,
    ticks_per_second: Rational,
    substeps_per_second: Rational,
    tick_transition_seconds: Rational,
    endpoint: Rational,
    frame_count: u64,
}

pub struct FrameBatchIter {
    timeline: Timeline,
    batch_size: u64,
    next_frame: u64,
    next_batch: u64,
}

pub struct TimelineEventIter {
    timeline: Timeline,
    frame: u64,
    tick: u32,
    time: Rational,
    emitted_initial_tick: bool,
    pending_apply_tick: bool,
}

impl Timeline {
    pub fn from_replay(replay: &ReplayIr) -> Result<Self> {
        let frames_per_second = required_rate(
            replay.timeline.frames_per_second.0.as_deref(),
            "framesPerSecond",
        )?;
        let ticks_per_second = required_rate(
            replay.timeline.ticks_per_second.0.as_deref(),
            "ticksPerSecond",
        )?;
        let substeps_per_second = required_rate(
            replay.timeline.substeps_per_second.0.as_deref(),
            "substepsPerSecond",
        )?;
        let tick_transition_seconds = required_rate(
            replay.timeline.tick_transition_seconds.0.as_deref(),
            "tickTransitionSeconds",
        )?;
        Self::new(
            replay.total_ticks,
            frames_per_second,
            ticks_per_second,
            substeps_per_second,
            tick_transition_seconds,
        )
    }

    pub fn new(
        total_ticks: u32,
        frames_per_second: Rational,
        ticks_per_second: Rational,
        substeps_per_second: Rational,
        tick_transition_seconds: Rational,
    ) -> Result<Self> {
        if frames_per_second.numerator() == 0
            || ticks_per_second.numerator() == 0
            || substeps_per_second.numerator() == 0
            || tick_transition_seconds.numerator() == 0
        {
            return Err(Error::Invalid(
                "timeline rates must all be positive".to_owned(),
            ));
        }
        let endpoint = Rational::new(total_ticks as u128, 1)?.checked_div(ticks_per_second)?;
        let frame_intervals = Rational::new(total_ticks as u128, 1)?
            .checked_mul(frames_per_second)?
            .checked_div(ticks_per_second)?
            .ceil()?;
        let frame_count = frame_intervals
            .checked_add(1)
            .and_then(|value| u64::try_from(value).ok())
            .ok_or(Error::ArithmeticOverflow)?;
        Ok(Self {
            total_ticks,
            frames_per_second,
            ticks_per_second,
            substeps_per_second,
            tick_transition_seconds,
            endpoint,
            frame_count,
        })
    }

    pub const fn frame_count(self) -> u64 {
        self.frame_count
    }

    pub const fn total_ticks(self) -> u32 {
        self.total_ticks
    }

    pub const fn endpoint(self) -> Rational {
        self.endpoint
    }

    /// Absolute time at which a replay tick is installed in the renderer.
    /// Tick zero is applied before frame zero; tick one is installed
    /// immediately after frame zero, also at time zero.
    pub fn apply_tick_time(self, tick: u32) -> Result<Rational> {
        if tick > self.total_ticks {
            return Err(Error::Invalid(
                "timeline apply tick exceeds replay endpoint".to_owned(),
            ));
        }
        let completed_intervals = tick.saturating_sub(1);
        Rational::new(u128::from(completed_intervals), 1)?.checked_div(self.ticks_per_second)
    }

    pub fn sample(self, frame: u64) -> Result<FrameSample> {
        if frame >= self.frame_count {
            return Err(Error::Invalid(
                "frame index exceeds replay endpoint".to_owned(),
            ));
        }
        let time = self.frame_time(frame)?;
        let next_time = if frame + 1 == self.frame_count {
            time.checked_add(Rational::new(1, 1)?.checked_div(self.frames_per_second)?)?
        } else {
            self.frame_time(frame + 1)?
        };
        let timestamp_us = to_timestamp_us(time)?;
        let next_timestamp_us = to_timestamp_us(next_time)?;
        let duration_us = next_timestamp_us
            .checked_sub(timestamp_us)
            .ok_or(Error::ArithmeticOverflow)?;

        let tick = if frame == 0 {
            0
        } else {
            let completed = time.checked_mul(self.ticks_per_second)?.floor();
            let next = completed.saturating_add(1).min(self.total_ticks as u128);
            u32::try_from(next).map_err(|_| Error::ArithmeticOverflow)?
        };
        let (tick_progress, transition_progress) = if tick == 0 {
            (0.0, 1.0)
        } else {
            let transition_start =
                Rational::new((tick - 1) as u128, 1)?.checked_div(self.ticks_per_second)?;
            let elapsed = time.checked_sub(transition_start)?;
            let tick_progress = elapsed
                .checked_mul(self.ticks_per_second)?
                .as_f64()
                .clamp(0.0, 1.0) as f32;
            let transition_progress = elapsed
                .checked_div(self.tick_transition_seconds)?
                .as_f64()
                .clamp(0.0, 1.0) as f32;
            (tick_progress, transition_progress)
        };

        Ok(FrameSample {
            frame,
            tick,
            time,
            timestamp_us,
            duration_us,
            tick_progress,
            transition_progress,
        })
    }

    pub fn batches(self, batch_size: u64) -> Result<FrameBatchIter> {
        if batch_size == 0 {
            return Err(Error::Invalid(
                "frame batch size must be positive".to_owned(),
            ));
        }
        Ok(FrameBatchIter {
            timeline: self,
            batch_size,
            next_frame: 0,
            next_batch: 0,
        })
    }

    /// Iterate the exact compatibility-runtime order without retaining a
    /// mutable Pixi scene: tick application, bounded action/ticker advancement,
    /// then frame rendering. Frame and fixed-substep boundaries both split an
    /// advance, and a coincident tick is applied before its render.
    pub fn events(self) -> TimelineEventIter {
        TimelineEventIter {
            timeline: self,
            frame: 0,
            tick: 0,
            time: Rational::ZERO,
            emitted_initial_tick: false,
            pending_apply_tick: false,
        }
    }

    fn frame_time(self, frame: u64) -> Result<Rational> {
        if frame + 1 == self.frame_count {
            Ok(self.endpoint)
        } else {
            Rational::new(frame as u128, 1)?.checked_div(self.frames_per_second)
        }
    }
}

impl Iterator for FrameBatchIter {
    type Item = Result<FrameBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next_frame >= self.timeline.frame_count {
            return None;
        }
        let end = self
            .next_frame
            .saturating_add(self.batch_size)
            .min(self.timeline.frame_count);
        let result = (self.next_frame..end)
            .map(|frame| self.timeline.sample(frame))
            .collect::<Result<Vec<_>>>()
            .map(|frames| FrameBatch {
                index: self.next_batch,
                frame_start: self.next_frame,
                frames,
            });
        self.next_frame = end;
        self.next_batch += 1;
        Some(result)
    }
}

impl Iterator for TimelineEventIter {
    type Item = Result<TimelineEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.emitted_initial_tick {
            self.emitted_initial_tick = true;
            return Some(Ok(TimelineEvent::ApplyTick {
                tick: 0,
                time: Rational::ZERO,
            }));
        }
        if self.pending_apply_tick {
            self.pending_apply_tick = false;
            return Some(Ok(TimelineEvent::ApplyTick {
                tick: self.tick,
                time: self.time,
            }));
        }
        if self.frame >= self.timeline.frame_count {
            return None;
        }

        let frame_time = match self.timeline.frame_time(self.frame) {
            Ok(time) => time,
            Err(error) => return Some(Err(error)),
        };
        if self.time < frame_time {
            let next_tick_time = match Rational::new(self.tick as u128, 1)
                .and_then(|tick| tick.checked_div(self.timeline.ticks_per_second))
            {
                Ok(time) => time,
                Err(error) => return Some(Err(error)),
            };
            let next_substep_time = match self
                .time
                .checked_mul(self.timeline.substeps_per_second)
                .map(Rational::floor)
                .and_then(|completed| completed.checked_add(1).ok_or(Error::ArithmeticOverflow))
                .and_then(|next| Rational::new(next, 1))
                .and_then(|next| next.checked_div(self.timeline.substeps_per_second))
            {
                Ok(time) => time,
                Err(error) => return Some(Err(error)),
            };
            let end = frame_time.min(next_tick_time).min(next_substep_time);
            let duration = match end.checked_sub(self.time) {
                Ok(duration) => duration,
                Err(error) => return Some(Err(error)),
            };
            let event = TimelineEvent::Advance(AdvanceStep {
                tick: self.tick,
                from: self.time,
                to: end,
                duration_seconds: duration.as_f64(),
            });
            self.time = end;
            if self.time == next_tick_time && self.tick < self.timeline.total_ticks {
                self.tick += 1;
                self.pending_apply_tick = true;
            }
            return Some(Ok(event));
        }

        let sample = match self.timeline.sample(self.frame) {
            Ok(sample) => sample,
            Err(error) => return Some(Err(error)),
        };
        if sample.tick != self.tick {
            return Some(Err(Error::Invalid(format!(
                "timeline event tick {} disagrees with frame {} tick {}",
                self.tick, self.frame, sample.tick
            ))));
        }
        self.frame += 1;
        if sample.frame == 0 && self.timeline.total_ticks > 0 {
            self.tick = 1;
            self.pending_apply_tick = true;
        }
        Some(Ok(TimelineEvent::Render(sample)))
    }
}

fn required_rate(value: Option<&str>, name: &str) -> Result<Rational> {
    Rational::parse_rate(
        value.ok_or_else(|| Error::Invalid(format!("ReplayIR timeline lacks {name}")))?,
        name,
    )
}

fn to_timestamp_us(time: Rational) -> Result<u64> {
    let value = time
        .checked_mul(Rational::new(MICROSECONDS_PER_SECOND, 1)?)?
        .round()?;
    u64::try_from(value).map_err(|_| Error::ArithmeticOverflow)
}

#[cfg(test)]
mod tests {
    use crate::artifact::tests::artifact_json;
    use crate::{Rational, ReplayArtifact};

    use super::{Timeline, TimelineEvent};

    #[test]
    fn matches_off_grid_endpoint_and_duration_contract() {
        let artifact = ReplayArtifact::from_slice(&artifact_json()).unwrap();
        let timeline = Timeline::from_replay(&artifact.replay).unwrap();
        assert_eq!(timeline.frame_count(), 2);

        let first = timeline.sample(0).unwrap();
        let final_frame = timeline.sample(1).unwrap();
        assert_eq!(first.time.to_string(), "0");
        assert_eq!(first.tick, 0);
        assert_eq!(first.timestamp_us, 0);
        assert_eq!(first.duration_us, 333_333);
        assert_eq!(final_frame.time.to_string(), "1/3");
        assert_eq!(final_frame.tick, 1);
        assert_eq!(final_frame.timestamp_us, 333_333);
        assert_eq!(final_frame.duration_us, 500_000);
        assert_eq!(final_frame.tick_progress, 1.0);
        assert_eq!(final_frame.transition_progress, 1.0);
    }

    #[test]
    fn matches_large_and_fractional_frame_counts() {
        let timeline = Timeline::new(
            2_000,
            Rational::parse_rate("30", "fps").unwrap(),
            Rational::parse_rate("15/4", "tps").unwrap(),
            Rational::parse_rate("120", "substeps").unwrap(),
            Rational::parse_rate("1/4", "transition").unwrap(),
        )
        .unwrap();
        assert_eq!(timeline.frame_count(), 16_001);

        let fractional = Timeline::new(
            121,
            Rational::parse_rate("30000/1001", "fps").unwrap(),
            Rational::parse_rate("15/4", "tps").unwrap(),
            Rational::parse_rate("120", "substeps").unwrap(),
            Rational::parse_rate("1/4", "transition").unwrap(),
        )
        .unwrap();
        assert_eq!(fractional.frame_count(), 969);
    }

    #[test]
    fn batches_cover_each_frame_once() {
        let timeline = Timeline::new(
            2,
            Rational::parse_rate("30", "fps").unwrap(),
            Rational::parse_rate("5", "tps").unwrap(),
            Rational::parse_rate("60", "substeps").unwrap(),
            Rational::parse_rate("1/5", "transition").unwrap(),
        )
        .unwrap();
        let batches = timeline
            .batches(4)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let frames = batches
            .iter()
            .flat_map(|batch| batch.frames.iter().map(|frame| frame.frame))
            .collect::<Vec<_>>();
        assert_eq!(frames, (0..timeline.frame_count()).collect::<Vec<_>>());
        assert_eq!(batches.last().unwrap().frames.len(), 1);
    }

    #[test]
    fn event_stream_matches_fixed_substeps_and_tick_before_render_order() {
        let timeline = Timeline::new(
            1,
            Rational::parse_rate("4", "fps").unwrap(),
            Rational::parse_rate("1", "tps").unwrap(),
            Rational::parse_rate("2", "substeps").unwrap(),
            Rational::parse_rate("1", "transition").unwrap(),
        )
        .unwrap();
        let events = timeline.events().collect::<Result<Vec<_>, _>>().unwrap();
        let labels = events
            .iter()
            .map(|event| match event {
                TimelineEvent::ApplyTick { tick, time } => {
                    format!("apply:{tick}@{time}")
                }
                TimelineEvent::Advance(step) => {
                    format!("advance:{}:{}-{}", step.tick, step.from, step.to)
                }
                TimelineEvent::Render(frame) => {
                    format!("render:{}:{}@{}", frame.frame, frame.tick, frame.time)
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            [
                "apply:0@0",
                "render:0:0@0",
                "apply:1@0",
                "advance:1:0-1/4",
                "render:1:1@1/4",
                "advance:1:1/4-1/2",
                "render:2:1@1/2",
                "advance:1:1/2-3/4",
                "render:3:1@3/4",
                "advance:1:3/4-1",
                "render:4:1@1",
            ]
        );
    }

    #[test]
    fn apply_tick_times_follow_the_initial_frame_compatibility_order() {
        let timeline = Timeline::new(
            3,
            Rational::new(6, 1).unwrap(),
            Rational::new(2, 1).unwrap(),
            Rational::new(12, 1).unwrap(),
            Rational::new(1, 4).unwrap(),
        )
        .unwrap();
        assert_eq!(timeline.apply_tick_time(0).unwrap(), Rational::ZERO);
        assert_eq!(timeline.apply_tick_time(1).unwrap(), Rational::ZERO);
        assert_eq!(
            timeline.apply_tick_time(2).unwrap(),
            Rational::new(1, 2).unwrap()
        );
        assert_eq!(
            timeline.apply_tick_time(3).unwrap(),
            Rational::new(1, 1).unwrap()
        );
        assert!(timeline.apply_tick_time(4).is_err());
    }

    #[test]
    fn coincident_tick_is_applied_before_the_frame_at_its_boundary() {
        let timeline = Timeline::new(
            2,
            Rational::parse_rate("2", "fps").unwrap(),
            Rational::parse_rate("2", "tps").unwrap(),
            Rational::parse_rate("8", "substeps").unwrap(),
            Rational::parse_rate("1/2", "transition").unwrap(),
        )
        .unwrap();
        let events = timeline.events().collect::<Result<Vec<_>, _>>().unwrap();
        let boundary = events
            .windows(2)
            .find(|pair| {
                matches!(
                    pair,
                    [
                        TimelineEvent::ApplyTick { tick: 2, .. },
                        TimelineEvent::Render(frame)
                    ] if frame.frame == 1 && frame.tick == 2
                )
            })
            .expect("tick application immediately precedes coincident render");
        assert!(matches!(
            boundary[0],
            TimelineEvent::ApplyTick {
                tick: 2,
                time
            } if time.to_string() == "1/2"
        ));
    }

    #[test]
    fn fractional_event_stream_is_gapless_bounded_and_complete() {
        let substeps = Rational::parse_rate("11/2", "substeps").unwrap();
        let timeline = Timeline::new(
            121,
            Rational::parse_rate("30000/1001", "fps").unwrap(),
            Rational::parse_rate("7/3", "tps").unwrap(),
            substeps,
            Rational::parse_rate("1/4", "transition").unwrap(),
        )
        .unwrap();
        let events = timeline.events().collect::<Result<Vec<_>, _>>().unwrap();
        let maximum_step = Rational::new(1, 1).unwrap().checked_div(substeps).unwrap();
        let mut previous_advance_end = Rational::ZERO;
        let mut apply_ticks = Vec::new();
        let mut renders = Vec::new();
        for event in events {
            match event {
                TimelineEvent::ApplyTick { tick, .. } => apply_ticks.push(tick),
                TimelineEvent::Advance(step) => {
                    assert_eq!(step.from, previous_advance_end);
                    let duration = step.to.checked_sub(step.from).unwrap();
                    assert!(duration > Rational::ZERO);
                    assert!(duration <= maximum_step);
                    previous_advance_end = step.to;
                }
                TimelineEvent::Render(frame) => renders.push(frame),
            }
        }
        assert_eq!(apply_ticks, (0..=121).collect::<Vec<_>>());
        assert_eq!(renders.len() as u64, timeline.frame_count());
        assert_eq!(renders.last().unwrap().time, timeline.endpoint());
        assert_eq!(previous_advance_end, timeline.endpoint());
    }
}
