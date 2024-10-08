use futures::future::Either;
use futures::stream::StreamExt;
use futures::SinkExt;
use futures_channel::mpsc;
use log::{log, Level};
use parking_lot::Mutex;
use quantities::mass::Mass;
use schemars::JsonSchema;
use std::marker::PhantomData;
use std::{error::Error, io, path::PathBuf, sync::Arc};
use tokio::select;
use tokio::sync::{Notify, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zbus::{interface, zvariant::Type};
use zeromq::{
    PubSocket, Socket, SocketEvent, SocketRecv, SocketSend, SubSocket, ZmqError, ZmqMessage,
};

use crate::filter::AnyFilterDecl;
use crate::filter::Filter;
use crate::filter::Filters;
use crate::ipc_ruler_client::IpcRulerProxy;
use crate::progbase;

const FILE_PREFIX: &'static str = "ipc://";
const FILE_PATH: &'static str = "/var/run/tfc/";
fn endpoint(file_name: &str) -> String {
    format!("{}{}{}", FILE_PREFIX, FILE_PATH, file_name)
}

#[derive(Clone)]
pub struct Base<T> {
    pub name: String,
    pub description: Option<String>,
    pub value: Arc<RwLock<Option<T>>>,
    pub log_key: String,
}

impl<T: TypeName> Base<T> {
    pub fn new(name: &str, description: Option<&str>) -> Self {
        Self {
            name: String::from(name),
            description: description.map(|s| s.to_string()),
            value: Arc::new(RwLock::new(None)),
            log_key: Self::type_and_name(name),
        }
    }
    fn type_and_name(name: &str) -> String {
        format!("{}/{}", T::type_name(), name)
    }
    fn full_name(&self) -> String {
        format!(
            "{}.{}.{}.{}",
            progbase::exe_name(),
            progbase::proc_name(),
            T::type_name(),
            self.name
        )
    }
    fn endpoint(&self) -> String {
        endpoint(&self.full_name())
    }
    fn path(&self) -> PathBuf {
        let runtime_dir =
            std::env::var("RUNTIME_DIRECTORY").unwrap_or_else(|_| FILE_PATH.to_string());
        PathBuf::from(format!("{}{}", runtime_dir, self.full_name()))
    }
}

pub struct Slot<T>
where
    T: Send + Sync + 'static + PartialEq + AnyFilterDecl,
    <T as AnyFilterDecl>::Type: Filter<T>,
{
    slot: Arc<RwLock<SlotImpl<T>>>,
    last_value: Arc<Mutex<Option<T>>>,
    new_value_channel: Arc<Mutex<mpsc::Receiver<T>>>,
    cb: Option<Arc<Mutex<Box<dyn Fn(&T) + Send + Sync>>>>,
    connect_notify: Arc<Notify>,
    filters: Arc<Mutex<Filters<T>>>,
    dbus_path: String,
    bus: zbus::Connection,
    recv_task: Option<JoinHandle<()>>,
    log_key: String,
}

impl<T> Slot<T>
where
    T: TypeName
        + TypeIdentifier
        + Deserialize
        + Send
        + Sync
        + 'static //
        + PartialEq
        // todo is this proper below
        + Type
        + Clone
        + detail::SupportedTypes
        + JsonSchema
        + for<'a> zbus::export::serde::de::Deserialize<'a>
        + zbus::export::serde::ser::Serialize
        + AnyFilterDecl,
    <T as AnyFilterDecl>::Type: Filter<T>,
{
    pub fn new(bus: zbus::Connection, base: Base<T>) -> Self
    where
        <T as AnyFilterDecl>::Type: Send
            + Sync
            + zbus::export::serde::ser::Serialize
            + for<'a> zbus::export::serde::de::Deserialize<'a>
            + JsonSchema,
    {
        let name = base.full_name();
        let name_cp = name.clone();
        let description = base.description.clone().unwrap_or(String::new());
        let dbus_path = format!("/is/centroid/Slot/{}", Base::<T>::type_and_name(&base.name));
        let log_key = base.log_key.clone();

        let last_value: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
        let connect_notify = Arc::new(Notify::new());
        let filters = Arc::new(Mutex::new(Filters::new(
            bus.clone(),
            format!("filters/{}", Base::<T>::type_and_name(&base.name)).as_str(),
            Arc::clone(&last_value),
        )));
        let slot = Arc::new(RwLock::new(SlotImpl::new(base)));

        let log_key_cp = log_key.clone();
        let (new_value_sender, new_value_receiver) = mpsc::channel(10);
        let client = SlotInterface::new(Arc::clone(&last_value), new_value_sender, &log_key);
        let dbus_path_cp = dbus_path.clone();
        let bus_cp = bus.clone();

        let shared_slot: Arc<RwLock<SlotImpl<T>>> = Arc::clone(&slot);
        let shared_connect_notify = Arc::clone(&connect_notify);

        // watch for connection changes for this slot
        // TODO we should make a awaitable boolean flag, we need to have started this spawn strictly before the next one where we register the slot
        tokio::spawn(async move {
            let proxy = IpcRulerProxy::builder(&bus_cp)
                .cache_properties(zbus::CacheProperties::No)
                .build()
                .await
                .unwrap();
            loop {
                let res = proxy.receive_connection_change().await;
                if res.is_ok() {
                    let args = res.unwrap().next().await.unwrap();
                    let slot_name = args.args().unwrap().slot_name().to_string();
                    if slot_name == name_cp {
                        shared_connect_notify.notify_waiters();
                        // let _ = shared_slot.write().await.disconnect().await;
                        let signal_name = args.args().unwrap().signal_name().to_string();
                        log!(target: &log_key_cp, Level::Trace,
                            "Connection change received for slot: {}, signal: {}", slot_name, signal_name);
                        let res = shared_slot.write().await.connect(&signal_name);
                        if res.is_err() {
                            log!(target: &log_key_cp, Level::Error,
                                "Failed to connect to signal: {}", res.err().unwrap());
                        }
                    }
                } else {
                }
            }
        });

        let bus_cp = bus.clone();
        let log_key_cp = log_key.clone();
        // register the dbus interface for this slot
        tokio::spawn(async move {
            let _ = bus_cp
                .object_server()
                .at(dbus_path_cp, client)
                .await
                .expect(&format!("Error registering object: {}", log_key_cp));
            let proxy = IpcRulerProxy::builder(&bus_cp)
                .cache_properties(zbus::CacheProperties::No)
                .build()
                .await
                .unwrap();
            loop {
                let res = proxy
                    .register_slot(name.as_str(), description.as_str(), T::type_identifier())
                    .await;
                if res.is_ok() {
                    break;
                } else {
                    log!(target: &log_key_cp, Level::Trace,
                    "Slot Registration failed: '{}', will try again in 1s", res.err().unwrap());
                    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                }
            }
        });

        Self {
            slot,
            last_value,
            new_value_channel: Arc::new(Mutex::new(new_value_receiver)),
            cb: None,
            connect_notify,
            filters,
            dbus_path,
            bus,
            recv_task: None,
            log_key,
        }
    }

    async fn process_value(
        value: T,
        shared_filters: Arc<Mutex<Filters<T>>>,
        shared_last_value: Arc<Mutex<Option<T>>>,
        shared_bus: zbus::Connection,
        shared_dbus_path: &str,
        log_key: &str,
    ) -> Result<Arc<Mutex<Option<T>>>, Box<dyn Error + Send + Sync>> {
        let filtered_value = shared_filters.lock().process(value).await;
        match filtered_value {
            Ok(filtered) => {
                *shared_last_value.lock() = Some(filtered);
                let value_guard = shared_last_value.lock();
                let value_ref = value_guard.as_ref().unwrap();
                let iface: zbus::InterfaceRef<SlotInterface<T>> = shared_bus
                    .object_server()
                    .interface(shared_dbus_path)
                    .await
                    .unwrap();
                let _ = SlotInterface::value(&iface.signal_context(), value_ref).await;
                Ok(Arc::clone(&shared_last_value))
            }
            Err(err) => {
                log!(target: log_key, Level::Trace,
                    "Filtered out value reason: {}", err);
                Err(err)
            }
        }
    }

    pub fn value(&self) -> Result<T, Box<dyn Error + Send + Sync>> {
        let guard = self.last_value.lock();
        match *guard {
            Some(ref value) => Ok(value.clone()),
            None => Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "No value set",
            ))),
        }
    }

    async fn async_recv_(
        shared_slot: Arc<RwLock<SlotImpl<T>>>,
        shared_new_value_channel: Arc<Mutex<mpsc::Receiver<T>>>,
        shared_connect_notify: Arc<Notify>,
        shared_filters: Arc<Mutex<Filters<T>>>,
        shared_last_value: Arc<Mutex<Option<T>>>,
        shared_bus: zbus::Connection,
        shared_dbus_path: &str,
        log_key: &str,
    ) -> Result<Arc<Mutex<Option<T>>>, Box<dyn Error + Send + Sync>> {
        loop {
            let slot_guard = shared_slot.read().await; // Create a binding for the read guard
            let mut new_value_guard = shared_new_value_channel.lock();
            tokio::select! {
                result = slot_guard.recv() => {
                    match result {
                        Ok(value) => {
                            return Slot::process_value(
                                value,
                                Arc::clone(&shared_filters),
                                Arc::clone(&shared_last_value),
                                shared_bus.clone(),
                                shared_dbus_path,
                                &log_key,
                            ).await;
                        }
                        Err(e) => {
                            log!(target: &log_key, Level::Info,
                                "Unsuccessful receive on slot, error: {}", e);
                            return Err(e);
                        }
                    }
                },
                _ = shared_connect_notify.notified() => {
                    log!(target: &log_key, Level::Info,
                            "Connecting to other signal, let's try receiving from it");
                },
                result = new_value_guard.next() => {
                    match result {
                        Some(value) => {
                            return Slot::process_value(
                                value,
                                Arc::clone(&shared_filters),
                                Arc::clone(&shared_last_value),
                                shared_bus.clone(),
                                shared_dbus_path,
                                &log_key,
                            ).await;
                        }
                        None => {
                            log!(target: &log_key, Level::Trace,
                                "New value channel ended");
                            return Err(Box::new(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                "New value channel ended",
                            )));
                        }
                    }
                }
            }
        }
    }

    pub async fn async_recv(&self) -> Result<Arc<Mutex<Option<T>>>, Box<dyn Error + Send + Sync>> {
        Slot::async_recv_(
            Arc::clone(&self.slot),
            Arc::clone(&self.new_value_channel),
            Arc::clone(&self.connect_notify),
            Arc::clone(&self.filters),
            Arc::clone(&self.last_value),
            self.bus.clone(),
            &self.dbus_path,
            &self.log_key,
        )
        .await
    }

    pub fn stream(&self) -> mpsc::Receiver<T> {
        let (mut sender, receiver) = mpsc::channel(10);
        let shared_slot = Arc::clone(&self.slot);
        let shared_new_value_channel = Arc::clone(&self.new_value_channel);
        let shared_connect_notify = Arc::clone(&self.connect_notify);
        let shared_filters = Arc::clone(&self.filters);
        let shared_last_value = Arc::clone(&self.last_value);
        let shared_bus = self.bus.clone();
        let shared_dbus_path = self.dbus_path.clone();
        let log_key = self.log_key.clone();
        tokio::spawn(async move {
            loop {
                let res = Slot::async_recv_(
                    Arc::clone(&shared_slot),
                    Arc::clone(&shared_new_value_channel),
                    Arc::clone(&shared_connect_notify),
                    Arc::clone(&shared_filters),
                    Arc::clone(&shared_last_value),
                    shared_bus.clone(),
                    &shared_dbus_path,
                    &log_key,
                )
                .await;
                if res.is_ok() {
                    let shared_last_value = res.unwrap();
                    let guard = shared_last_value.lock();
                    let value = guard.as_ref().unwrap();
                    sender.send(value.clone()).await.unwrap();
                } else {
                    log!(target: &log_key, Level::Warn,
                        "Error receiving value: {}", res.err().unwrap());
                }
            }
        });
        receiver
    }

    pub fn recv(&mut self, callback: Box<dyn Fn(&T) + Send + Sync>)
    where
        <T as AnyFilterDecl>::Type: Send + Sync + Filter<T>,
    {
        let shared_cb: Arc<Mutex<Box<dyn Fn(&T) + Send + Sync>>> = Arc::new(Mutex::new(callback));
        self.cb = Some(Arc::clone(&shared_cb));
        let log_key = self.log_key.clone();
        let shared_slot: Arc<RwLock<SlotImpl<T>>> = Arc::clone(&self.slot);
        let shared_last_value = Arc::clone(&self.last_value);
        let shared_connect_notify = Arc::clone(&self.connect_notify);
        let shared_filters = Arc::clone(&self.filters);
        let shared_new_value_channel = Arc::clone(&self.new_value_channel);
        let shared_bus = self.bus.clone();
        let shared_dbus_path = self.dbus_path.clone();

        self.recv_task.replace(tokio::spawn(async move {
            loop {
                let res = Slot::async_recv_(
                    Arc::clone(&shared_slot),
                    Arc::clone(&shared_new_value_channel),
                    Arc::clone(&shared_connect_notify),
                    Arc::clone(&shared_filters),
                    Arc::clone(&shared_last_value),
                    shared_bus.clone(),
                    &shared_dbus_path,
                    &log_key,
                )
                .await;
                if res.is_ok() {
                    let shared_last_value = res.unwrap();
                    let guard = shared_last_value.lock();
                    let value = guard.as_ref().unwrap();
                    shared_cb.lock()(&value);
                } else {
                    log!(target: &log_key, Level::Warn,
                        "Error receiving value: {}", res.err().unwrap());
                }
            }
        }));
    }
    pub async fn async_connect(
        &mut self,
        signal_name: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.connect_notify.notify_waiters();
        self.slot.write().await.async_connect(signal_name).await
    }
    pub fn connect(&mut self, signal_name: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        let shared_slot: Arc<RwLock<SlotImpl<T>>> = Arc::clone(&self.slot);
        let shared_connect_notify = Arc::clone(&self.connect_notify);
        let name = signal_name.to_string();
        tokio::spawn(async move {
            shared_connect_notify.notify_waiters();
            shared_slot.write().await.connect(&name)
        });
        Ok(())
    }
}
impl<T: Send + Sync + 'static + PartialEq + AnyFilterDecl> Drop for Slot<T>
where
    <T as AnyFilterDecl>::Type: Filter<T>,
{
    fn drop(&mut self) {
        if let Some(task) = self.recv_task.take() {
            task.abort();
        }
    }
}

