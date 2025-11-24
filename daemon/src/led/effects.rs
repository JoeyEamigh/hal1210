use std::{f32::consts::TAU, time::Duration};

use ledcomm::{self, StateFrame};
use tokio::time::Instant;

pub const DEFAULT_FADE_IN_DURATION: Duration = Duration::from_millis(1_500);
pub const DEFAULT_FADE_OUT_DURATION: Duration = Duration::from_millis(3_000);
const RAINBOW_TICK: Duration = Duration::from_millis(8);
const BREATHING_TICK: Duration = Duration::from_millis(16);
const FADE_TICK: Duration = Duration::from_millis(16);
const BREATHING_PERIOD_SECONDS: f32 = 4.0;

#[derive(Debug)]
pub enum AnimationStatus {
  Running,
  Finished,
}

#[derive(Debug)]
pub struct AnimationState {
  kind: AnimationKind,
  interval: Duration,
  next_tick: Instant,
}

impl AnimationState {
  pub fn rainbow() -> Self {
    Self::new(AnimationKind::Rainbow(RainbowState::default()), RAINBOW_TICK)
  }

  pub fn breathing(color: [u8; 3]) -> Self {
    Self::new(AnimationKind::Breathing(BreathingState::new(color)), BREATHING_TICK)
  }

  pub fn fade_out(current: &StateFrame, duration: Duration) -> Self {
    let start = *current;
    let target = ledcomm::zero_state_frame();
    Self::new(
      AnimationKind::Fade(FadeState::new(start, target, duration, FadeKind::Out)),
      FADE_TICK,
    )
  }

  pub fn fade_in(target: StateFrame, duration: Duration) -> Self {
    let start = ledcomm::zero_state_frame();
    Self::new(
      AnimationKind::Fade(FadeState::new(start, target, duration, FadeKind::In)),
      FADE_TICK,
    )
  }

  fn new(kind: AnimationKind, interval: Duration) -> Self {
    Self {
      kind,
      interval,
      next_tick: Instant::now(),
    }
  }

  pub fn next_deadline(&self) -> Instant {
    self.next_tick
  }

  pub fn schedule_next_tick(&mut self) {
    self.next_tick = Instant::now() + self.interval;
  }

  pub fn render(&mut self, frame: &mut StateFrame) -> AnimationStatus {
    self.kind.render(frame)
  }

  pub fn update_fade_target(&mut self, current_frame: &StateFrame, target: &StateFrame) -> bool {
    match &mut self.kind {
      AnimationKind::Fade(state) => state.retarget(current_frame, target),
      _ => false,
    }
  }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum AnimationKind {
  Rainbow(RainbowState),
  Breathing(BreathingState),
  Fade(FadeState),
}

impl AnimationKind {
  fn render(&mut self, frame: &mut StateFrame) -> AnimationStatus {
    match self {
      AnimationKind::Rainbow(state) => {
        state.render(frame);
        AnimationStatus::Running
      }
      AnimationKind::Breathing(state) => {
        state.render(frame);
        AnimationStatus::Running
      }
      AnimationKind::Fade(state) => {
        if state.render(frame) {
          AnimationStatus::Finished
        } else {
          AnimationStatus::Running
        }
      }
    }
  }
}

#[derive(Debug, Default)]
struct RainbowState {
  hue: u8,
}

impl RainbowState {
  fn render(&mut self, frame: &mut StateFrame) {
    let base_hue = self.hue;
    self.hue = self.hue.wrapping_add(1);

    for (idx, pixel) in frame.iter_mut().enumerate() {
      let offset = (idx as u16 % 256) as u8;
      let hue = base_hue.wrapping_add(offset);
      *pixel = hsv_to_rgb(hue, 255, 255);
    }
  }
}

#[derive(Debug)]
struct BreathingState {
  color: [u8; 3],
  phase: f32,
  phase_step: f32,
}

impl BreathingState {
  fn new(color: [u8; 3]) -> Self {
    let step = (BREATHING_TICK.as_secs_f32() / BREATHING_PERIOD_SECONDS) * TAU;
    Self {
      color,
      phase: 0.0,
      phase_step: step,
    }
  }

  fn render(&mut self, frame: &mut StateFrame) {
    let brightness = 0.5 + 0.5 * self.phase.sin();
    self.phase = (self.phase + self.phase_step) % TAU;

    for pixel in frame.iter_mut() {
      pixel[0] = scale_channel(self.color[0], brightness);
      pixel[1] = scale_channel(self.color[1], brightness);
      pixel[2] = scale_channel(self.color[2], brightness);
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FadeKind {
  In,
  Out,
}

#[derive(Debug)]
struct FadeState {
  start: StateFrame,
  target: StateFrame,
  duration: Duration,
  started_at: Instant,
  kind: FadeKind,
}

impl FadeState {
  fn new(start: StateFrame, target: StateFrame, duration: Duration, kind: FadeKind) -> Self {
    Self {
      start,
      target,
      duration,
      started_at: Instant::now(),
      kind,
    }
  }

  fn render(&mut self, frame: &mut StateFrame) -> bool {
    if self.duration.is_zero() {
      frame.clone_from(&self.target);
      return true;
    }

    let now = Instant::now();
    let elapsed = now.saturating_duration_since(self.started_at);
    let progress = (elapsed.as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0);

    for (idx, pixel) in frame.iter_mut().enumerate() {
      let start = self.start[idx];
      let target = self.target[idx];
      pixel[0] = lerp(start[0], target[0], progress);
      pixel[1] = lerp(start[1], target[1], progress);
      pixel[2] = lerp(start[2], target[2], progress);
    }

    progress >= 1.0
  }

  fn retarget(&mut self, current: &StateFrame, new_target: &StateFrame) -> bool {
    if !matches!(self.kind, FadeKind::In) {
      return false;
    }

    let now = Instant::now();
    let elapsed = now.saturating_duration_since(self.started_at);
    let remaining = self.duration.saturating_sub(elapsed);

    self.start = *current;
    self.target = *new_target;
    self.duration = remaining;
    self.started_at = now;
    true
  }
}

fn hsv_to_rgb(hue: u8, sat: u8, val: u8) -> [u8; 3] {
  if sat == 0 {
    return [val, val, val];
  }

  let h = (hue as f32 / 255.0) * 6.0;
  let s = sat as f32 / 255.0;
  let v = val as f32 / 255.0;

  let sector = h.floor() as i32;
  let fraction = h - sector as f32;

  let p = v * (1.0 - s);
  let q = v * (1.0 - s * fraction);
  let t = v * (1.0 - s * (1.0 - fraction));

  let (r, g, b) = match sector.rem_euclid(6) {
    0 => (v, t, p),
    1 => (q, v, p),
    2 => (p, v, t),
    3 => (p, q, v),
    4 => (t, p, v),
    _ => (v, p, q),
  };

  [scale_float(r), scale_float(g), scale_float(b)]
}

fn scale_channel(value: u8, factor: f32) -> u8 {
  let scaled = (value as f32) * factor;
  scaled.round().clamp(0.0, 255.0) as u8
}

fn scale_float(channel: f32) -> u8 {
  (channel * 255.0).round().clamp(0.0, 255.0) as u8
}

fn lerp(start: u8, end: u8, t: f32) -> u8 {
  let s = start as f32;
  let e = end as f32;
  (s + (e - s) * t).round().clamp(0.0, 255.0) as u8
}
