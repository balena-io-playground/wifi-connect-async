use anyhow::{anyhow, bail, Context, Result};

use tokio::sync::oneshot;

use glib::translate::FromGlib;
use glib::{MainContext, MainLoop};

use std::cell::RefCell;
use std::collections::HashSet;
use std::future::Future;
use std::rc::Rc;

use serde::Serialize;

use crate::opts::Opts;

use nm::{
    utils_get_timestamp_msec, AccessPoint, ActiveConnection, ActiveConnectionExt,
    ActiveConnectionState, Cast, Client, Connection, ConnectionExt, Device, DeviceExt, DeviceState,
    DeviceType, DeviceWifi, IPAddress, SettingConnection, SettingIP4Config, SettingIPConfigExt,
    SettingWireless, SettingWirelessSecurity, SimpleConnection, SETTING_IP4_CONFIG_METHOD_MANUAL,
    SETTING_WIRELESS_MODE_AP, SETTING_WIRELESS_SETTING_NAME,
};

const WIFI_SCAN_TIMEOUT_SECONDS: usize = 45;

const NETWORK_THREAD_NOT_INITIALIZED: &str = "Network thread not yet initialized";

type TokioResponder = oneshot::Sender<Result<CommandResponce>>;

#[derive(Debug)]
pub enum Command {
    CheckConnectivity,
    ListConnections,
    ListWiFiNetworks,
    Shutdown,
    Stop,
}

pub struct CommandRequest {
    responder: TokioResponder,
    command: Command,
}

impl CommandRequest {
    pub fn new(responder: TokioResponder, command: Command) -> Self {
        Self { responder, command }
    }
}

pub enum CommandResponce {
    CheckConnectivity(Connectivity),
    ListConnections(ConnectionList),
    ListWiFiNetworks(NetworkList),
    Shutdown(Shutdown),
    Stop(Stop),
}

#[derive(Serialize)]
pub struct Connectivity {
    pub connectivity: String,
}

impl Connectivity {
    fn new(connectivity: String) -> Self {
        Self { connectivity }
    }
}

#[derive(Serialize)]
pub struct ConnectionList {
    pub connections: Vec<ConnectionDetails>,
}

impl ConnectionList {
    fn new(connections: Vec<ConnectionDetails>) -> Self {
        Self { connections }
    }
}

#[derive(Serialize)]
pub struct ConnectionDetails {
    pub id: String,
    pub uuid: String,
}

impl ConnectionDetails {
    fn new(id: String, uuid: String) -> Self {
        Self { id, uuid }
    }
}

#[derive(Serialize)]
pub struct NetworkList {
    pub stations: Vec<Station>,
}