mod detail {
    pub trait SupportedTypes {
        fn to_value(&self) -> zbus::zvariant::Value<'static>;
    }
    // make macro to impl for all types
    macro_rules! impl_supported_types {
        ($($t:ty),*) => {
            $(
                impl SupportedTypes for $t {
                    fn to_value(&self) -> zbus::zvariant::Value<'static> {
                        zbus::zvariant::Value::from(*self)
                    }
                }
            )*
        };
    }
    impl_supported_types!(bool, i64, u64, f64);
    impl SupportedTypes for String {
        fn to_value(&self) -> zbus::zvariant::Value<'static> {
            zbus::zvariant::Value::from(self.clone())
        }
    }
}

struct SlotInterface<T> {
    last_value: Arc<Mutex<Option<T>>>,
    new_value_channel: mpsc::Sender<T>,
    log_key: String,
}

impl<T: detail::SupportedTypes + zbus::zvariant::Type + Sync + Send + 'static> SlotInterface<T> {
    pub fn new(
        last_value: Arc<Mutex<Option<T>>>,
        new_value_channel: mpsc::Sender<T>,
        key: &str,
    ) -> Self {
        Self {
            last_value,
            new_value_channel,
            log_key: key.to_string(),
        }
    }
}

#[interface(name = "is.centroid.Slot")]
impl<T> SlotInterface<T>
where
    T: Clone
        + detail::SupportedTypes
        + zbus::zvariant::Type
        + Sync
        + Send
        + 'static
        + JsonSchema
        + for<'a> zbus::export::serde::de::Deserialize<'a>
        + zbus::export::serde::ser::Serialize,
{
    #[zbus(property, name = "Value")]
    async fn value_prop(&self) -> Result<zbus::zvariant::Value<'static>, zbus::fdo::Error> {
        let guard = self.last_value.lock();

        match *guard {
            Some(ref value) => Ok(value.to_value()),
            None => Err(zbus::fdo::Error::Failed("No value set".into())),
        }
    }
    #[zbus(property(emits_changed_signal = "const"))]
    async fn schema(&self) -> Result<String, zbus::fdo::Error> {
        serde_json::to_string_pretty(&schemars::schema_for!(T)).map_err(|e| {
            let err_msg = format!("Error serializing to JSON schema: {}", e);
            log!(target: &self.log_key, Level::Error, "{}", err_msg);
            zbus::fdo::Error::Failed(err_msg)
        })
    }
    async fn last_value(&self) -> Result<T, zbus::fdo::Error> {
        let guard = self.last_value.lock();
        match *guard {
            Some(ref value) => Ok(value.clone()),
            None => Err(zbus::fdo::Error::Failed("No value set".into())),
        }
    }
    async fn tinker(&mut self, value: T) -> Result<(), zbus::fdo::Error> {
        self.new_value_channel.send(value).await.map_err(|e| {
            let err_msg = format!("Error sending new value: {}", e);
            log!(target: &self.log_key, Level::Error, "{}", err_msg);
            zbus::fdo::Error::Failed(err_msg)
        })?;
        Ok(())
    }
    #[zbus(signal)]
    async fn value(signal_ctxt: &zbus::SignalContext<'_>, val: &T) -> zbus::Result<()>;
}

