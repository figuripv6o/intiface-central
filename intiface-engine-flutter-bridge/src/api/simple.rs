use crate::{
  in_process_frontend::FlutterIntifaceEngineFrontend,
  logging::FlutterTracingWriter,
  mobile_init,
};
use anyhow::Result;
pub use buttplug::{
  server::device::configuration::WebsocketSpecifier,
  util::device_configuration::UserConfigDeviceIdentifier,
};
pub use buttplug::{
  core::message::DeviceFeature, server::device::{configuration::{ProtocolAttributesIdentifier, ProtocolDeviceFeatures}, protocol::get_default_protocol_map, ServerDeviceIdentifier}, util::device_configuration::{
    load_protocol_configs, load_user_configs, ProtocolConfiguration, ProtocolDefinition,
    UserConfigDefinition, UserDeviceConfig, UserDeviceConfigPair,
  }
};
pub use buttplug::core::message::{ButtplugDeviceMessageType, DeviceFeatureActuator, DeviceFeatureSensor, FeatureType};

use crate::frb_generated::StreamSink;
use flutter_rust_bridge::frb;
use futures::{pin_mut, StreamExt};
use lazy_static::lazy_static;
use once_cell::sync::OnceCell;
use sentry::ClientInitGuard;
pub use std::{
  collections::{HashMap, HashSet}, ops::RangeInclusive, sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
  }, thread, time::Duration
};
use tokio::{
  select,
  sync::{broadcast, Notify}, runtime::Runtime,
};
use tracing_futures::Instrument;

pub use intiface_engine::{EngineOptions, EngineOptionsExternal, IntifaceEngine, IntifaceMessage};

static CRASH_REPORTING: OnceCell<ClientInitGuard> = OnceCell::new();
static ENGINE_NOTIFIER: OnceCell<Arc<Notify>> = OnceCell::new();
lazy_static! {
  static ref RUNTIME: Arc<Mutex<Option<Runtime>>> = Arc::new(Mutex::new(None));
  static ref LOGGER: Arc<Mutex<Option<FlutterTracingWriter>>> = Arc::new(Mutex::new(None));
  static ref RUN_STATUS: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
  static ref ENGINE_BROADCASTER: Arc<broadcast::Sender<IntifaceMessage>> =
    Arc::new(broadcast::channel(255).0);
  static ref BACKDOOR_INCOMING_BROADCASTER: Arc<broadcast::Sender<String>> =
    Arc::new(broadcast::channel(255).0);
}

pub fn runtime_started() -> bool {
  RUNTIME.lock().unwrap().is_some()
}

