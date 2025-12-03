use std::{
  collections::{HashMap, VecDeque},
  time::Duration,
};

use color_eyre::{Result, eyre::WrapErr, eyre::eyre};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use daemoncomm::{
  Color, KinectCommand, KinectEvent, KinectStatus, LedCommand, MessageToClient, MessageToClientData,
  MessageToServerData, client::Hal1210Client,
};
use ratatui::{
  DefaultTerminal, Frame,
  layout::{Constraint, Direction, Layout, Rect},
  style::{Color as TuiColor, Modifier, Style},
  text::{Line, Span},
  widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use tokio::{
  runtime::Runtime,
  sync::mpsc::{self, error::TryRecvError},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod monitoring;

const HEADER_HEIGHT: u16 = 5;
const LOG_HEIGHT: u16 = 9;
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const LOG_CAPACITY: usize = 200;
const LOG_VISIBLE_LINES: usize = (LOG_HEIGHT as usize).saturating_sub(2);
const DEFAULT_FADE_OUT_DURATION_MS: u64 = 3_000;

fn main() -> Result<()> {
  monitoring::init_logger();

  color_eyre::install()?;
  let app = App::new()?;
  let terminal = ratatui::init();
  let result = app.run(terminal);
  ratatui::restore();
  result
}

pub struct App {
  running: bool,
  _runtime: Runtime,
  client: Option<Hal1210Client>,
  incoming: mpsc::UnboundedReceiver<MessageToClient>,
  manual_state: ManualModeState,
  idle_inhibit_state: IdleInhibitState,
  selected_command: usize,
  color_input: ColorInput,
  idle_timeout_input: DurationInput,
  fade_out_duration_input: DurationInput,
  logs: VecDeque<String>,
  pending: HashMap<Uuid, String>,
}

impl App {
  pub fn new() -> Result<Self> {
    let runtime = Runtime::new().wrap_err("failed to create tokio runtime")?;
    let cancel_token = CancellationToken::new();
    let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();
    let client = runtime
      .block_on(Hal1210Client::connect(incoming_tx, cancel_token))
      .map_err(|err| {
        tracing::error!("failed to connect to hal1210 daemon: {err}");
        err
      })?;

    let app = Self {
      running: false,
      _runtime: runtime,
      client: Some(client),
      incoming: incoming_rx,
      manual_state: ManualModeState::Unknown,
      idle_inhibit_state: IdleInhibitState::Unknown,
      selected_command: 0,
      color_input: ColorInput::new(),
      idle_timeout_input: DurationInput::new("Idle inhibit timeout", None),
      fade_out_duration_input: DurationInput::new("Fade out duration", Some(DEFAULT_FADE_OUT_DURATION_MS)),
      logs: VecDeque::with_capacity(LOG_CAPACITY),
      pending: HashMap::new(),
    };

    // app.request_manual_mode(true)?;
    Ok(app)
  }

  pub fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
    self.running = true;
    let loop_result = (|| -> Result<()> {
      while self.running {
        self.drain_incoming();
        terminal.draw(|frame| self.render(frame))?;
        self.read_input()?;
      }
      Ok(())
    })();
    self.shutdown();
    loop_result
  }

  fn read_input(&mut self) -> Result<()> {
    if event::poll(INPUT_POLL_INTERVAL)? {
      match event::read()? {
        Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key_event(key),
        Event::Mouse(_) => {}
        Event::Resize(_, _) => {}
        _ => {}
      }
    }
    Ok(())
  }

  fn render(&mut self, frame: &mut Frame) {
    let area = frame.area();
    let vertical = Layout::default()
      .direction(Direction::Vertical)
      .constraints([
        Constraint::Length(HEADER_HEIGHT),
        Constraint::Min(8),
        Constraint::Length(LOG_HEIGHT),
      ])
      .split(area);

    self.render_header(frame, vertical[0]);
    self.render_body(frame, vertical[1]);
    self.render_logs(frame, vertical[2]);
  }

  fn render_header(&self, frame: &mut Frame, area: Rect) {
    let (status_label, status_style) = self.manual_state.label_and_style();
    let (idle_label, idle_style) = self.idle_inhibit_state.label_and_style();
    let idle_timeout = self.idle_timeout_input.formatted();
    let lines = vec![
      Line::from(vec![
        Span::styled("Manual mode: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(status_label, status_style),
      ]),
      Line::from(vec![
        Span::styled("Idle inhibit: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(idle_label, idle_style),
        Span::raw(" · timeout: "),
        Span::styled(idle_timeout, Style::default().fg(TuiColor::Gray)),
      ]),
      Line::from(format!("Pending requests: {}", self.pending.len())),
      Line::from("Press Enter to send · 'm' toggles manual mode · 'i' toggles idle inhibit · Esc/q exits"),
    ];

    frame.render_widget(
      Paragraph::new(lines)
        .block(Block::default().title("hal1210 manual control").borders(Borders::ALL))
        .wrap(Wrap { trim: true }),
      area,
    );
  }

  fn render_body(&mut self, frame: &mut Frame, area: Rect) {
    let chunks = Layout::default()
      .direction(Direction::Horizontal)
      .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
      .split(area);

    self.render_command_list(frame, chunks[0]);
    self.render_command_detail(frame, chunks[1]);
  }

  fn render_command_list(&mut self, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = COMMANDS
      .iter()
      .enumerate()
      .map(|(idx, command)| {
        let mut content = command.label.to_string();
        if !command.supported {
          content.push_str(" (unsupported)");
        }
        if command.needs_color() {
          content.push_str(" · color");
        }
        let mut item = ListItem::new(content);
        if !command.supported {
          item = item.style(Style::default().fg(TuiColor::DarkGray));
        } else if idx == self.selected_command {
          item = item.style(Style::default().fg(TuiColor::Cyan));
        }
        item
      })
      .collect();

    let list = List::new(items)
      .block(Block::default().title("Messages").borders(Borders::ALL))
      .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    state.select(Some(self.selected_command));
    frame.render_stateful_widget(list, area, &mut state);
  }

  fn render_command_detail(&self, frame: &mut Frame, area: Rect) {
    let descriptor = self.selected_descriptor();
    let mut lines = vec![Line::from(descriptor.description)];

    if !descriptor.supported {
      lines.push(Line::from(
        "This command requires uploading a full LED frame and is not yet supported.",
      ));
    }

    if descriptor.needs_color() {
      let (color_style, hint) = if self.color_input.is_complete() {
        (
          Style::default().fg(TuiColor::LightCyan),
          "Press 'e' to edit the #RRGGBB value.",
        )
      } else {
        (
          Style::default().fg(TuiColor::LightRed),
          "Enter a full #RRGGBB value by pressing 'e'.",
        )
      };

      lines.push(Line::from(vec![
        Span::styled("Color: ", Style::default().fg(TuiColor::Gray)),
        Span::styled(self.color_input.formatted(), color_style),
      ]));

      if self.color_input.editing {
        lines.push(Line::from(
          "Editing color — type hex digits, Backspace to delete, Enter to confirm.",
        ));
      } else {
        lines.push(Line::from(hint));
      }
    }

    if let Some(field) = descriptor.duration_field() {
      let input = self.duration_input(field);
      let value_style = if input.value().is_some() {
        Style::default().fg(TuiColor::LightCyan)
      } else {
        Style::default().fg(TuiColor::Gray)
      };
      lines.push(Line::from(vec![
        Span::styled(format!("{}: ", input.label()), Style::default().fg(TuiColor::Gray)),
        Span::styled(input.formatted(), value_style),
      ]));

      if input.is_editing() {
        lines.push(Line::from(
          "Editing duration — type digits, Backspace to delete, Enter to confirm.",
        ));
      } else {
        lines.push(Line::from(
          "Press 't' to edit duration · Leave blank to send no override.",
        ));
      }
    }

    if descriptor.supported {
      lines.push(Line::from("Press Enter to send the highlighted message."));
    } else {
      lines.push(Line::from("Selection is informational only."));
    }

    lines.push(Line::from(
      "Arrow keys move selection · 'm' toggles manual mode · 'i' toggles idle inhibit · 't' edits duration when available.",
    ));

    frame.render_widget(
      Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Details").borders(Borders::ALL)),
      area,
    );
  }

  fn render_logs(&self, frame: &mut Frame, area: Rect) {
    let mut lines: Vec<Line> = self
      .logs
      .iter()
      .rev()
      .take(LOG_VISIBLE_LINES)
      .cloned()
      .map(Line::from)
      .collect();
    lines.reverse();
    if lines.is_empty() {
      lines.push(Line::from("Activity will appear here."));
    }

    frame.render_widget(
      Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Activity").borders(Borders::ALL)),
      area,
    );
  }

  fn read_pending_color(&self) -> Option<Color> {
    self.color_input.color()
  }

  fn selected_descriptor(&self) -> &'static CommandDescriptor {
    COMMANDS.get(self.selected_command).unwrap_or_else(|| &COMMANDS[0])
  }

  fn duration_input(&self, field: DurationField) -> &DurationInput {
    match field {
      DurationField::IdleInhibit => &self.idle_timeout_input,
      DurationField::FadeOut => &self.fade_out_duration_input,
    }
  }

  fn duration_input_mut(&mut self, field: DurationField) -> &mut DurationInput {
    match field {
      DurationField::IdleInhibit => &mut self.idle_timeout_input,
      DurationField::FadeOut => &mut self.fade_out_duration_input,
    }
  }

  fn on_key_event(&mut self, key: KeyEvent) {
    if self.color_input.handle_key(key) {
      return;
    }

    if self.handle_duration_input_key(key) {
      return;
    }

    match (key.modifiers, key.code) {
      (_, KeyCode::Esc | KeyCode::Char('q')) | (KeyModifiers::CONTROL, KeyCode::Char('c') | KeyCode::Char('C')) => {
        self.quit()
      }
      (_, KeyCode::Up) => self.move_selection(-1),
      (_, KeyCode::Down) => self.move_selection(1),
      (_, KeyCode::PageUp) => self.move_selection(-3),
      (_, KeyCode::PageDown) => self.move_selection(3),
      (_, KeyCode::Home) => self.selected_command = 0,
      (_, KeyCode::End) => self.selected_command = COMMANDS.len().saturating_sub(1),
      (_, KeyCode::Enter) => self.send_selected_command(),
      (_, KeyCode::Char('m')) => {
        if matches!(
          self.manual_state,
          ManualModeState::PendingEnable | ManualModeState::PendingDisable
        ) {
          return;
        }

        let target = !matches!(self.manual_state, ManualModeState::Disabled);
        if target {
          if let Err(err) = self.request_manual_mode(false) {
            self.push_error(err.to_string());
          }
        } else if let Err(err) = self.request_manual_mode(true) {
          self.push_error(err.to_string());
        }
      }
      (_, KeyCode::Char('i')) => {
        if matches!(
          self.idle_inhibit_state,
          IdleInhibitState::PendingEnable | IdleInhibitState::PendingDisable
        ) {
          return;
        }

        let disable = matches!(self.idle_inhibit_state, IdleInhibitState::Enabled);
        if disable {
          if let Err(err) = self.request_idle_inhibit(false, None) {
            self.push_error(err.to_string());
          }
        } else {
          let timeout = self.idle_timeout_input.value();
          if let Err(err) = self.request_idle_inhibit(true, timeout) {
            self.push_error(err.to_string());
          }
        }
      }
      (_, KeyCode::Char('e')) => {
        if self.selected_descriptor().needs_color() {
          self.color_input.toggle_editing();
        }
      }
      (_, KeyCode::Char('t')) => {
        if let Some(field) = self.selected_descriptor().duration_field() {
          match field {
            DurationField::IdleInhibit => self.fade_out_duration_input.stop_editing(),
            DurationField::FadeOut => self.idle_timeout_input.stop_editing(),
          }
          self.duration_input_mut(field).toggle_editing();
        }
      }
      _ => {}
    }
  }

  fn handle_duration_input_key(&mut self, key: KeyEvent) -> bool {
    let mut handled = false;
    if self.idle_timeout_input.is_editing() {
      handled |= self.idle_timeout_input.handle_key(key);
    }
    if self.fade_out_duration_input.is_editing() {
      handled |= self.fade_out_duration_input.handle_key(key);
    }
    handled
  }

  fn send_selected_command(&mut self) {
    let descriptor = self.selected_descriptor();
    if !descriptor.supported {
      self.push_error("That command is not supported in this UI yet.");
      return;
    }

    match descriptor.kind {
      CommandKind::GetManualMode => self.send_or_log("Get manual mode", MessageToServerData::GetManualMode),
      CommandKind::Manual(enabled) => {
        if let Err(err) = self.request_manual_mode(enabled) {
          self.push_error(err.to_string());
        }
      }
      CommandKind::GetIdleInhibit => {
        self.send_or_log("Get idle inhibit", MessageToServerData::GetIdleInhibit);
      }
      CommandKind::IdleInhibit(enabled) => {
        let timeout = if enabled { self.idle_timeout_input.value() } else { None };
        if let Err(err) = self.request_idle_inhibit(enabled, timeout) {
          self.push_error(err.to_string());
        }
      }
      CommandKind::Kinect(KinectCommandKind::RequestStatus) => {
        self.send_or_log(
          "Get Kinect status",
          MessageToServerData::Kinect(KinectCommand::RequestStatus),
        );
      }
      CommandKind::Led(LedCommandKind::SetStaticColor) => {
        if let Some(color) = self.read_pending_color() {
          self.send_or_log(
            "Set static color",
            MessageToServerData::Led(LedCommand::SetStaticColor(color)),
          );
        } else {
          self.push_error("Provide a full #RRGGBB color before sending.");
        }
      }
      CommandKind::Led(LedCommandKind::Breathing) => {
        if let Some(color) = self.read_pending_color() {
          self.send_or_log(
            "Breathing effect",
            MessageToServerData::Led(LedCommand::Breathing(color)),
          );
        } else {
          self.push_error("Provide a full #RRGGBB color before sending.");
        }
      }
      CommandKind::Led(LedCommandKind::FadeOut) => {
        let duration_ms = self.fade_out_duration_input.value();
        self.send_or_log(
          "Fade out",
          MessageToServerData::Led(LedCommand::FadeOut { duration_ms }),
        );
      }
      CommandKind::Led(LedCommandKind::Rainbow) => {
        self.send_or_log("Rainbow", MessageToServerData::Led(LedCommand::Rainbow));
      }
      CommandKind::Led(LedCommandKind::SetStripState | LedCommandKind::FadeIn) => {
        self.push_error("Full strip uploads are not supported yet.");
      }
    }
  }

  fn move_selection(&mut self, delta: isize) {
    let len = COMMANDS.len() as isize;
    let mut idx = self.selected_command as isize + delta;
    if idx < 0 {
      idx = 0;
    } else if idx >= len {
      idx = len - 1;
    }
    self.selected_command = idx as usize;
  }

  fn send_or_log(&mut self, label: &str, data: MessageToServerData) {
    if let Err(err) = self.try_send(label, data) {
      self.push_error(err.to_string());
    }
  }

  fn try_send(&mut self, label: &str, data: MessageToServerData) -> Result<Uuid> {
    let client = self.client.as_ref().ok_or_else(|| eyre!("client disconnected"))?;
    let id = client
      .send(data)
      .map_err(|err| eyre!("failed to send {label}: {err}"))?;
    self.pending.insert(id, label.to_string());
    self.push_log(format!("sent {label} ({id})"));
    Ok(id)
  }

  fn request_manual_mode(&mut self, enabled: bool) -> Result<()> {
    let label = if enabled {
      "Enable manual mode"
    } else {
      "Disable manual mode"
    };
    self.try_send(label, MessageToServerData::SetManualMode { enabled })?;
    self.manual_state = if enabled {
      ManualModeState::PendingEnable
    } else {
      ManualModeState::PendingDisable
    };
    Ok(())
  }

  fn request_idle_inhibit(&mut self, enabled: bool, timeout_ms: Option<u64>) -> Result<()> {
    let label = if enabled {
      "Enable idle inhibit"
    } else {
      "Disable idle inhibit"
    };
    self.try_send(label, MessageToServerData::SetIdleInhibit { enabled, timeout_ms })?;
    self.idle_inhibit_state = if enabled {
      IdleInhibitState::PendingEnable
    } else {
      IdleInhibitState::PendingDisable
    };
    Ok(())
  }

  fn drain_incoming(&mut self) {
    loop {
      match self.incoming.try_recv() {
        Ok(msg) => self.handle_incoming_message(msg),
        Err(TryRecvError::Empty) => break,
        Err(TryRecvError::Disconnected) => {
          self.push_error("Connection to daemon closed.");
          self.running = false;
          break;
        }
      }
    }
  }

  fn handle_incoming_message(&mut self, msg: MessageToClient) {
    match msg.data {
      MessageToClientData::Ack => {
        if let Some(label) = self.pending.remove(&msg.id) {
          self.push_log(format!("ack: {label} ({})", msg.id));
        } else {
          self.push_log(format!("ack: {}", msg.id));
        }
      }
      MessageToClientData::Nack { reason } => {
        if let Some(label) = self.pending.remove(&msg.id) {
          self.push_error(format!("nack for {label}: {reason}"));
        } else {
          self.push_error(format!("nack {}: {reason}", msg.id));
        }
      }
      MessageToClientData::ManualMode { enabled } => {
        self.manual_state = if enabled {
          ManualModeState::Enabled
        } else {
          ManualModeState::Disabled
        };
        self.push_log(format!(
          "daemon reports manual mode {}",
          if enabled { "enabled" } else { "disabled" }
        ));
      }
      MessageToClientData::IdleInhibit { enabled, timeout_ms } => {
        self.idle_inhibit_state = if enabled {
          IdleInhibitState::Enabled
        } else {
          IdleInhibitState::Disabled
        };
        let mut message = format!(
          "daemon reports idle inhibit {}",
          if enabled { "enabled" } else { "disabled" }
        );
        if let Some(ms) = timeout_ms {
          message.push_str(&format!(" (timeout: {ms} ms)"));
        }
        self.push_log(message);
      }
      MessageToClientData::Kinect(notification) => {
        self.handle_kinect_notification(notification);
      }
    }
  }

  fn handle_kinect_notification(&mut self, notification: KinectEvent) {
    match notification {
      KinectEvent::Status(status) => {
        self.push_log(format!("kinect status · {}", Self::format_kinect_status(&status)));
      }
    }
  }

  fn format_kinect_status(status: &KinectStatus) -> String {
    let mut parts = vec![if status.connected { "connected" } else { "disconnected" }.to_string()];

    if let Some(serial) = &status.device_serial {
      parts.push(format!("serial={serial}"));
    }

    if let Some(fw) = &status.firmware_version {
      parts.push(format!("fw={fw}"));
    }

    parts.push(format!("depth={}", if status.depth_active { "on" } else { "off" }));
    parts.push(format!("rgb={}", if status.rgb_active { "on" } else { "off" }));

    parts.push(format!("status={}", status.status));
    parts.join(" · ")
  }

  fn push_log(&mut self, message: impl Into<String>) {
    if self.logs.len() == LOG_CAPACITY {
      self.logs.pop_front();
    }
    self.logs.push_back(message.into());
  }

  fn push_error(&mut self, message: impl Into<String>) {
    let msg = message.into();
    // tracing::error!("{msg}");
    self.push_log(format!("error: {msg}"));
  }

  fn quit(&mut self) {
    self.running = false;
  }

  fn shutdown(&mut self) {
    let should_disable = matches!(
      self.manual_state,
      ManualModeState::Enabled | ManualModeState::PendingEnable | ManualModeState::PendingDisable
    );

    if should_disable && let Err(err) = self.request_manual_mode(false) {
      self.push_error(format!("failed to disable manual mode: {err}"));
    }

    let should_clear_idle = matches!(
      self.idle_inhibit_state,
      IdleInhibitState::Enabled | IdleInhibitState::PendingEnable | IdleInhibitState::PendingDisable
    );

    if should_clear_idle && let Err(err) = self.request_idle_inhibit(false, None) {
      self.push_error(format!("failed to disable idle inhibit: {err}"));
    }
    if let Some(client) = self.client.take() {
      client.cancel();
    }
  }
}

#[derive(Debug, Clone, Copy)]
enum ManualModeState {
  Unknown,
  PendingEnable,
  PendingDisable,
  Enabled,
  Disabled,
}

impl ManualModeState {
  fn label_and_style(self) -> (&'static str, Style) {
    match self {
      ManualModeState::Unknown => ("unknown", Style::default().fg(TuiColor::Yellow)),
      ManualModeState::PendingEnable => ("requesting control", Style::default().fg(TuiColor::LightYellow)),
      ManualModeState::PendingDisable => ("releasing control", Style::default().fg(TuiColor::LightYellow)),
      ManualModeState::Enabled => ("enabled", Style::default().fg(TuiColor::Green)),
      ManualModeState::Disabled => ("disabled", Style::default().fg(TuiColor::Red)),
    }
  }
}

#[derive(Debug, Clone, Copy)]
enum IdleInhibitState {
  Unknown,
  PendingEnable,
  PendingDisable,
  Enabled,
  Disabled,
}

impl IdleInhibitState {
  fn label_and_style(self) -> (&'static str, Style) {
    match self {
      IdleInhibitState::Unknown => ("unknown", Style::default().fg(TuiColor::Yellow)),
      IdleInhibitState::PendingEnable => ("enabling", Style::default().fg(TuiColor::LightYellow)),
      IdleInhibitState::PendingDisable => ("disabling", Style::default().fg(TuiColor::LightYellow)),
      IdleInhibitState::Enabled => ("enabled", Style::default().fg(TuiColor::Green)),
      IdleInhibitState::Disabled => ("disabled", Style::default().fg(TuiColor::Red)),
    }
  }
}

struct ColorInput {
  buffer: String,
  editing: bool,
}

impl ColorInput {
  fn new() -> Self {
    Self {
      buffer: String::from("FF8800"),
      editing: false,
    }
  }

  fn formatted(&self) -> String {
    let mut value = self.buffer.clone();
    while value.len() < 6 {
      value.push('_');
    }
    format!("#{}", value)
  }

  fn color(&self) -> Option<Color> {
    if self.buffer.len() != 6 {
      return None;
    }
    let r = u8::from_str_radix(&self.buffer[0..2], 16).ok()?;
    let g = u8::from_str_radix(&self.buffer[2..4], 16).ok()?;
    let b = u8::from_str_radix(&self.buffer[4..6], 16).ok()?;
    Some([r, g, b])
  }

  fn is_complete(&self) -> bool {
    self.buffer.len() == 6
  }

  fn toggle_editing(&mut self) {
    self.editing = !self.editing;
  }

  fn handle_key(&mut self, key: KeyEvent) -> bool {
    if !self.editing {
      return false;
    }
    match key.code {
      KeyCode::Enter | KeyCode::Esc => {
        self.editing = false;
        true
      }
      KeyCode::Backspace => {
        self.buffer.pop();
        true
      }
      KeyCode::Char(ch) => {
        if ch.is_ascii_hexdigit() && self.buffer.len() < 6 {
          self.buffer.push(ch.to_ascii_uppercase());
        }
        true
      }
      _ => false,
    }
  }
}

struct DurationInput {
  label: &'static str,
  buffer: String,
  editing: bool,
  max_digits: usize,
}

impl DurationInput {
  fn new(label: &'static str, default_ms: Option<u64>) -> Self {
    Self {
      label,
      buffer: default_ms.map(|value| value.to_string()).unwrap_or_default(),
      editing: false,
      max_digits: 9,
    }
  }

  fn formatted(&self) -> String {
    if self.buffer.is_empty() {
      "none".to_string()
    } else {
      format!("{} ms", self.buffer)
    }
  }

  fn value(&self) -> Option<u64> {
    if self.buffer.is_empty() {
      None
    } else {
      self.buffer.parse().ok()
    }
  }

  fn toggle_editing(&mut self) {
    self.editing = !self.editing;
  }

  fn stop_editing(&mut self) {
    self.editing = false;
  }

  fn label(&self) -> &'static str {
    self.label
  }

  fn is_editing(&self) -> bool {
    self.editing
  }

  fn handle_key(&mut self, key: KeyEvent) -> bool {
    if !self.editing {
      return false;
    }

    match key.code {
      KeyCode::Enter | KeyCode::Esc => {
        self.editing = false;
        true
      }
      KeyCode::Backspace => {
        self.buffer.pop();
        true
      }
      KeyCode::Char(ch) if ch.is_ascii_digit() => {
        if self.buffer.len() < self.max_digits {
          self.buffer.push(ch);
        }
        true
      }
      _ => true,
    }
  }
}

#[derive(Clone, Copy)]
struct CommandDescriptor {
  label: &'static str,
  description: &'static str,
  kind: CommandKind,
  supported: bool,
  duration: Option<DurationField>,
}

impl CommandDescriptor {
  const fn needs_color(&self) -> bool {
    matches!(
      self.kind,
      CommandKind::Led(LedCommandKind::SetStaticColor | LedCommandKind::Breathing)
    )
  }

  const fn duration_field(&self) -> Option<DurationField> {
    self.duration
  }
}

#[derive(Clone, Copy)]
enum CommandKind {
  GetManualMode,
  Manual(bool),
  GetIdleInhibit,
  IdleInhibit(bool),
  Led(LedCommandKind),
  Kinect(KinectCommandKind),
}

#[derive(Clone, Copy)]
enum LedCommandKind {
  SetStaticColor,
  SetStripState,
  FadeIn,
  FadeOut,
  Rainbow,
  Breathing,
}

#[derive(Clone, Copy)]
enum DurationField {
  IdleInhibit,
  FadeOut,
}

#[derive(Clone, Copy)]
enum KinectCommandKind {
  RequestStatus,
}

const COMMANDS: &[CommandDescriptor] = &[
  CommandDescriptor {
    label: "Get manual mode",
    description: "Request the daemon's view of manual mode.",
    kind: CommandKind::GetManualMode,
    supported: true,
    duration: None,
  },
  CommandDescriptor {
    label: "Enable manual mode",
    description: "Ask the daemon to cede LED control to this console.",
    kind: CommandKind::Manual(true),
    supported: true,
    duration: None,
  },
  CommandDescriptor {
    label: "Disable manual mode",
    description: "Cede control back to the daemon.",
    kind: CommandKind::Manual(false),
    supported: true,
    duration: None,
  },
  CommandDescriptor {
    label: "Get idle inhibit",
    description: "Request the daemon's view of idle inhibit state.",
    kind: CommandKind::GetIdleInhibit,
    supported: true,
    duration: None,
  },
  CommandDescriptor {
    label: "Enable idle inhibit",
    description: "Prevent idle events from fading the LEDs.",
    kind: CommandKind::IdleInhibit(true),
    supported: true,
    duration: Some(DurationField::IdleInhibit),
  },
  CommandDescriptor {
    label: "Disable idle inhibit",
    description: "Allow the daemon to fade out when idle again.",
    kind: CommandKind::IdleInhibit(false),
    supported: true,
    duration: None,
  },
  CommandDescriptor {
    label: "Get Kinect status",
    description: "Query the daemon for Kinect availability and health.",
    kind: CommandKind::Kinect(KinectCommandKind::RequestStatus),
    supported: true,
    duration: None,
  },
  CommandDescriptor {
    label: "Set static color",
    description: "Fill every LED with the specified #RRGGBB color.",
    kind: CommandKind::Led(LedCommandKind::SetStaticColor),
    supported: true,
    duration: None,
  },
  CommandDescriptor {
    label: "Breathing effect",
    description: "Pulse the selected color at the configured brightness envelope.",
    kind: CommandKind::Led(LedCommandKind::Breathing),
    supported: true,
    duration: None,
  },
  CommandDescriptor {
    label: "Fade out",
    description: "Quickly fade all LEDs to black.",
    kind: CommandKind::Led(LedCommandKind::FadeOut),
    supported: true,
    duration: Some(DurationField::FadeOut),
  },
  CommandDescriptor {
    label: "Rainbow",
    description: "Start the rainbow animation shipped with the daemon.",
    kind: CommandKind::Led(LedCommandKind::Rainbow),
    supported: true,
    duration: None,
  },
  CommandDescriptor {
    label: "Set strip state (full frame)",
    description: "Upload a custom LED frame. Not yet supported in this UI.",
    kind: CommandKind::Led(LedCommandKind::SetStripState),
    supported: false,
    duration: None,
  },
  CommandDescriptor {
    label: "Fade in frame (full frame)",
    description: "Fade into a provided LED frame. Not yet supported in this UI.",
    kind: CommandKind::Led(LedCommandKind::FadeIn),
    supported: false,
    duration: None,
  },
];