pub struct SlotImpl<T> {
    base: Base<T>,
    sock: Arc<Mutex<Option<SubSocket>>>,
    is_connected: Arc<Mutex<bool>>,
    connect_notify: Arc<Notify>,
    connect_cancel_token: CancellationToken,
}

impl<T> SlotImpl<T>
where
    T: TypeName + TypeIdentifier + Deserialize,
{
    pub fn new(base: Base<T>, // , callback: Box<dyn Fn(T)>
    ) -> Self {
        // todo: https://github.com/zeromq/zmq.rs/issues/196
        // let mut options = SocketOptions::default();
        // options.ZMQ_RECONNECT_IVL
        Self {
            base,
            sock: Arc::new(Mutex::new(None)),
            is_connected: Arc::new(Mutex::new(false)),
            connect_notify: Arc::new(Notify::new()),
            connect_cancel_token: CancellationToken::new(),
        }
    }
    async fn async_connect_(
        log_key: &str,
        sock: Arc<Mutex<Option<SubSocket>>>,
        signal_name: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        if signal_name.is_empty() {
            return Ok(());
        }
        let socket_path = endpoint(signal_name);
        log!(target: &log_key, Level::Trace, "Trying to connect to: {}", socket_path);
        let mut sock = sock.lock();
        if sock.is_none() {
            *sock = Some(SubSocket::new());
        }
        sock.as_mut().unwrap().connect(socket_path.as_str()).await?;
        sock.as_mut().unwrap().subscribe("").await?;
        Ok(())
    }
    pub async fn async_connect(
        &mut self,
        signal_name: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // todo reconnect
        let res = Self::async_connect_(
            self.base.log_key.as_str(),
            Arc::clone(&self.sock),
            signal_name,
        )
        .await;
        if res.is_ok() {
            *self.is_connected.lock() = true;
            self.connect_notify.notify_waiters();
        }
        res
    }
    pub fn connect(&mut self, signal_name: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        let shared_sock = Arc::clone(&self.sock);
        let signal_name_str = signal_name.to_string();
        let log_key_cp = self.base.log_key.clone();
        let shared_is_connected = Arc::clone(&self.is_connected);
        let shared_connect_notify = Arc::clone(&self.connect_notify);

        // cancel any previous connect
        self.connect_cancel_token.cancel();
        self.connect_cancel_token = CancellationToken::new();
        let token = self.connect_cancel_token.child_token();
        tokio::spawn(async move {
            {
                let mut socket = shared_sock.lock();
                if let Some(sub_socket) = socket.take() {
                    sub_socket.close().await;
                }
                *socket = None;
                if signal_name_str.is_empty() {
                    *shared_is_connected.lock() = false;
                    shared_connect_notify.notify_waiters();
                    return;
                }
            }
            loop {
                let connect_task = async {
                    Self::async_connect_(
                        log_key_cp.as_str(),
                        Arc::clone(&shared_sock),
                        signal_name_str.as_str(),
                    )
                    .await
                };
                let timeout_task =
                    async { tokio::time::sleep(tokio::time::Duration::from_millis(100)).await };

                match select! {
                    result = connect_task => Either::Left(result),
                    _ = timeout_task => Either::Right(()),
                    cancel_result = token.cancelled() => {
                        log!(target: &log_key_cp, Level::Trace,
                            "Connect to: {:?} cancelled, error: {:?}", signal_name_str, cancel_result);
                        break;
                    }
                } {
                    Either::Left(Ok(_)) => {
                        log!(target: &log_key_cp, Level::Trace,
                            "Connect to: {:?} succesful", signal_name_str);
                        // don't know why we need to notify here, if we are notifying in async_connect
                        *shared_is_connected.lock() = true;
                        shared_connect_notify.notify_waiters();
                        break;
                    }
                    Either::Left(Err(e)) => {
                        log!(target: &log_key_cp, Level::Trace, "Connect failed: {:?} will try again", e);
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    }
                    Either::Right(_) => {
                        log!(target: &log_key_cp, Level::Trace, "Connect timed out");
                    }
                }
            }
        });
        Ok(())
    }
    pub async fn recv(&self) -> Result<T, Box<dyn Error + Send + Sync>> {
        if !*self.is_connected.lock() {
            log!(target: self.base.log_key.as_str(), Level::Trace, "Still not connected will wait until then");
            self.connect_notify.notified().await;
        }
        // todo tokio select, if we were to disconnect in the middle of recv
        let mut sock = self.sock.lock();
        if sock.is_none() {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Socket is not connected",
            )));
        }
        let buffer: ZmqMessage = sock.as_mut().unwrap().recv().await?;
        // todo remove copying
        let flattened_buffer: Vec<u8> = buffer.iter().flat_map(|b| b.to_vec()).collect();
        let mut cursor = io::Cursor::new(flattened_buffer);
        let deserialized_packet =
            DeserializePacket::<T>::deserialize(&mut cursor).expect("Deserialization failed");
        Ok(deserialized_packet.value)
    }
}