pub fn run_engine(sink: StreamSink<String>, args: EngineOptionsExternal) -> Result<()> {

  if RUN_STATUS.load(Ordering::Relaxed) {
    return Err(anyhow::Error::msg("Server already running!"));
  }
  RUN_STATUS.store(true, Ordering::Relaxed);

  let mut runtime_storage = RUNTIME.lock().unwrap();

  if runtime_storage.is_some() {
    return Err(anyhow::Error::msg("Runtime already created!"));
  }

  let runtime = mobile_init::create_runtime(sink.clone())
    .expect("Runtime should work, otherwise we can't function.");

  if ENGINE_NOTIFIER.get().is_none() {
    ENGINE_NOTIFIER
      .set(Arc::new(Notify::new()))
      .expect("We already checked creation so this shouldn't fail");
  }

  let frontend = Arc::new(FlutterIntifaceEngineFrontend::new(
    sink.clone(),
    ENGINE_BROADCASTER.clone(),
  ));
  info!("Frontend logging set up.");
  let frontend_waiter = frontend.notify_on_creation();
  let engine = Arc::new(IntifaceEngine::default());
  let engine_clone = engine.clone();
  let engine_clone_clone = engine.clone();
  let notify = ENGINE_NOTIFIER.get().expect("Should be set").clone();
  let notify_clone = notify.clone();
  let notify_clone_clone = notify.clone();
  let options = args.into();

  let mut backdoor_incoming = BACKDOOR_INCOMING_BROADCASTER.subscribe();
  let outgoing_sink = sink.clone();
  let sink_clone = sink.clone();

  runtime.spawn(
    async move {
      info!("Entering main join.");
      tokio::join!(
        // Backdoor server task
        async move {
          // Once we finish our waiter, continue. If we cancel the server run before then, just kill the
          // task.
          info!("Entering backdoor waiter task");
          select! {
            _ = frontend_waiter => {
              // This firing means the frontend is set up, and we just want to continue to creating our backdoor server.
            }
            _ = notify_clone.notified() => {
              return;
            }
          };
          // At this point we know we'll have a server.
          let backdoor_server = if let Some(backdoor_server) = engine_clone.backdoor_server() {
            backdoor_server
          } else {
            // If we somehow *don't* have a server here, something has gone very wrong. Just die.
            return;
          };
          let backdoor_server_stream = backdoor_server.event_stream();
          pin_mut!(backdoor_server_stream);
          loop {
            select! {
              msg = backdoor_incoming.recv() => {
                match msg {
                  Ok(msg) => {
                    //let runtime = RUNTIME.get().expect("Runtime not initialized");
                    let sink = outgoing_sink.clone();
                    let backdoor_server_clone = backdoor_server.clone();
                    tokio::spawn(async move {
                      sink.add(backdoor_server_clone.parse_message(&msg).await);
                    });
                  }
                  Err(_) => break
                }
              },
              outgoing = backdoor_server_stream.next() => {
                match outgoing {
                  Some(msg) => { sink.add(msg); }
                  None => break
                }
              },
              _ = notify_clone.notified() => break
            }
          }
          info!("Exiting backdoor waiter task");
        }
        .instrument(info_span!("IC Backdoor server task")),
        // Main engine task.
        async move {
          info!("Entering main engine waiter task");
          if let Err(e) = engine.run(&options, Some(frontend)).await {
            error!("Error running engine: {:?}", e);
          }
          info!("Exiting main engine waiter task");
          notify_clone_clone.notify_waiters();
        }.instrument(info_span!("IC main engine task")),
        // Our notifier needs to run in a task by itself, because we don't want our engine future to get
        // cancelled, so we can't select between it and the notifier. It needs to shutdown gracefully.
        async move {
          info!("Entering engine stop notification task");
          notify.notified().await;
          info!("Notifier called, stopping engine");
          engine_clone_clone.stop();
        }
      );
      RUN_STATUS.store(false, Ordering::Relaxed);
      info!("Exiting main join.");
    }
    .instrument(info_span!("IC main engine task")),
  );
  *runtime_storage = Some(runtime);
  Ok(())
}

pub fn send(msg_json: String) {
  let msg: IntifaceMessage = serde_json::from_str(&msg_json).unwrap();
  if ENGINE_BROADCASTER.receiver_count() > 0 {
    ENGINE_BROADCASTER
      .send(msg)
      .expect("This should be infallible since we already checked for receivers");
  }
}

pub fn stop_engine() {
  info!("Stop engine called in rust.");
  if let Some(notifier) = ENGINE_NOTIFIER.get() {
    notifier.notify_waiters();
  }
  // Need to park ourselves real quick to let the other runtime threads finish out.
  //
  // HACK The android JNI drop calls (and sometimes windows UWP calls) are slow (100ms+) and need
  // quite a while to get everything disconnected if there are currently connected devices. If they
  // don't run to completion, the runtime won't shutdown properly and everything will stall. Running
  // runtime_shutdown() doesn't work here because these are all tasks that may be stalled at the OS
  // level so we don't have enough info. Waiting on this is not the optimal way to do it, but I also
  // don't have a good way to know when shutdown is finished right now. So waiting it is. 1s isn't
  // super noticable from an UX standpoint.
  thread::sleep(Duration::from_millis(500));
  let runtime;
  {
    runtime = RUNTIME.lock().unwrap().take();
  }
  if let Some(rt) = runtime {
    info!("Shutting down runtime");
    rt.shutdown_timeout(Duration::from_secs(1));
    info!("Runtime shutdown complete");
  }
  RUN_STATUS.store(false, Ordering::Relaxed);
}

pub fn send_backend_server_message(msg: String) {
  if BACKDOOR_INCOMING_BROADCASTER.receiver_count() > 0 {
    BACKDOOR_INCOMING_BROADCASTER
      .send(msg)
      .expect("This should be infallible since we already checked for receivers");
  }
}

#[frb(mirror(UserConfigDeviceIdentifier))]
pub struct _UserConfigDeviceIdentifier {
  #[allow(dead_code)]
  address: String,
  #[allow(dead_code)]
  protocol: String,
  #[allow(dead_code)]
  identifier: Option<String>,
}

pub struct ExposedWebsocketSpecifier {
  pub names: Vec<String>,
}

