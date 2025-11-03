// Thank you to:
// https://github.com/nagisa/msi-rgb
// https://github.com/garashchenko/mystic-why
// https://github.com/cpldcpu/Gen2-Addressable-RGB
// even tho we probably won't end up using this code (fml)

use std::{
  fs::File,
  os::fd::AsRawFd,
  path::{Path, PathBuf},
  sync::{Arc, Mutex},
  time::{Duration as StdDuration, Instant},
};

use async_hid::{Device, DeviceId, HidBackend, HidError};
use futures::StreamExt;
use tokio::task;

use super::{Command, LedStripState};

const MSI_VENDOR_ID: u16 = 0x1462;
const TARGET_PRODUCT_ID: u16 = 0x7D77;

const REPORT_ID_FEATURE: u8 = 0x52;
const REPORT_ID_PER_LED: u8 = 0x53;

const PACKET_LEN: usize = 185;
const ZONE_LEN: usize = 10;
const RAINBOW_ZONE_LEN: usize = 11;

const J_RGB1_OFFSET: usize = 1;
const J_PIPE1_OFFSET: usize = 11;
const J_PIPE2_OFFSET: usize = 21;
const J_RAINBOW1_OFFSET: usize = 31;
const J_RAINBOW2_OFFSET: usize = 42;
const J_CORSAIR_OFFSET: usize = 53;
const J_CORSAIR_OUTER_OFFSET: usize = 64;
const ONBOARD_BASE_OFFSET: usize = 74;
const ONBOARD_COUNT: usize = 10;
const J_RGB2_OFFSET: usize = 174;
const SAVE_DATA_OFFSET: usize = 184;

const NUM_PER_LED_MODE_LEDS: usize = 240;
const PER_LED_PACKET_LEN: usize = 1 + 4 + NUM_PER_LED_MODE_LEDS * 3;

const MSI_DIRECT_MODE: u8 = 0x25;
const MODE_STATIC: u8 = 0x01;

const SYNC_SETTING_ONBOARD: u8 = 0x01;
const SYNC_SETTING_JRAINBOW1: u8 = 0x02;
const SYNC_SETTING_JRAINBOW2: u8 = 0x04;
const SYNC_SETTING_JCORSAIR: u8 = 0x08;
const SYNC_SETTING_JPIPE1: u8 = 0x10;
const SYNC_SETTING_JPIPE2: u8 = 0x20;
const SYNC_SETTING_JRGB: u8 = 0x80;

const PER_LED_BASIC_SYNC_MODE: u8 =
  SYNC_SETTING_ONBOARD | SYNC_SETTING_JPIPE1 | SYNC_SETTING_JPIPE2 | SYNC_SETTING_JRGB;
const PER_LED_FULL_SYNC_MODE: u8 =
  PER_LED_BASIC_SYNC_MODE | SYNC_SETTING_JRAINBOW1 | SYNC_SETTING_JRAINBOW2 | SYNC_SETTING_JCORSAIR;

const JRAINBOW1_MAX_LED_COUNT: usize = 200;
const JRAINBOW2_MAX_LED_COUNT: usize = 240;
const SYNC_PER_LED_MODE_JRAINBOW_LED_COUNT: usize = 40;

const JRAINBOW1_ACTIVE_LED_COUNT: usize = 160;
const JRAINBOW2_ACTIVE_LED_COUNT: usize = 0;

const SYNC_JRGB_OFFSET: usize = 0;
const SYNC_JRGB_COUNT: usize = 2;
const SYNC_JRAINBOW1_OFFSET: usize = SYNC_JRGB_OFFSET + SYNC_JRGB_COUNT;
const SYNC_JRAINBOW1_COUNT: usize = SYNC_PER_LED_MODE_JRAINBOW_LED_COUNT;
const SYNC_JRAINBOW2_OFFSET: usize = SYNC_JRAINBOW1_OFFSET + SYNC_JRAINBOW1_COUNT;
const SYNC_JRAINBOW2_COUNT: usize = SYNC_PER_LED_MODE_JRAINBOW_LED_COUNT;