pub struct Signal<T: TypeName> {
    signal: Arc<Mutex<SignalImpl<T>>>,
    full_name: String,
    base: Arc<Mutex<Base<T>>>,
    dbus_path: Arc<Mutex<String>>,
    bus: zbus::Connection,
    #[allow(dead_code)]
    log_key: String,
    init_task: Option<tokio::task::JoinHandle<()>>,
}

impl<T> Signal<T>
where
    T: TypeName
        + TypeIdentifier
        + SerializeSize
        + Serialize
        + Deserialize
        + Send
        + Sync
        + 'static //
        // todo is this proper below
        + Type
        + Clone
        + detail::SupportedTypes
        + JsonSchema
        + std::fmt::Debug
        + for<'a> zbus::export::serde::de::Deserialize<'a>
        + zbus::export::serde::ser::Serialize,
{
    pub fn new(bus: zbus::Connection, base: Base<T>) -> Self {
        let log_key_cp = base.log_key.clone();
        let log_key = base.log_key.clone();
        let client = SignalInterface::new(Arc::clone(&base.value), &log_key);
        let dbus_path = format!(
            "/is/centroid/Signal/{}",
            Base::<T>::type_and_name(&base.name)
        );
        let dbus_path_cp = dbus_path.clone();
        let bus_cp = bus.clone();
        let base_cp = base.clone();
        let signal = Arc::new(Mutex::new(SignalImpl::new(base_cp)));
        let signal_cp = Arc::clone(&signal);

        let name = base.full_name();
        let description = base.description.clone().unwrap_or(String::new());

        let init_task = tokio::spawn(async move {
            let _ = signal_cp.lock().init().await;
        });
        tokio::spawn(async move {
            let _ = bus_cp
                .object_server()
                .at(dbus_path_cp, client)
                .await
                .expect(&format!("Error registering object: {}", log_key_cp));
            let proxy = IpcRulerProxy::builder(&bus_cp)
                .cache_properties(zbus::CacheProperties::No)
                .build()
                .await
                .unwrap();
            loop {
                let res = proxy
                    .register_signal(name.as_str(), description.as_str(), T::type_identifier())
                    .await;
                if res.is_ok() {
                    break;
                } else {
                    log!(target: &log_key_cp, Level::Trace,
                "Signal Registration failed: '{}', will try again in 1s", res.err().unwrap());
                    tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
                }
            }
        });
        Self {
            signal,
            full_name: base.full_name(),
            base: Arc::new(Mutex::new(base)),
            dbus_path: Arc::new(Mutex::new(dbus_path)),
            bus,
            log_key,
            init_task: Some(init_task),
        }
    }

    // Use to wait for the constructor to finish
    pub async fn init_task(&mut self) -> Result<(), tokio::task::JoinError> {
        self.init_task
            .take()
            .expect("Init task has not been started")
            .await
    }

    async fn async_send_impl(
        signal: Arc<Mutex<SignalImpl<T>>>,
        base: Arc<Mutex<Base<T>>>,
        bus: zbus::Connection,
        dbus_path: Arc<Mutex<String>>,
        value: T,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        signal.lock().send(value).await?;
        let base = base.lock();
        let value_guard = base.value.read().await;
        let value_ref = value_guard.as_ref().unwrap();

        let iface: Result<zbus::InterfaceRef<SignalInterface<T>>, zbus::Error> = bus
            .object_server()
            .interface(dbus_path.lock().as_str())
            .await;
        if let Ok(iface) = iface {
            SignalInterface::value(&iface.signal_context(), value_ref).await?;
        } else {
            // This happens when the signal has not been registered yet,
            // which can happen if the signal is sent before the interface is registered
            // We simply ignore this case as the interface will be registered soon
            log!(target: &base.log_key, Level::Info,
                "Error sending signal value: {}", iface.err().unwrap());
        }
        Ok(())
    }

    pub fn send(&mut self, value: T) -> Result<(), Box<dyn Error + Send + Sync>> {
        let shared_signal = Arc::clone(&self.signal);
        let shared_base = Arc::clone(&self.base);
        let dbus_path = Arc::clone(&self.dbus_path);
        let bus = self.bus.clone();
        tokio::spawn(async move {
            Self::async_send_impl(shared_signal, shared_base, bus, dbus_path, value).await
        });
        Ok(())
    }

    pub async fn async_send(&mut self, value: T) -> Result<(), Box<dyn Error + Send + Sync>> {
        Self::async_send_impl(
            Arc::clone(&self.signal),
            Arc::clone(&self.base),
            self.bus.clone(),
            self.dbus_path.clone(),
            value,
        )
        .await
    }
    pub fn full_name(&self) -> &str {
        &self.full_name
    }
}