impl From<&WebsocketSpecifier> for ExposedWebsocketSpecifier {
  fn from(other: &WebsocketSpecifier) -> Self {
    ExposedWebsocketSpecifier {
      names: other.names().iter().cloned().collect(),
    }
  }
}

impl Into<WebsocketSpecifier> for ExposedWebsocketSpecifier {
  fn into(self) -> WebsocketSpecifier {
    WebsocketSpecifier::new(&self.names)
  }
}

pub struct ExposedUserDeviceSpecifiers {
  pub websocket: Option<ExposedWebsocketSpecifier>,
}

pub struct ExposedUserConfig {
  pub specifiers: Vec<(String, ExposedUserDeviceSpecifiers)>,
  pub configurations: Vec<ExposedUserDeviceConfig>,
}

impl Into<UserConfigDefinition> for ExposedUserConfig {
  fn into(self) -> UserConfigDefinition {
    let mut user_config_def = UserConfigDefinition::default();
    let configs: Vec<UserDeviceConfigPair> =
      self.configurations.into_iter().map(|x| x.into()).collect();
    let mut specifier_map: HashMap<String, ProtocolDefinition> = HashMap::new();
    self
      .specifiers
      .into_iter()
      .for_each(|(protocol, specifiers)| {
        if let Some(websocket_specifier) = specifiers.websocket {
          if websocket_specifier.names.len() > 0 {
            let mut protocol_def = ProtocolDefinition::default();
            protocol_def.set_websocket(Some(websocket_specifier.into()));
            specifier_map.insert(protocol, protocol_def);
          }
        }
      });
    //if !specifier_map.is_empty() {
    user_config_def.set_specifiers(Some(specifier_map));
    //}
    if !configs.is_empty() {
      user_config_def.set_user_device_configs(Some(configs));
    }
    user_config_def
  }
}

pub struct ExposedUserDeviceConfig {
  pub identifier: UserConfigDeviceIdentifier,
  pub name: String,
  pub display_name: Option<String>,
  pub allow: Option<bool>,
  pub deny: Option<bool>,
  pub reserved_index: Option<u32>,
}

impl From<&UserDeviceConfigPair> for ExposedUserDeviceConfig {
  fn from(value: &UserDeviceConfigPair) -> Self {
    Self {
      identifier: value.identifier().clone(),
      name: "".to_owned(),
      display_name: value.config().display_name().clone(),
      allow: value.config().allow().clone(),
      deny: value.config().deny().clone(),
      reserved_index: value.config().index().clone(),
    }
  }
}

impl Into<UserDeviceConfigPair> for ExposedUserDeviceConfig {
  fn into(self) -> UserDeviceConfigPair {
    let mut config = UserDeviceConfig::default();
    config.set_display_name(self.display_name);
    config.set_allow(self.allow);
    config.set_deny(self.deny);
    config.set_index(self.reserved_index);
    UserDeviceConfigPair::new(self.identifier, config)
  }
}

pub fn get_user_device_configs(
  device_config_json: String,
  user_config_json: String,
) -> ExposedUserConfig {
  let mut dcm_builder = load_protocol_configs(
    Some(device_config_json.to_owned()),
    Some(user_config_json.to_owned()),
    false,
  )
  .unwrap();
  let dcm = dcm_builder.finish().unwrap();
  let raw_user_configs = load_user_configs(&user_config_json);
  let mut config_out = vec![];
  let mut websocket_specifiers_out = Vec::new();
  if let Some(user_specifiers) = raw_user_configs.specifiers() {
    for (protocol, specifiers) in user_specifiers {
      if let Some(websocket_specifiers) = specifiers.websocket() {
        websocket_specifiers_out.push((
          protocol.clone(),
          ExposedUserDeviceSpecifiers {
            websocket: Some(ExposedWebsocketSpecifier::from(websocket_specifiers)),
          },
        ));
      }
    }
  }
  if let Some(configs) = raw_user_configs.user_device_configs() {
    for config in configs {
      let maybe_attrs = dcm.protocol_device_attributes(
        &ServerDeviceIdentifier::from(config.identifier().clone()),
        &[],
      );
      if let Some(attrs) = maybe_attrs {
        let mut user_config = ExposedUserDeviceConfig::from(*&config);
        user_config.name = attrs.name().to_owned();
        config_out.push(user_config)
      }
    }
  }
  ExposedUserConfig {
    specifiers: websocket_specifiers_out,
    configurations: config_out,
  }
}