impl NetworkList {
    fn new(stations: Vec<Station>) -> Self {
        Self { stations }
    }
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Station {
    pub ssid: String,
    pub quality: u8,
}

impl Station {
    fn new(ssid: String, quality: u8) -> Self {
        Self { ssid, quality }
    }
}

#[derive(Serialize)]
pub struct Shutdown {
    pub shutdown: &'static str,
}

impl Shutdown {
    fn new(shutdown: &'static str) -> Self {
        Self { shutdown }
    }
}

#[derive(Serialize)]
pub struct Stop {
    pub stop: &'static str,
}

impl Stop {
    fn new(stop: &'static str) -> Self {
        Self { stop }
    }
}

struct NetworkState {
    client: Client,
    _device: DeviceWifi,
    stations: Vec<Station>,
    portal_connection: Option<ActiveConnection>,
}

impl NetworkState {
    fn new(
        client: Client,
        _device: DeviceWifi,
        stations: Vec<Station>,
        portal_connection: Option<ActiveConnection>,
    ) -> Self {
        Self {
            client,
            _device,
            stations,
            portal_connection,
        }
    }
}

thread_local! {
    static GLOBAL: RefCell<Option<NetworkState>> = RefCell::new(None);
}

pub fn create_channel() -> (glib::Sender<CommandRequest>, glib::Receiver<CommandRequest>) {
    MainContext::channel(glib::PRIORITY_DEFAULT)
}

pub fn run_network_manager_loop(
    opts: Opts,
    initialized_sender: oneshot::Sender<Result<()>>,
    glib_receiver: glib::Receiver<CommandRequest>,
) {
    let context = MainContext::new();
    let loop_ = MainLoop::new(Some(&context), false);

    context
        .with_thread_default(|| {
            glib_receiver.attach(None, dispatch_command_requests);

            context.spawn_local(init_network_respond(opts, initialized_sender));

            loop_.run();
        })
        .unwrap();
}

async fn init_network_respond(opts: Opts, initialized_sender: oneshot::Sender<Result<()>>) {
    let init_result = init_network(opts).await;

    initialized_sender.send(init_result).ok();
}

async fn init_network(opts: Opts) -> Result<()> {
    let client = create_client().await?;

    delete_exising_wifi_connect_ap_profile(&client, &opts.ssid).await?;

    let device = find_device(&client, &opts.interface)?;

    let interface = device.clone().upcast::<Device>().iface().unwrap();

    println!("Interface: {}", interface);

    scan_wifi(&device).await?;

    let access_points = get_nearby_access_points(&device);

    let stations = access_points
        .iter()
        .map(|ap| Station::new(ap_ssid(ap), ap.strength()))
        .collect::<Vec<_>>();

    let portal_connection = Some(
        create_portal(&client, &device, &opts)
            .await
            .context("Failed to create captive portal")?,
    );

    GLOBAL.with(|global| {
        let state = NetworkState::new(client, device, stations, portal_connection);
        *global.borrow_mut() = Some(state);
    });

    println!("Network initilized");

    Ok(())
}

fn dispatch_command_requests(command_request: CommandRequest) -> glib::Continue {
    let CommandRequest { responder, command } = command_request;
    match command {
        Command::CheckConnectivity => spawn(check_connectivity(), responder),
        Command::ListConnections => spawn(list_connections(), responder),
        Command::ListWiFiNetworks => spawn(list_wifi_networks(), responder),
        Command::Shutdown => spawn(shutdown(), responder),
        Command::Stop => spawn(stop(), responder),
    };
    glib::Continue(true)
}

fn spawn(
    command_future: impl Future<Output = Result<CommandResponce>> + 'static,
    responder: TokioResponder,
) {
    let context = MainContext::ref_thread_default();
    context.spawn_local(execute_and_respond(command_future, responder));
}

async fn execute_and_respond(
    command_future: impl Future<Output = Result<CommandResponce>> + 'static,
    responder: TokioResponder,
) {
    let result = command_future.await;
    let _ = responder.send(result);
}

async fn check_connectivity() -> Result<CommandResponce> {
    let client = get_global_client()?;

    let connectivity = client
        .check_connectivity_future()
        .await
        .context("Failed to execute check connectivity")?;

    Ok(CommandResponce::CheckConnectivity(Connectivity::new(
        connectivity.to_string(),
    )))
}

async fn list_connections() -> Result<CommandResponce> {
    let client = get_global_client()?;

    let all_connections: Vec<_> = client
        .connections()
        .into_iter()
        .map(|c| c.upcast::<Connection>())
        .collect();

    let mut connections = Vec::new();

    for connection in all_connections {
        if let Some(setting_connection) = connection.setting_connection() {
            if let Some(id) = setting_connection.id() {
                if let Some(uuid) = setting_connection.uuid() {
                    connections.push(ConnectionDetails::new(id.to_string(), uuid.to_string()));
                }
            }
        }
    }

    Ok(CommandResponce::ListConnections(ConnectionList::new(
        connections,
    )))
}

async fn list_wifi_networks() -> Result<CommandResponce> {
    Ok(CommandResponce::ListWiFiNetworks(NetworkList::new(
        get_global_stations()?,
    )))
}

fn get_global_stations() -> Result<Vec<Station>> {
    GLOBAL.with(|global| {
        if let Some(ref state) = *global.borrow() {
            Ok(state.stations.clone())
        } else {
            Err(anyhow!(NETWORK_THREAD_NOT_INITIALIZED))
        }
    })
}

fn get_global_portal_connection() -> Result<Option<ActiveConnection>> {
    GLOBAL.with(|global| {
        if let Some(ref state) = *global.borrow() {
            Ok(state.portal_connection.clone())
        } else {
            Err(anyhow!(NETWORK_THREAD_NOT_INITIALIZED))
        }
    })
}

fn get_global_client() -> Result<Client> {
    GLOBAL.with(|global| {
        if let Some(ref state) = *global.borrow() {
            Ok(state.client.clone())
        } else {
            Err(anyhow!(NETWORK_THREAD_NOT_INITIALIZED))
        }
    })
}

async fn shutdown() -> Result<CommandResponce> {
    Ok(CommandResponce::Shutdown(Shutdown::new("ok")))
}

async fn stop() -> Result<CommandResponce> {
    let client = get_global_client()?;

    if let Some(active_connection) = get_global_portal_connection()? {
        stop_portal(&client, &active_connection).await?;
    }

    Ok(CommandResponce::Stop(Stop::new("ok")))
}

async fn scan_wifi(device: &DeviceWifi) -> Result<()> {
    println!("Scanning for networks...");

    let prescan = utils_get_timestamp_msec();

    device
        .request_scan_future()
        .await
        .context("Failed to request WiFi scan")?;

    for _ in 0..WIFI_SCAN_TIMEOUT_SECONDS {
        if prescan < device.last_scan() {
            break;
        }

        glib::timeout_future_seconds(1).await;
    }

    Ok(())
}

fn get_nearby_access_points(device: &DeviceWifi) -> Vec<AccessPoint> {
    let mut access_points = device.access_points();

    // Purge non-string SSIDs
    access_points.retain(|ap| ssid_to_string(ap.ssid()).is_some());

    // Sort access points by signal strength first and then ssid
    access_points.sort_by_key(|ap| (ap.strength(), ap_ssid(ap)));
    access_points.reverse();

    // Purge access points with duplicate SSIDs
    let mut inserted = HashSet::new();
    access_points.retain(|ap| inserted.insert(ap_ssid(ap)));

    // Purge access points without SSID (hidden)
    access_points.retain(|ap| !ap_ssid(ap).is_empty());

    access_points
}

fn ssid_to_string(ssid: Option<glib::Bytes>) -> Option<String> {
    // An access point SSID could be random bytes and not a UTF-8 encoded string
    std::str::from_utf8(&ssid?).ok().map(str::to_owned)
}

fn ap_ssid(ap: &AccessPoint) -> String {
    ssid_to_string(ap.ssid()).unwrap()
}

async fn create_client() -> Result<Client> {
    let client = Client::new_future()
        .await
        .context("Failed to create NetworkManager client")?;

    if !client.is_nm_running() {
        return Err(anyhow!("NetworkManager daemon is not running"));
    }

    Ok(client)
}

async fn delete_exising_wifi_connect_ap_profile(client: &Client, ssid: &str) -> Result<()> {
    let connections = client.connections();

    for connection in connections {
        let c = connection.clone().upcast::<Connection>();
        if is_access_point_connection(&c) && is_same_ssid(&c, ssid) {
            println!(
                "Deleting already created by WiFi Connect access point connection profile: {:?}",
                ssid,
            );
            connection.delete_future().await?;
        }
    }

    Ok(())
}

fn is_same_ssid(connection: &Connection, ssid: &str) -> bool {
    connection_ssid_as_str(connection) == Some(ssid.to_string())
}

fn connection_ssid_as_str(connection: &Connection) -> Option<String> {
    ssid_to_string(connection.setting_wireless()?.ssid())
}

fn is_access_point_connection(connection: &Connection) -> bool {
    is_wifi_connection(connection) && is_access_point_mode(connection)
}

fn is_access_point_mode(connection: &Connection) -> bool {
    if let Some(setting) = connection.setting_wireless() {
        if let Some(mode) = setting.mode() {
            return mode == *SETTING_WIRELESS_MODE_AP;
        }
    }

    false
}

fn is_wifi_connection(connection: &Connection) -> bool {
    if let Some(setting) = connection.setting_connection() {
        if let Some(connection_type) = setting.connection_type() {
            return connection_type == *SETTING_WIRELESS_SETTING_NAME;
        }
    }

    false
}

pub fn find_device(client: &Client, interface: &Option<String>) -> Result<DeviceWifi> {
    if let Some(ref interface) = *interface {
        get_exact_device(client, interface)
    } else {
        find_any_wifi_device(client)
    }
}

fn get_exact_device(client: &Client, interface: &str) -> Result<DeviceWifi> {
    let device = client
        .device_by_iface(interface)
        .context(format!("Failed to find interface '{}'", interface))?;

    if device.device_type() != DeviceType::Wifi {
        bail!("Not a WiFi interface '{}'", interface);
    }

    if device.state() == DeviceState::Unmanaged {
        bail!("Interface is not managed by NetworkManager '{}'", interface);
    }

    Ok(device.downcast().unwrap())
}

fn find_any_wifi_device(client: &Client) -> Result<DeviceWifi> {
    for device in client.devices() {
        if device.device_type() == DeviceType::Wifi && device.state() != DeviceState::Unmanaged {
            return Ok(device.downcast().unwrap());
        }
    }

    bail!("Failed to find a managed WiFi device")
}

async fn create_portal(
    client: &Client,
    device: &DeviceWifi,
    opts: &Opts,
) -> Result<ActiveConnection> {
    let interface = device.clone().upcast::<Device>().iface().unwrap();

    let connection = create_ap_connection(
        interface.as_str(),
        &opts.ssid,
        &opts.gateway,
        &opts.password.as_deref(),
    )?;

    let active_connection = client
        .add_and_activate_connection_future(Some(&connection), device, None)
        .await
        .context("Failed to add and activate connection")?;

    let state = finalize_active_connection_state(&active_connection).await?;

    if state == ActiveConnectionState::Deactivated {
        if let Some(remote_connection) = active_connection.connection() {
            remote_connection
                .delete_future()
                .await
                .context("Failed to delete captive portal connection after failing to activate")?;
        }
        Err(anyhow!("Failed to activate captive portal connection"))
    } else {
        Ok(active_connection)
    }
}

async fn stop_portal(client: &Client, active_connection: &ActiveConnection) -> Result<()> {
    client
        .deactivate_connection_future(active_connection)
        .await?;

    finalize_active_connection_state(active_connection).await?;

    if let Some(remote_connection) = active_connection.connection() {
        remote_connection
            .delete_future()
            .await
            .context("Failed to delete captive portal connection profile")?;
    }

    Ok(())
}

async fn finalize_active_connection_state(
    active_connection: &ActiveConnection,
) -> Result<ActiveConnectionState> {
    println!("Monitoring connection state...");

    let (sender, receiver) = oneshot::channel::<ActiveConnectionState>();
    let sender = Rc::new(RefCell::new(Some(sender)));

    let handler_id = active_connection.connect_state_changed(move |_, state, _| {
        let sender = sender.clone();
        spawn_local(async move {
            let state = unsafe { ActiveConnectionState::from_glib(state.try_into().unwrap()) };
            println!("Connection: {:?}", state);

            let exit = match state {
                ActiveConnectionState::Activated => Some(ActiveConnectionState::Activated),
                ActiveConnectionState::Deactivated => Some(ActiveConnectionState::Deactivated),
                _ => None,
            };
            if let Some(result) = exit {
                let sender = sender.borrow_mut().take().unwrap();
                sender.send(result).ok();
            }
        });
    });

    let state = receiver
        .await
        .context("Failed to receive active connection state change")?;

    glib::signal_handler_disconnect(active_connection, handler_id);

    Ok(state)
}

fn create_ap_connection(
    interface: &str,
    ssid: &str,
    address: &str,
    passphrase: &Option<&str>,
) -> Result<SimpleConnection> {
    let connection = SimpleConnection::new();

    let s_connection = SettingConnection::new();
    s_connection.set_type(Some(&SETTING_WIRELESS_SETTING_NAME));
    s_connection.set_id(Some(ssid));
    s_connection.set_autoconnect(false);
    s_connection.set_interface_name(Some(interface));
    connection.add_setting(&s_connection);

    let s_wireless = SettingWireless::new();
    s_wireless.set_ssid(Some(&(ssid.as_bytes().into())));
    s_wireless.set_band(Some("bg"));
    s_wireless.set_hidden(false);
    s_wireless.set_mode(Some(&SETTING_WIRELESS_MODE_AP));
    connection.add_setting(&s_wireless);

    if let Some(password) = passphrase {
        let s_wireless_security = SettingWirelessSecurity::new();
        s_wireless_security.set_key_mgmt(Some("wpa-psk"));
        s_wireless_security.set_psk(Some(password));
        connection.add_setting(&s_wireless_security);
    }

    let s_ip4 = SettingIP4Config::new();
    let address =
        IPAddress::new(libc::AF_INET, address, 24).context("Failed to parse gateway address")?;
    s_ip4.add_address(&address);
    s_ip4.set_method(Some(&SETTING_IP4_CONFIG_METHOD_MANUAL));
    connection.add_setting(&s_ip4);

    Ok(connection)
}

pub fn spawn_local<F: Future<Output = ()> + 'static>(f: F) {
    glib::MainContext::ref_thread_default().spawn_local(f);
}