struct SignalInterface<T> {
    last_value: Arc<RwLock<Option<T>>>,
    log_key: String,
}

impl<T: detail::SupportedTypes + zbus::zvariant::Type + Sync + Send + 'static> SignalInterface<T> {
    pub fn new(last_value: Arc<RwLock<Option<T>>>, key: &str) -> Self {
        Self {
            last_value,
            log_key: key.to_string(),
        }
    }
}

#[interface(name = "is.centroid.Signal")]
impl<T> SignalInterface<T>
where
    T: Clone
        + detail::SupportedTypes
        + zbus::zvariant::Type
        + Sync
        + Send
        + 'static
        + JsonSchema
        + for<'a> zbus::export::serde::de::Deserialize<'a>
        + zbus::export::serde::ser::Serialize,
{
    #[zbus(property, name = "Value")]
    async fn value_prop(&self) -> Result<zbus::zvariant::Value<'static>, zbus::fdo::Error> {
        let guard = self.last_value.read().await;

        match *guard {
            Some(ref value) => Ok(value.to_value()),
            None => Err(zbus::fdo::Error::Failed("No value set".into())),
        }
    }
    #[zbus(property(emits_changed_signal = "const"))]
    async fn schema(&self) -> Result<String, zbus::fdo::Error> {
        serde_json::to_string_pretty(&schemars::schema_for!(T)).map_err(|e| {
            let err_msg = format!("Error serializing to JSON schema: {}", e);
            log!(target: &self.log_key, Level::Error, "{}", err_msg);
            zbus::fdo::Error::Failed(err_msg)
        })
    }
    async fn last_value(&self) -> Result<T, zbus::fdo::Error> {
        let guard = self.last_value.read().await;
        match *guard {
            Some(ref value) => Ok(value.clone()),
            None => Err(zbus::fdo::Error::Failed("No value set".into())),
        }
    }
    #[zbus(signal)]
    async fn value(signal_ctxt: &zbus::SignalContext<'_>, val: &T) -> zbus::Result<()>;
}

pub struct SignalImpl<T: TypeName> {
    base: Base<T>,
    sock: Arc<Mutex<PubSocket>>,
    monitor: Option<mpsc::Receiver<SocketEvent>>,
}

impl<T> SignalImpl<T>
where
    T: TypeName
        + TypeIdentifier
        + SerializeSize
        + Serialize
        + Deserialize
        + std::fmt::Debug
        + std::marker::Sync
        + std::marker::Send
        + 'static,
{
    pub fn new(base: Base<T>) -> Self {
        Self {
            base,
            sock: Arc::new(Mutex::new(PubSocket::new())),
            monitor: None,
        }
    }
    pub async fn init(&mut self) -> Result<(), Box<dyn Error>> {
        // Todo cpp azmq does this by itself
        // We should check whether any PID is binded to the file before removing it
        // But the bind below fails if the file exists, even though no one is binded to it
        let path = self.base.path();
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        self.sock.lock().bind(self.base.endpoint().as_str()).await?;
        self.monitor = Some(self.sock.lock().monitor());

        let shared_value = Arc::clone(&self.base.value);
        let shared_sock = Arc::clone(&self.sock);
        let log_key = self.base.log_key.clone();
        if let Some(mut receiver) = self.monitor.take() {
            tokio::spawn(async move {
                while let Some(event) = receiver.next().await {
                    match event {
                        SocketEvent::Accepted { .. } => {
                            log!(target: &log_key, Level::Trace,
                                "Accepted event, last value: {:?}", shared_value.read().await
                            );
                            let mut locked_sock = shared_sock.lock();
                            let mut buffer = Vec::new();
                            {
                                let value = shared_value.read().await;
                                if value.is_none() {
                                    return Ok(()) as Result<(), ZmqError>;
                                }
                                let packet = SerializePacket::new(value.as_ref().unwrap());
                                packet.serialize(&mut buffer).expect("Serialization failed");
                            }
                            // TODO use ZMQ_EVENT_HANDSHAKE_SUCCEEDED and throw this sleep out
                            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                            locked_sock.send(ZmqMessage::from(buffer)).await?;
                        }
                        other_event => {
                            log!(target: &log_key, Level::Info,
                                "Other event: {:?}, last value: {:?}",
                                other_event,
                                shared_value.read().await
                            );
                        }
                    }
                }
                println!("Receiver has been dropped. Task terminating.");
                Ok(())
            });
        }
        Ok(())
    }
    pub async fn send(&mut self, value: T) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut buffer = Vec::new();
        {
            let packet = SerializePacket::new(&value);
            packet.serialize(&mut buffer).expect("Serialization failed");
        }
        *self.base.value.write().await = Some(value);
        self.sock.lock().send(ZmqMessage::from(buffer)).await?;
        Ok(())
    }

    pub fn full_name(&self) -> String {
        self.base.full_name()
    }
}

impl<T: TypeName> Drop for SignalImpl<T> {
    fn drop(&mut self) {
        let sock = self.sock.lock();
        drop(sock);
        let file = self.base.endpoint();
        std::fs::remove_file(file).unwrap_or(());
    }
}

// ------------------ trait TypeName ------------------

pub trait TypeName {
    fn type_name() -> &'static str;
}

macro_rules! impl_type_name {
    ($($t:ty => $name:expr),*) => {
        $(
            impl TypeName for $t {
                fn type_name() -> &'static str {
                    $name
                }
            }
        )*
    };
}

impl_type_name! {
    bool => "bool",
    i64 => "i64",
    u64 => "u64",
    f64 => "double",
    String => "string",
    // todo json
    Mass => "mass"
}

// ------------------ trait TypeIdentifier ------------------

pub trait TypeIdentifier {
    fn type_identifier() -> u8;
}

macro_rules! impl_type_identifier {
    ($($t:ty => $name:expr),*) => {
        $(
            impl TypeIdentifier for $t {
                fn type_identifier() -> u8 {
                    $name
                }
            }
        )*
    };
}

impl_type_identifier! {
    bool => 1,
    i64 => 2,
    u64 => 3,
    f64 => 4,
    String => 5,
    // todo json
    Result<Mass, i32> => 7, // TODO let's just make a ipc_mass.rs or something and impl this there with its enum
    Mass => 7 // TODO can we deduce this somehow
}

// ------------------ enum Protocol Version ------------------
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Version {
    Unknown,
    V0,
}

trait IsFundamental {}
macro_rules! impl_is_fundamental {
    ($($t:ty),*) => {
        $(
            impl IsFundamental for $t {}
        )*
    };
}
impl_is_fundamental!(bool, i64, i32, i16, i8, u64, u32, u16, u8, f64, f32);