pub fn generate_user_device_config_file(user_config: ExposedUserConfig) -> String {
  let mut config_file = ProtocolConfiguration::new(2, 0);
  let user_config_def: UserConfigDefinition = user_config.into();
  config_file.user_configs = Some(user_config_def);
  config_file.to_json()
}

pub fn get_protocol_names() -> Vec<String> {
  get_default_protocol_map()
    .keys()
    .into_iter()
    .cloned()
    .collect()
}

pub fn setup_logging(sink: StreamSink<String>) {
  // Default log to debug, we'll filter in UI if we need it.
  std::env::set_var(
    "RUST_LOG",
    format!("debug,h2=warn,reqwest=warn,rustls=warn,hyper=warn"),
  );
  *LOGGER.lock().unwrap() = Some(FlutterTracingWriter::new(sink));
}

pub fn shutdown_logging() {
  *LOGGER.lock().unwrap() = None;
}

pub fn crash_reporting(sentry_api_key: String) {
  // Set up Sentry
  info!("Initializing native crash reporting.");
  let _ = CRASH_REPORTING.set(sentry::init((
    sentry_api_key,
    sentry::ClientOptions {
      release: sentry::release_name!(),
      ..Default::default()
    },
  )));
  info!("Native crash reporting initialized");
}

pub fn get_user_configs() -> HashMap<ProtocolAttributesIdentifier, ProtocolDeviceFeatures> {
  HashMap::new()
}

/*
#[frb(mirror(ProtocolAttributesIdentifier))]
pub struct _ProtocolAttributesIdentifier {
  pub protocol: String,
  pub attributes_identifier: Option<String>,
  pub address: Option<String>,
}

#[frb(mirror(ProtocolDeviceFeatures))]
pub struct _ProtocolDeviceFeatures {
  /// Given name of the device this instance represents.
  name: Option<String>,
  /// User configured name of the device this instance represents, assuming one exists.
  display_name: Option<String>,
  /// Message attributes for this device instance.
  features: Vec<DeviceFeature>,
}

#[frb(mirror(FeatureType))]
pub enum _FeatureType {
  Unknown,
  Vibrate,
  // Single Direction Rotation Speed
  Rotate,
  Oscillate,
  Constrict,
  Inflate,
  // For instances where we specify a position to move to ASAP. Usually servos, probably for the
  // OSR-2/SR-6.
  Position,
  // Sensor Types
  Battery,
  RSSI,
  Button,
  Pressure,
  // Currently unused but possible sensor features:
  // Temperature,
  // Accelerometer,
  // Gyro,
  //
  // Raw Feature, for when raw messages are on
  Raw,
}

#[frb(mirror(DeviceFeature))]
pub struct _DeviceFeature {
  pub description: String,
  pub feature_type: FeatureType,
  pub actuator: Option<DeviceFeatureActuator>,
  pub sensor: Option<DeviceFeatureSensor>,
}

#[frb(mirror(DeviceFeatureActuator))]
pub struct _DeviceFeatureActuator {
  step_range: RangeInclusive<u32>,
  step_limit: Option<RangeInclusive<u32>>,
  messages: HashSet<ButtplugDeviceMessageType>
}

#[frb(mirror(DeviceFeatureSensor))]
pub struct _DeviceFeatureSensor {
  value_range: Vec<RangeInclusive<i32>>,
  messages: HashSet<ButtplugDeviceMessageType>
}

*/

#[frb(mirror(EngineOptionsExternal))]
pub struct _EngineOptionsExternal {
  pub device_config_json: Option<String>,
  pub user_device_config_json: Option<String>,
  pub server_name: String,
  pub websocket_use_all_interfaces: bool,
  pub websocket_port: Option<u16>,
  pub frontend_websocket_port: Option<u16>,
  pub frontend_in_process_channel: bool,
  pub max_ping_time: u32,
  pub allow_raw_messages: bool,
  pub use_bluetooth_le: bool,
  pub use_serial_port: bool,
  pub use_hid: bool,
  pub use_lovense_dongle_serial: bool,
  pub use_lovense_dongle_hid: bool,
  pub use_xinput: bool,
  pub use_lovense_connect: bool,
  pub use_device_websocket_server: bool,
  pub device_websocket_server_port: Option<u16>,
  pub crash_main_thread: bool,
  pub crash_task_thread: bool,
  pub websocket_client_address: Option<String>,
  pub broadcast_server_mdns: bool,
  pub mdns_suffix: Option<String>,
  pub repeater_mode: bool,
  pub repeater_local_port: Option<u16>,
  pub repeater_remote_address: Option<String>,
}