pub struct MysticDevice {
  fd: Arc<Mutex<File>>,
  direct_packet: DirectModePacket,
  jr1_packet: PerLedPacket,
  jr2_packet: PerLedPacket,
  sync_packet: SyncPacket,
}

impl MysticDevice {
  pub async fn connect() -> Result<Self, MysticError> {
    let backend = HidBackend::default();
    let mut devices = backend.enumerate().await?;

    while let Some(device) = devices.next().await {
      if device.vendor_id == MSI_VENDOR_ID && device.product_id == TARGET_PRODUCT_ID {
        tracing::info!(name = %device.name, "found MSI Mystic device");
        let devnode = resolve_devnode(&device)?;
        let file = open_devnode(&devnode).await?;

        let mut mystic = Self {
          fd: Arc::new(Mutex::new(file)),
          direct_packet: DirectModePacket::new_unsynced(),
          jr1_packet: PerLedPacket::jr1(),
          jr2_packet: PerLedPacket::jr2(),
          sync_packet: SyncPacket::new(),
        };

        mystic.initialize().await?;
        mystic.trace_device_state().await?;
        return Ok(mystic);
      }
    }

    Err(MysticError::DeviceNotFound)
  }

  pub async fn handle_command(&mut self, cmd: Command) -> Result<(), MysticError> {
    match cmd {
      Command::SetStaticColor(color) => {
        self.apply_static_color(color);
        self.flush().await
      }
      Command::SetStripState(state) => {
        self.apply_strip_state(&state);
        self.flush().await
      }
    }
  }

  async fn initialize(&mut self) -> Result<(), MysticError> {
    self.sync_packet.clear_tail();
    self.apply_static_color([0, 0, 0]);
    self.enter_direct_mode().await?;
    self.flush().await
  }

  async fn trace_device_state(&self) -> Result<(), MysticError> {
    let feature = self.read_feature_report::<PACKET_LEN>(REPORT_ID_FEATURE).await?;
    let feature_state = FeatureReportState::from(&feature);
    tracing::trace!(raw52 = %hex_dump(&feature), "mystic feature state: {feature_state:?}" );

    let per_led = self
      .read_feature_report::<PER_LED_PACKET_LEN>(REPORT_ID_PER_LED)
      .await?;
    let per_led_state = PerLedReadback::from(&per_led);
    tracing::trace!(raw53 = %hex_dump(&per_led[..PER_LED_HEADER_LEN]), "mystic per-led report: {per_led_state:?}");
    Ok(())
  }

  async fn enter_direct_mode(&self) -> Result<(), MysticError> {
    let packet = self.direct_packet.as_bytes().to_vec();
    self.send_packet(packet).await
  }

  fn apply_static_color(&mut self, color: [u8; 3]) {
    self.jr1_packet.fill_constant(color);
    self.jr2_packet.fill_constant([0, 0, 0]);
    self
      .sync_packet
      .fill_constant_range(SYNC_JRGB_OFFSET, SYNC_JRGB_COUNT, color);
    self
      .sync_packet
      .fill_constant_range(SYNC_JRAINBOW1_OFFSET, SYNC_JRAINBOW1_COUNT, color);
    self
      .sync_packet
      .fill_constant_range(SYNC_JRAINBOW2_OFFSET, SYNC_JRAINBOW2_COUNT, color);
  }

  fn apply_strip_state(&mut self, state: &LedStripState) {
    self.jr1_packet.fill_from_slice(state);
    self.jr2_packet.clear();

    let primary = state.first().copied().unwrap_or([0, 0, 0]);
    self.sync_packet.set_led(SYNC_JRGB_OFFSET, primary);
    self.sync_packet.set_led(SYNC_JRGB_OFFSET + 1, primary);

    let (first_segment, rest) = state.split_at(state.len().min(SYNC_JRAINBOW1_COUNT));
    self
      .sync_packet
      .fill_range_from_slice(SYNC_JRAINBOW1_OFFSET, SYNC_JRAINBOW1_COUNT, first_segment);
    self
      .sync_packet
      .fill_range_from_slice(SYNC_JRAINBOW2_OFFSET, SYNC_JRAINBOW2_COUNT, rest);
  }