trait ToLeBytesVec {
    fn to_le_bytes_vec(&self) -> Vec<u8>;
}
macro_rules! impl_to_le_bytes_vec_for_fundamental {
    ($($t:ty),*) => {
        $(
            impl ToLeBytesVec for $t {
                fn to_le_bytes_vec(&self) -> Vec<u8> {
                    self.to_le_bytes().to_vec()
                }
            }
        )*
    };
}
impl_to_le_bytes_vec_for_fundamental!(i8, i16, i32, i64, u8, u16, u32, u64, f32, f64);
impl ToLeBytesVec for bool {
    fn to_le_bytes_vec(&self) -> Vec<u8> {
        vec![*self as u8]
    }
}

pub trait SerializeSize {
    fn serialize_size(&self) -> usize;
}
impl<T: IsFundamental> SerializeSize for T {
    fn serialize_size(&self) -> usize {
        std::mem::size_of::<T>()
    }
}
impl SerializeSize for String {
    fn serialize_size(&self) -> usize {
        self.len()
    }
}
impl<T, E> SerializeSize for Result<T, E>
where
    T: quantities::Quantity,
    E: IsFundamental,
{
    fn serialize_size(&self) -> usize {
        1 // Used to indicate whether the Result is Type or Error
            + if self.is_ok() { std::mem::size_of::<quantities::AmountT>() } else { std::mem::size_of::<E>() }
    }
}
pub trait Serialize {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()>;
}
impl<T> Serialize for T
where
    T: ToLeBytesVec,
{
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(&self.to_le_bytes_vec())?;
        Ok(())
    }
}
impl Serialize for String {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(&self.as_bytes())?;
        Ok(())
    }
}
impl<T, E> Serialize for Result<T, E>
where
    T: quantities::Quantity,
    E: IsFundamental + std::fmt::Debug + ToLeBytesVec,
{
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        if self.is_ok() {
            writer.write_all(&[1 as u8])?; // use 1 to indicate value being okay
            writer.write_all(&self.as_ref().unwrap().amount().to_le_bytes())?;
        } else {
            writer.write_all(&[0 as u8])?; // use 1 to indicate value being okay
            writer.write_all(&self.as_ref().err().unwrap().to_le_bytes_vec())?;
        }
        Ok(())
    }
}

trait FromLeBytes: Sized {
    fn from_le_bytes(bytes: &[u8]) -> Self;
}
macro_rules! impl_from_le_bytes_for_fundamental {
    ($($t:ty),*) => {
        $(
            impl FromLeBytes for $t {
                fn from_le_bytes(bytes: &[u8]) -> Self {
                    assert!(bytes.len() == std::mem::size_of::<$t>(), "Invalid byte slice length for {}", stringify!($t));
                    let mut arr = [0u8; std::mem::size_of::<$t>()];
                    arr.copy_from_slice(bytes);
                    <$t>::from_le_bytes(arr)
                }
            }
        )*
    };
}
impl_from_le_bytes_for_fundamental!(i8, i16, i32, i64, u8, u16, u32, u64, f32, f64);
impl FromLeBytes for bool {
    fn from_le_bytes(bytes: &[u8]) -> Self {
        assert!(bytes.len() == 1, "Invalid byte slice length for bool");
        bytes[0] != 0
    }
}

pub trait Deserialize: Sized {
    fn deserialize<R: io::Read>(reader: &mut R) -> io::Result<Self>;
}

impl<T> Deserialize for T
where
    T: FromLeBytes,
{
    fn deserialize<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut buffer = vec![0u8; std::mem::size_of::<T>()];
        reader.read_exact(&mut buffer)?;
        Ok(T::from_le_bytes(&buffer))
    }
}

impl Deserialize for String {
    fn deserialize<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut buffer = Vec::new();
        reader.read_to_end(&mut buffer)?;
        String::from_utf8(buffer).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

impl<T, E> Deserialize for Result<T, E>
where
    T: quantities::Quantity<UnitType = quantities::mass::MassUnit>
        // TODO this now only supports mass
        + TypeIdentifier,
    quantities::AmountT: FromLeBytes, // I would much rather like T::AmountT
    E: IsFundamental + std::fmt::Debug + FromLeBytes,
{
    fn deserialize<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut status_byte = [0u8; 1];
        reader.read_exact(&mut status_byte)?;

        if status_byte[0] == 1 {
            // Deserialize as Ok(T)
            let mut amount_buffer = [0u8; std::mem::size_of::<quantities::AmountT>()];
            reader.read_exact(&mut amount_buffer)?;
            let amount = quantities::AmountT::from_le_bytes(amount_buffer);
            // So the unit type of Quantity is a member variable so we cannot determine the type
            // at compile time, and this is god damn bloated
            let unit;
            if T::type_identifier() == Mass::type_identifier() {
                unit = quantities::mass::MILLIGRAM;
            } else {
                panic!(
                    "Unable to determine UnitType for type id: {}",
                    T::type_identifier()
                )
            }
            Ok(Ok(T::new(amount, unit)))
        } else if status_byte[0] == 0 {
            // Deserialize as Err(E)
            let mut error_buffer = vec![0u8; std::mem::size_of::<E>()];
            reader.read_exact(&mut error_buffer)?;
            let error = E::from_le_bytes(&error_buffer);
            Ok(Err(error))
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid status byte in serialized Result",
            ))
        }
    }
}

#[derive(Debug, Clone)]
struct Header<T> {
    version: Version,
    type_id: u8,
    value_size: usize,
    _phantom: PhantomData<T>,
}

impl<T: TypeIdentifier> Header<T> {
    fn new(value_size: usize) -> Self {
        Self {
            version: Version::V0,
            type_id: T::type_identifier(),
            value_size,
            _phantom: PhantomData,
        }
    }

    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        writer.write_all(&[self.version as u8])?;
        writer.write_all(&[self.type_id as u8])?;
        writer.write_all(&self.value_size.to_le_bytes())?;
        Ok(())
    }

    fn deserialize<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let mut version_buf = [0u8; 1];
        reader.read_exact(&mut version_buf)?;
        let _ = match version_buf[0] {
            0 => Version::Unknown,
            1 => Version::V0,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid version",
                ))
            }
        };

        let mut type_id_buf = [0u8; 1];
        reader.read_exact(&mut type_id_buf)?;
        let type_id = type_id_buf[0];

        let mut size_buf = [0u8; std::mem::size_of::<usize>()];
        reader.read_exact(&mut size_buf)?;
        let value_size = usize::from_le_bytes(size_buf);

        if type_id != T::type_identifier() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid type"));
        }
        Ok(Self::new(value_size))
    }
}

#[derive(Debug)]
struct SerializePacket<'a, V> {
    header: Header<V>,
    value: &'a V,
}

impl<'a, V> SerializePacket<'a, V>
where
    V: TypeIdentifier + SerializeSize + Serialize + Deserialize,
{
    fn new(value: &'a V) -> Self {
        Self {
            header: Header::new(V::serialize_size(&value)),
            value,
        }
    }
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        self.header.serialize(writer)?;
        self.value.serialize(writer)?;
        Ok(())
    }
}

#[derive(Debug)]
struct DeserializePacket<V> {
    #[allow(dead_code)]
    header: Header<V>,
    value: V,
}

impl<V> DeserializePacket<V>
where
    V: TypeIdentifier + Deserialize,
{
    fn deserialize<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let header = Header::deserialize(reader)?;
        let value = V::deserialize(reader)?;
        Ok(Self { header, value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logger;
    use quantities::mass::Mass;
    use rand::Rng;
    use std::io::Cursor;

    fn setup_dirs() {
        // let current_dir = std::env::current_dir().expect("Failed to get current directory");
        // let current_dir_str = current_dir
        //     .to_str()
        //     .expect("Failed to convert path to string");
        // std::env::set_var("RUNTIME_DIRECTORY", current_dir_str);
        // std::env::set_var("CONFIGURATION_DIRECTORY", current_dir_str);
    }

    fn packet_serialize_deserialize<T>(value: T)
    where
        T: TypeIdentifier + SerializeSize + Serialize + Deserialize + PartialEq + std::fmt::Debug,
    {
        let packet = SerializePacket::new(&value);

        // Serialize the packet
        let mut buffer = Vec::new();
        packet.serialize(&mut buffer).expect("Serialization failed");

        // Deserialize the packet
        let mut cursor = Cursor::new(buffer);
        let deserialized_packet =
            DeserializePacket::<T>::deserialize(&mut cursor).expect("Deserialization failed");

        // Check that the deserialized value matches the original
        assert_eq!(*packet.value, deserialized_packet.value);
        assert_eq!(
            packet.header.value_size,
            deserialized_packet.header.value_size
        );
        assert_eq!(packet.header.type_id, deserialized_packet.header.type_id);
    }

    #[test]
    fn test_packet_serialize_deserialize_fundamentals() {
        packet_serialize_deserialize(std::i64::MAX);
        packet_serialize_deserialize(std::i64::MIN);
        packet_serialize_deserialize(1337 as i64);

        packet_serialize_deserialize(std::u64::MAX);
        packet_serialize_deserialize(std::u64::MIN);
        packet_serialize_deserialize(1337 as u64);

        packet_serialize_deserialize(std::f64::MAX);
        packet_serialize_deserialize(std::f64::MIN);
        packet_serialize_deserialize(13.37 as f64);

        packet_serialize_deserialize(true);
        packet_serialize_deserialize(false);
    }

    #[test]
    fn test_packet_serialize_deserialize_string() {
        packet_serialize_deserialize("false".to_string());

        let size = 1024 * 1024; // 1KB * 1KB
        let mut rng = rand::thread_rng();
        let mut random_string = String::with_capacity(size);

        for _ in 0..size {
            let random_char = rng.gen_range('a'..='z');
            random_string.push(random_char);
        }

        packet_serialize_deserialize(random_string);
    }

    #[test]
    fn test_packet_serialize_deserialize_mass() {
        packet_serialize_deserialize(
            Ok(quantities::Amnt!(10.0) * quantities::mass::MILLIGRAM) as Result<Mass, i32>
        );
        packet_serialize_deserialize(
            Ok(quantities::Amnt!(1333333337) * quantities::mass::MILLIGRAM) as Result<Mass, i32>,
        );
        packet_serialize_deserialize(
            Ok(quantities::Amnt!(-1333333337) * quantities::mass::MILLIGRAM) as Result<Mass, i32>,
        );
        packet_serialize_deserialize(
            Ok(quantities::Amnt!(13333.33337) * quantities::mass::MILLIGRAM) as Result<Mass, i32>,
        );
        packet_serialize_deserialize(Ok(
            quantities::Amnt!(-13333.33337) * quantities::mass::MILLIGRAM
        ) as Result<Mass, i32>);

        packet_serialize_deserialize(Err(-1) as Result<Mass, i32>); // todo make
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_send_recv() -> Result<(), Box<dyn std::error::Error>> {
        setup_dirs();
        let _ = progbase::try_init();
        let _ = logger::init_combined_logger();

        let mut slot = SlotImpl::<bool>::new(Base::new("slot1", None));
        let mut signal = SignalImpl::<bool>::new(Base::new("signal1", None));
        signal.init().await?;

        slot.async_connect(signal.full_name().as_str())
            .await
            .expect("This should connect");

        let (recv, send) = tokio::join!(
            tokio::time::timeout(tokio::time::Duration::from_secs(1), slot.recv()),
            signal.send(true)
        );

        send.expect("Sender task panicked");

        let value = match recv {
            Ok(inner_result) => match inner_result {
                Ok(value) => value,
                Err(e) => {
                    panic!("Error receiving value: {:?}", e);
                }
            },
            Err(_) => {
                panic!("Timeout occurred");
            }
        };

        assert_eq!(value, true);

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_1000_sync_send_recv() -> Result<(), Box<dyn std::error::Error>> {
        setup_dirs();
        let _ = progbase::try_init();
        let _ = logger::init_combined_logger();

        let mut slot = SlotImpl::<bool>::new(Base::new("slot2", None));
        let mut signal = SignalImpl::<bool>::new(Base::new("signal2", None));
        signal.init().await?;

        slot.async_connect(signal.full_name().as_str())
            .await
            .expect("This should connect");

        let mut send_value = true;
        for _ in 0..1000 {
            let (recv, send) = tokio::join!(
                tokio::time::timeout(tokio::time::Duration::from_secs(1), slot.recv()),
                signal.send(send_value)
            );

            send.expect("Sender task panicked");

            let value = match recv {
                Ok(inner_result) => match inner_result {
                    Ok(value) => value,
                    Err(e) => {
                        panic!("Error receiving value: {:?}", e);
                    }
                },
                Err(_) => {
                    panic!("Timeout occurred");
                }
            };

            assert_eq!(value, send_value);
            send_value = !send_value;
        }

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_recv_after_send() -> Result<(), Box<dyn std::error::Error>> {
        setup_dirs();
        let _ = progbase::try_init();
        let _ = logger::init_combined_logger();

        let mut slot = SlotImpl::<bool>::new(Base::new("slot3", None));
        let mut signal = SignalImpl::<bool>::new(Base::new("signal3", None));
        signal.init().await?;

        slot.async_connect(signal.full_name().as_str())
            .await
            .expect("This should connect");

        signal.send(true).await.expect("This should not fail");

        let recv = tokio::time::timeout(tokio::time::Duration::from_secs(1), slot.recv()).await;

        let value = match recv {
            Ok(inner_result) => match inner_result {
                Ok(value) => value,
                Err(e) => {
                    panic!("Error receiving value: {:?}", e);
                }
            },
            Err(_) => {
                panic!("Timeout occurred");
            }
        };

        assert_eq!(value, true);

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_async_send_recv() -> Result<(), Box<dyn std::error::Error>> {
        setup_dirs();
        let _ = progbase::try_init();
        let _ = logger::init_combined_logger();

        let mut slot = SlotImpl::<bool>::new(Base::new("slot4", None));
        let mut signal = SignalImpl::<bool>::new(Base::new("signal4", None));
        signal.init().await?;

        slot.async_connect(signal.full_name().as_str())
            .await
            .expect("This should connect");

        let send_values = vec![
            true, true, false, false, true, true, false, false, true, true, false, false, true,
            true, false, false,
        ];
        let recv_values = vec![
            true, true, false, false, true, true, false, false, true, true, false, false, true,
            true, false, false,
        ];
        let send_task = tokio::spawn(async move {
            // This sleep makes sure that the receiver has already started and is listening before sending
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
            for val in send_values {
                signal.send(val).await.expect("This should not fail");
            }
        });

        let recv_task = tokio::spawn(async move {
            for val in recv_values {
                let recv_val = slot.recv().await.expect("This should not fail");
                assert_eq!(recv_val, val);
            }
        });

        let (recv_res, send_res) = tokio::join!(recv_task, send_task);

        send_res.expect("Sender task panicked");
        recv_res.expect("Receiver task panicked");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_public_async_send_recv() -> Result<(), Box<dyn std::error::Error>> {
        setup_dirs();
        let _ = progbase::try_init();
        let _ = logger::init_test_logger();
        let bus = zbus::connection::Builder::system()?
            .name(format!("is.centroid.{}.{}", "tfctest2", "tfctest2"))?
            .build()
            .await?;

        let mut slot = Slot::<bool>::new(bus.clone(), Base::new("slot5", None));
        let mut signal = Signal::<bool>::new(bus.clone(), Base::new("signal5", None));
        log::info!("Waiting for init");
        signal.init_task().await?;
        log::info!("Init complete");

        log::info!("Connecting");
        slot.async_connect(signal.full_name())
            .await
            .expect("This should connect");
        log::info!("Connected");
        let send_values = vec![
            true, true, false, false, true, true, false, false, true, true, false, false, true,
            true, false, false,
        ];
        let recv_values = vec![
            true, true, false, false, true, true, false, false, true, true, false, false, true,
            true, false, false,
        ];
        let send_task = tokio::spawn(async move {
            // This sleep makes sure that the receiver has already started and is listening before sending
            tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
            for val in send_values {
                signal.async_send(val).await.expect("This should not fail");
            }
        });

        let recv_task = tokio::spawn(async move {
            for val in recv_values {
                let recv_val = slot.async_recv().await;
                // filtered out values are Errors, newstate filter
                // the recv_values vector contains all values with filtered out values removed
                if let Ok(recv_val) = recv_val {
                    assert_eq!(recv_val.lock().unwrap(), val);
                } else {
                    assert_eq!(recv_val.err().unwrap().to_string(), "Same as last value");
                }
            }
        });

        let (recv_res, send_res) = tokio::join!(recv_task, send_task);

        send_res.expect("Sender task panicked");
        recv_res.expect("Receiver task panicked");
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_public_recv_after_send() -> Result<(), Box<dyn std::error::Error>> {
        setup_dirs();
        let _ = progbase::try_init();
        let _ = logger::init_combined_logger();

        let bus = zbus::connection::Builder::system()?
            .name(format!("is.centroid.{}.{}", "tfctest3", "tfctest3"))?
            .build()
            .await?;

        let mut slot = Slot::<bool>::new(bus.clone(), Base::new("slot6", None));
        let mut signal = Signal::<bool>::new(bus.clone(), Base::new("signal6", None));
        signal.init_task().await?;

        slot.async_connect(signal.full_name())
            .await
            .expect("This should connect");

        signal.async_send(true).await.expect("This should not fail");

        let recv =
            tokio::time::timeout(tokio::time::Duration::from_secs(1), slot.async_recv()).await;

        let value = match recv {
            Ok(inner_result) => match inner_result {
                Ok(value) => value,
                Err(e) => {
                    panic!("Error receiving value: {:?}", e);
                }
            },
            Err(_) => {
                panic!("Timeout occurred");
            }
        };

        assert_eq!(value.lock().unwrap(), true);

        Ok(())
    }
    #[tokio::test]
    async fn initialize_button() {
        let bus = zbus::connection::Builder::system()
            .expect("Build a bus")
            .build()
            .await
            .expect("Build success");
        let mut slot = Slot::<bool>::new(bus.clone(), Base::new("test_slot", None));
        let mut signal = Signal::<bool>::new(bus, Base::new("test_signal", None));

        // Connect them together
        slot.connect(signal.full_name()).expect("Connect success");
        //tokio::time::sleep(Duration::from_millis(1000)).await;

        // Send some values and check they are correct
        // Send true value
        signal.async_send(true).await.expect("Send success");
        let value =
            (*slot.async_recv().await.expect("Have some value").lock()).expect("Got some value");
        assert_eq!(value, true);

        // Send false value
        signal.async_send(false).await.expect("Send success");
        tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        assert_eq!(slot.value().expect("Has value"), false);
    }
    // #[tokio::test]
    // async fn basic_send_recv_test() -> Result<(), Box<dyn std::error::Error>> {
    //     progbase::init();

    //     let _ = logger::init_combined_logger();

    //     let bus = zbus::connection::Builder::system()?
    //         .name(format!("is.centroid.{}.{}", "tfctest", "tfctest"))?
    //         .build()
    //         .await?;
    //     let mut my_little_slot = Slot::<bool>::new(bus.clone(), Base::new("my_little_slot", None));
    //     let mut my_little_signal = Signal::<bool>::new(bus, Base::new("my_little_signal", None));

    //     let vals_to_send = vec![
    //         false, false, true, true, false, false, true, true, false, false, true, true, false,
    //         false,
    //     ];
    //     let vals_to_recv = vec![false, true, false, true, false, true, false];

    //     my_little_slot
    //         .connect(my_little_signal.full_name())
    //         .expect("This should connect");

    //     let mut stream = my_little_slot.stream();
    //     let recv_task = tokio::spawn(async move {
    //         let mut counter = 0;
    //         for expected_val in vals_to_recv {
    //             tokio::select! {
    //                 val = stream.next() => {
    //                     assert_eq!(val.expect("This should not fail"), expected_val);
    //                     counter += 1;
    //                 },
    //                 _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
    //                     panic!("No value received for 1 second, only received {} values", counter);
    //                 }
    //             }
    //         }
    //     });

    //     let send_task = tokio::spawn(async move {
    //         for val in vals_to_send {
    //             my_little_signal
    //                 .async_send(val)
    //                 .await
    //                 .expect("This should not fail");
    //         }
    //     });

    //     let (recv_res, send_res) = tokio::join!(recv_task, send_task);

    //     recv_res.expect("Receiver task panicked");

    //     send_res.expect("Sender task panicked");

    //     Ok(())
    // }
}