  #[tracing::instrument(name = "flush", skip(self), level = "trace")]
  async fn flush(&self) -> Result<(), MysticError> {
    let jr1 = self.jr1_packet.as_bytes().to_vec();
    self.send_packet(jr1).await?;

    if self.jr2_packet.has_content() {
      let jr2 = self.jr2_packet.as_bytes().to_vec();
      self.send_packet(jr2).await?;
    }

    let sync = self.sync_packet.as_bytes().to_vec();
    self.send_packet(sync).await
  }

  #[tracing::instrument(name="send", skip(self, data), fields(len = data.len()), level="trace")]
  async fn send_packet(&self, data: Vec<u8>) -> Result<(), MysticError> {
    let start = Instant::now();
    self
      .with_device(move |file| {
        let fd = file.as_raw_fd();
        let mut buffer = data;
        let request = hid_iocsfeature(buffer.len());
        let rc = unsafe { libc::ioctl(fd, request as libc::c_ulong, buffer.as_mut_ptr() as *mut libc::c_void) };
        if rc < 0 {
          return Err(MysticError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
      })
      .await?;

    let elapsed = start.elapsed();
    let elapsed_ms = elapsed.as_secs_f64() * 1_000.0;
    tracing::debug!(elapsed_ms, "feature report sent");
    if elapsed > StdDuration::from_millis(16) {
      tracing::warn!(elapsed_ms, "feature write exceeded frame budget");
    }
    Ok(())
  }

  async fn read_feature_report<const N: usize>(&self, report_id: u8) -> Result<[u8; N], MysticError> {
    self
      .with_device(move |file| {
        let fd = file.as_raw_fd();
        let mut buffer = [0u8; N];
        buffer[0] = report_id;
        let request = hid_iocgfeature(buffer.len());
        let rc = unsafe { libc::ioctl(fd, request as libc::c_ulong, buffer.as_mut_ptr() as *mut libc::c_void) };
        if rc < 0 {
          return Err(MysticError::Io(std::io::Error::last_os_error()));
        }
        Ok(buffer)
      })
      .await
  }

  async fn with_device<F, T>(&self, func: F) -> Result<T, MysticError>
  where
    F: FnOnce(&mut File) -> Result<T, MysticError> + Send + 'static,
    T: Send + 'static,
  {
    let fd = Arc::clone(&self.fd);
    task::spawn_blocking(move || {
      let mut guard = fd.lock().map_err(|_| MysticError::Poisoned)?;
      func(&mut guard)
    })
    .await?
  }
}

struct DirectModePacket {
  bytes: [u8; PACKET_LEN],
}

impl DirectModePacket {
  fn new_unsynced() -> Self {
    let mut this = Self {
      bytes: [0u8; PACKET_LEN],
    };
    this.bytes[0] = REPORT_ID_FEATURE;

    this.write_zone(J_RGB1_OFFSET, MODE_STATIC, 0x08, 0x80);
    this.write_zone(J_PIPE1_OFFSET, MODE_STATIC, 0x2A, 0x80);
    this.write_zone(J_PIPE2_OFFSET, MODE_STATIC, 0x2A, 0x80);
    this.write_rainbow_zone(
      J_RAINBOW1_OFFSET,
      MSI_DIRECT_MODE,
      0x29,
      0x80,
      JRAINBOW1_MAX_LED_COUNT as u8,
    );
    this.write_rainbow_zone(
      J_RAINBOW2_OFFSET,
      MSI_DIRECT_MODE,
      0x29,
      0x80,
      JRAINBOW2_MAX_LED_COUNT as u8,
    );
    this.write_zone(J_CORSAIR_OFFSET, MODE_STATIC, 0x29, 0x80);
    this.write_zone(J_CORSAIR_OUTER_OFFSET, MODE_STATIC, 0x28, 0x80);

    this.write_zone(
      ONBOARD_BASE_OFFSET,
      MSI_DIRECT_MODE,
      0x29 | SYNC_SETTING_JRGB,
      PER_LED_FULL_SYNC_MODE,
    );
    for idx in 1..ONBOARD_COUNT {
      let offset = ONBOARD_BASE_OFFSET + idx * ZONE_LEN;
      this.write_zone(offset, MODE_STATIC, 0x28, 0x80);
    }

    this.write_zone(J_RGB2_OFFSET, MODE_STATIC, 0x2A, 0x80);

    this.bytes[SAVE_DATA_OFFSET] = 0;
    this
  }

  fn write_zone(&mut self, offset: usize, effect: u8, speed: u8, color_flags: u8) {
    let zone = &mut self.bytes[offset..offset + ZONE_LEN];
    zone[0] = effect;
    zone[4] = speed;
    zone[8] = color_flags;
  }

  fn write_rainbow_zone(&mut self, offset: usize, effect: u8, speed: u8, color_flags: u8, cycle: u8) {
    let zone = &mut self.bytes[offset..offset + RAINBOW_ZONE_LEN];
    zone[0] = effect;
    zone[4] = speed;
    zone[8] = color_flags;
    zone[10] = cycle;
  }

  fn as_bytes(&self) -> &[u8] {
    &self.bytes
  }
}

struct PerLedPacket {
  bytes: [u8; PER_LED_PACKET_LEN],
  zone_len: usize,
  active_len: usize,
}

impl PerLedPacket {
  fn jr1() -> Self {
    let mut bytes = [0u8; PER_LED_PACKET_LEN];
    bytes[0] = REPORT_ID_PER_LED;
    bytes[1] = 0x25;
    bytes[2] = 0x04;
    Self {
      bytes,
      zone_len: JRAINBOW1_MAX_LED_COUNT,
      active_len: JRAINBOW1_ACTIVE_LED_COUNT,
    }
  }

  fn jr2() -> Self {
    let mut bytes = [0u8; PER_LED_PACKET_LEN];
    bytes[0] = REPORT_ID_PER_LED;
    bytes[1] = 0x25;
    bytes[2] = 0x04;
    bytes[3] = 0x01;
    Self {
      bytes,
      zone_len: JRAINBOW2_MAX_LED_COUNT,
      active_len: JRAINBOW2_ACTIVE_LED_COUNT,
    }
  }

  fn set_led(&mut self, index: usize, color: [u8; 3]) {
    if index >= NUM_PER_LED_MODE_LEDS {
      return;
    }
    let base = 5 + index * 3;
    self.bytes[base] = color[0];
    self.bytes[base + 1] = color[1];
    self.bytes[base + 2] = color[2];
  }

  fn fill_constant(&mut self, color: [u8; 3]) {
    for idx in 0..self.active_len {
      self.set_led(idx, color);
    }
    for idx in self.active_len..self.zone_len {
      self.set_led(idx, [0, 0, 0]);
    }
    for idx in self.zone_len..NUM_PER_LED_MODE_LEDS {
      self.set_led(idx, [0, 0, 0]);
    }
  }

  fn fill_from_slice(&mut self, colors: &[[u8; 3]]) {
    let limit = colors.len().min(self.active_len);
    for (idx, &color) in colors.iter().take(limit).enumerate() {
      self.set_led(idx, color);
    }
    for idx in limit..self.active_len {
      self.set_led(idx, [0, 0, 0]);
    }
    for idx in self.active_len..self.zone_len {
      self.set_led(idx, [0, 0, 0]);
    }
    for idx in self.zone_len..NUM_PER_LED_MODE_LEDS {
      self.set_led(idx, [0, 0, 0]);
    }
  }

  fn clear(&mut self) {
    for idx in 0..NUM_PER_LED_MODE_LEDS {
      self.set_led(idx, [0, 0, 0]);
    }
  }

  fn has_content(&self) -> bool {
    self.active_len > 0
  }

  fn as_bytes(&self) -> &[u8] {
    &self.bytes
  }
}

struct SyncPacket {
  bytes: [u8; PER_LED_PACKET_LEN],
}

impl SyncPacket {
  fn new() -> Self {
    let mut bytes = [0u8; PER_LED_PACKET_LEN];
    bytes[0] = REPORT_ID_PER_LED;
    bytes[1] = 0x25;
    bytes[2] = 0x06;
    Self { bytes }
  }

  fn set_led(&mut self, index: usize, color: [u8; 3]) {
    if index >= NUM_PER_LED_MODE_LEDS {
      return;
    }
    let base = 5 + index * 3;
    self.bytes[base] = color[0];
    self.bytes[base + 1] = color[1];
    self.bytes[base + 2] = color[2];
  }

  fn fill_constant_range(&mut self, start: usize, count: usize, color: [u8; 3]) {
    for idx in 0..count {
      self.set_led(start + idx, color);
    }
  }

  fn fill_range_from_slice(&mut self, start: usize, count: usize, colors: &[[u8; 3]]) {
    let limit = count.min(colors.len());
    for (offset, &color) in colors.iter().take(limit).enumerate() {
      self.set_led(start + offset, color);
    }
    for idx in limit..count {
      self.set_led(start + idx, [0, 0, 0]);
    }
  }

  fn clear_tail(&mut self) {
    for idx in (SYNC_JRAINBOW2_OFFSET + SYNC_JRAINBOW2_COUNT)..NUM_PER_LED_MODE_LEDS {
      self.set_led(idx, [0, 0, 0]);
    }
  }

  fn as_bytes(&self) -> &[u8] {
    &self.bytes
  }
}

#[derive(thiserror::Error, Debug)]
pub enum MysticError {
  #[error("MSI Mystic 7D77 device not found")]
  DeviceNotFound,
  #[error("HID backend error: {0}")]
  Hid(#[from] HidError),
  #[error("I/O error: {0}")]
  Io(#[from] std::io::Error),
  #[error("task join error: {0}")]
  Join(#[from] tokio::task::JoinError),
  #[error("mutex poisoned")]
  Poisoned,
}

fn resolve_devnode(device: &Device) -> Result<PathBuf, MysticError> {
  match &device.id {
    DeviceId::DevPath(sys_path) => {
      let uevent_path = sys_path.join("uevent");
      let contents = std::fs::read_to_string(&uevent_path)?;
      let devname = parse_devname(&contents)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "DEVNAME missing in uevent"))?;
      Ok(Path::new("/dev").join(devname))
    }
    _ => Err(MysticError::DeviceNotFound),
  }
}

fn parse_devname(contents: &str) -> Option<&str> {
  contents.lines().find_map(|line| {
    let (key, value) = line.split_once('=')?;
    (key == "DEVNAME").then_some(value)
  })
}

async fn open_devnode(path: &Path) -> Result<File, MysticError> {
  let path = path.to_owned();
  let file = task::spawn_blocking(move || std::fs::OpenOptions::new().read(true).write(true).open(path)).await??;
  Ok(file)
}

fn hid_iocsfeature(len: usize) -> u32 {
  hid_ioc(IOC_WRITE | IOC_READ, b'H' as u32, 0x06, len)
}

fn hid_iocgfeature(len: usize) -> u32 {
  hid_ioc(IOC_WRITE | IOC_READ, b'H' as u32, 0x07, len)
}

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

fn hid_ioc(dir: u32, type_: u32, nr: u32, size: usize) -> u32 {
  (dir << IOC_DIRSHIFT) | (type_ << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | ((size as u32) << IOC_SIZESHIFT)
}

const PER_LED_HEADER_LEN: usize = 8;

fn hex_dump(bytes: &[u8]) -> String {
  const MAX_LEN: usize = 64;
  bytes
    .iter()
    .take(MAX_LEN)
    .map(|b| format!("{b:02X}"))
    .collect::<Vec<_>>()
    .join("")
}

fn rgb_from_slice(slice: Option<&[u8]>) -> [u8; 3] {
  let mut rgb = [0u8; 3];
  if let Some(values) = slice {
    for (dst, src) in rgb.iter_mut().zip(values.iter()) {
      *dst = *src;
    }
  }
  rgb
}

fn slice_to_array<const N: usize>(slice: &[u8]) -> [u8; N] {
  let mut buf = [0u8; N];
  let len = slice.len().min(N);
  buf[..len].copy_from_slice(&slice[..len]);
  buf
}

#[allow(dead_code)]
#[derive(Debug)]
struct FeatureReportState {
  report_id: u8,
  direct_mode_enabled: bool,
  onboard: ZoneSummary,
  sync: SyncTargets,
  jpipe1: ZoneSummary,
  jpipe2: ZoneSummary,
  jrainbow1: RainbowZoneSummary,
  jrainbow2: RainbowZoneSummary,
  corsair: CorsairZoneSummary,
  save_flag: u8,
}

impl From<&[u8; PACKET_LEN]> for FeatureReportState {
  fn from(bytes: &[u8; PACKET_LEN]) -> Self {
    let report_id = bytes[0];
    let jpipe1 = ZoneSummary::from(&bytes[J_PIPE1_OFFSET..J_PIPE1_OFFSET + ZONE_LEN]);
    let jpipe2 = ZoneSummary::from(&bytes[J_PIPE2_OFFSET..J_PIPE2_OFFSET + ZONE_LEN]);
    let jrainbow1 = RainbowZoneSummary::from(&bytes[J_RAINBOW1_OFFSET..J_RAINBOW1_OFFSET + RAINBOW_ZONE_LEN]);
    let jrainbow2 = RainbowZoneSummary::from(&bytes[J_RAINBOW2_OFFSET..J_RAINBOW2_OFFSET + RAINBOW_ZONE_LEN]);
    let corsair = CorsairZoneSummary::from(&bytes[J_CORSAIR_OFFSET..J_CORSAIR_OFFSET + RAINBOW_ZONE_LEN]);
    let onboard = ZoneSummary::from(&bytes[ONBOARD_BASE_OFFSET..ONBOARD_BASE_OFFSET + ZONE_LEN]);
    let sync = SyncTargets::from_flags(onboard.color_flags);
    let direct_mode_enabled = onboard.mode.raw == MSI_DIRECT_MODE;

    Self {
      report_id,
      direct_mode_enabled,
      onboard,
      sync,
      jpipe1,
      jpipe2,
      jrainbow1,
      jrainbow2,
      corsair,
      save_flag: bytes[SAVE_DATA_OFFSET],
    }
  }
}

#[allow(dead_code)]
#[derive(Debug)]
struct ZoneSummary {
  mode: ModeDesc,
  speed: u8,
  brightness: u8,
  color_flags: u8,
  primary: [u8; 3],
  secondary: [u8; 3],
}

impl From<&[u8]> for ZoneSummary {
  fn from(slice: &[u8]) -> Self {
    let zone = slice_to_array::<ZONE_LEN>(slice);
    let speed_and_brightness = zone[4];
    Self {
      mode: ModeDesc::new(zone[0]),
      speed: speed_and_brightness & 0x03,
      brightness: (speed_and_brightness >> 2) & 0x1F,
      color_flags: zone[8],
      primary: [zone[1], zone[2], zone[3]],
      secondary: [zone[5], zone[6], zone[7]],
    }
  }
}

#[allow(dead_code)]
#[derive(Debug)]
struct RainbowZoneSummary {
  zone: ZoneSummary,
  cycle: u8,
}

impl From<&[u8]> for RainbowZoneSummary {
  fn from(slice: &[u8]) -> Self {
    let cycle = slice.get(RAINBOW_ZONE_LEN - 1).copied().unwrap_or_default();
    Self {
      zone: ZoneSummary::from(slice),
      cycle,
    }
  }
}

#[allow(dead_code)]
#[derive(Debug)]
struct CorsairZoneSummary {
  mode: ModeDesc,
  color: [u8; 3],
  fan_flags: u8,
  quantity: u8,
  individual: bool,
}

impl From<&[u8]> for CorsairZoneSummary {
  fn from(slice: &[u8]) -> Self {
    let zone = slice_to_array::<RAINBOW_ZONE_LEN>(slice);
    Self {
      mode: ModeDesc::new(zone[0]),
      color: [zone[1], zone[2], zone[3]],
      fan_flags: zone[4],
      quantity: zone[5],
      individual: zone[10] != 0,
    }
  }
}

#[allow(dead_code)]
#[derive(Debug)]
struct SyncTargets {
  raw: u8,
  onboard: bool,
  jrainbow1: bool,
  jrainbow2: bool,
  jcorsair: bool,
  jpipe1: bool,
  jpipe2: bool,
  jrgb: bool,
}

impl SyncTargets {
  fn from_flags(flags: u8) -> Self {
    Self {
      raw: flags,
      onboard: flags & SYNC_SETTING_ONBOARD != 0,
      jrainbow1: flags & SYNC_SETTING_JRAINBOW1 != 0,
      jrainbow2: flags & SYNC_SETTING_JRAINBOW2 != 0,
      jcorsair: flags & SYNC_SETTING_JCORSAIR != 0,
      jpipe1: flags & SYNC_SETTING_JPIPE1 != 0,
      jpipe2: flags & SYNC_SETTING_JPIPE2 != 0,
      jrgb: flags & SYNC_SETTING_JRGB != 0,
    }
  }
}

#[allow(dead_code)]
#[derive(Debug)]
struct ModeDesc {
  raw: u8,
  label: &'static str,
}

impl ModeDesc {
  fn new(raw: u8) -> Self {
    let label = match raw {
      0x00 => "disabled",
      MODE_STATIC => "static",
      MSI_DIRECT_MODE => "direct",
      _ => "unknown",
    };
    Self { raw, label }
  }
}

#[allow(dead_code)]
#[derive(Debug)]
struct PerLedReadback {
  header: PerLedHeader,
  first_led: [u8; 3],
  last_led: [u8; 3],
  first_non_zero: [u8; 3],
  last_non_zero: [u8; 3],
  non_zero_leds: usize,
}

impl From<&[u8; PER_LED_PACKET_LEN]> for PerLedReadback {
  fn from(bytes: &[u8; PER_LED_PACKET_LEN]) -> Self {
    let header = PerLedHeader {
      report_id: bytes[0],
      hdr0: bytes.get(1).copied().unwrap_or_default(),
      hdr1: bytes.get(2).copied().unwrap_or_default(),
      hdr2: bytes.get(3).copied().unwrap_or_default(),
      hdr3: bytes.get(4).copied().unwrap_or_default(),
    };

    let led_bytes = &bytes[5..];
    let first_led = rgb_from_slice(led_bytes.get(0..3));
    let last_led = if led_bytes.len() >= 3 {
      let base = led_bytes.len() - 3;
      [led_bytes[base], led_bytes[base + 1], led_bytes[base + 2]]
    } else {
      [0, 0, 0]
    };

    let mut non_zero_leds = 0usize;
    let mut first_non_zero = None;
    let mut last_non_zero = None;

    for chunk in led_bytes.chunks_exact(3) {
      if chunk.iter().any(|&v| v != 0) {
        let rgb = [chunk[0], chunk[1], chunk[2]];
        if first_non_zero.is_none() {
          first_non_zero = Some(rgb);
        }
        last_non_zero = Some(rgb);
        non_zero_leds += 1;
      }
    }

    Self {
      header,
      first_led,
      last_led,
      first_non_zero: first_non_zero.unwrap_or(first_led),
      last_non_zero: last_non_zero.unwrap_or(last_led),
      non_zero_leds,
    }
  }
}

#[allow(dead_code)]
#[derive(Debug)]
struct PerLedHeader {
  report_id: u8,
  hdr0: u8,
  hdr1: u8,
  hdr2: u8,
  hdr3: u8,
}
