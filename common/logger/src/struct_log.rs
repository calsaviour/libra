// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use chrono::Utc;
use once_cell::sync::Lazy;
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::HashMap,
    env,
    fs::{File, OpenOptions},
    io,
    io::Write as IoWrite,
    marker::PhantomData,
    net::UdpSocket,
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
};

pub trait StructLogSink: Sync {
    fn send(&self, entry: StructuredLogEntry);
}

// This is poor's man AtomicReference from crossbeam
// It have few unsafe lines, but does not require extra dependency
static NOP: NopStructLog = NopStructLog {};
static mut STRUCT_LOGGER: &'static dyn StructLogSink = &NOP;
static STRUCT_LOGGER_STATE: AtomicUsize = AtomicUsize::new(UNINITIALIZED);
const UNINITIALIZED: usize = 0;

const INITIALIZING: usize = 1;
const INITIALIZED: usize = 2;

// severity level - lower is worse
const SEVERITY_CRITICAL: usize = 1;
const SEVERITY_WARNING: usize = 2;

#[derive(Default, Serialize)]
pub struct StructuredLogEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    log: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pattern: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    module: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_rev: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    severity: Option<usize>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    data: HashMap<&'static str, Value>,
}

#[must_use = "use StructuredLogEntry::send to send structured log"]
impl StructuredLogEntry {
    pub fn new_unnamed() -> Self {
        let mut ret = Self::default();
        ret.timestamp = Some(Utc::now().format("%F %T").to_string());
        ret
    }

    pub fn new_named(name: &'static str) -> Self {
        let mut ret = Self::default();
        ret.name = Some(name);
        ret.timestamp = Some(Utc::now().format("%F %T").to_string());
        ret
    }

    pub fn critical(mut self) -> Self {
        self.severity = Some(SEVERITY_CRITICAL);
        self
    }

    pub fn warning(mut self) -> Self {
        self.severity = Some(SEVERITY_WARNING);
        self
    }

    pub fn json_data(mut self, key: &'static str, value: Value) -> Self {
        self.data.insert(key, value);
        self
    }

    pub fn data<D: Serialize>(mut self, key: &'static str, value: D) -> Self {
        self.data.insert(
            key,
            serde_json::to_value(value).expect("Failed to serialize StructuredLogEntry key"),
        );
        self
    }

    pub fn field<D: Serialize>(self, field: &LoggingField<D>, value: D) -> Self {
        self.data(field.0, value)
    }

    #[doc(hidden)] // set from macro
    pub fn log(&mut self, log: String) -> &mut Self {
        self.log = Some(log);
        self
    }

    #[doc(hidden)] // set from macro
    pub fn pattern(&mut self, pattern: &'static str) -> &mut Self {
        self.pattern = Some(pattern);
        self
    }

    #[doc(hidden)] // set from macro
    pub fn module(&mut self, module: &'static str) -> &mut Self {
        self.module = Some(module);
        self
    }

    #[doc(hidden)] // set from macro
    pub fn location(&mut self, location: &'static str) -> &mut Self {
        self.location = Some(location);
        self
    }

    #[doc(hidden)] // set from macro
    pub fn git_rev(&mut self, git_rev: Option<&'static str>) -> &mut Self {
        self.git_rev = git_rev;
        self
    }

    #[doc(hidden)] // set from macro
    pub fn data_mutref<D: Serialize>(&mut self, key: &'static str, value: D) -> &mut Self {
        self.data.insert(
            key,
            serde_json::to_value(value).expect("Failed to serialize StructuredLogEntry key"),
        );
        self
    }

    // Use send_struct_log! macro instead of this method to populate extra meta information such as git rev and module name
    #[doc(hidden)]
    pub fn send(self) {
        struct_logger().send(self);
    }
}

/// Field is similar to .data but restricts type of the value to a specific type.
///
/// Example:
///
/// mod logging {
///    pub const MY_FIELD:LoggingField<u64> = LoggingField::new("my_field");
/// }
///
/// mod my_code {
///    fn my_fn() {
///        send_struct_log!(StructuredLogEntry::new(...).field(&logging::MY_FIELD, 0))
///    }
/// }
pub struct LoggingField<D>(&'static str, PhantomData<D>);

impl<D> LoggingField<D> {
    pub const fn new(name: &'static str) -> Self {
        Self(name, PhantomData)
    }
}

// This is exact copy of similar function in log crate
/// Sets structured logger
pub fn set_struct_logger(logger: &'static dyn StructLogSink) -> Result<(), ()> {
    unsafe {
        match STRUCT_LOGGER_STATE.compare_and_swap(UNINITIALIZED, INITIALIZING, Ordering::SeqCst) {
            UNINITIALIZED => {
                STRUCT_LOGGER = logger;
                STRUCT_LOGGER_STATE.store(INITIALIZED, Ordering::SeqCst);
                Ok(())
            }
            INITIALIZING => {
                while STRUCT_LOGGER_STATE.load(Ordering::SeqCst) == INITIALIZING {}
                Err(())
            }
            _ => Err(()),
        }
    }
}

static STRUCT_LOG_LEVEL: Lazy<log::Level> = Lazy::new(|| {
    let level = env::var("STRUCT_LOG_LEVEL").unwrap_or_else(|_| "debug".to_string());
    log::Level::from_str(&level).expect("Failed to parse log level")
});

/// Checks if structured logging is enabled for level
pub fn struct_logger_enabled(level: log::Level) -> bool {
    struct_logger_set() && level <= *STRUCT_LOG_LEVEL
}

/// Checks if structured logging is enabled
pub fn struct_logger_set() -> bool {
    STRUCT_LOGGER_STATE.load(Ordering::SeqCst) == INITIALIZED
}

/// Initializes struct logger from STRUCT_LOG_FILE env var.
/// If STRUCT_LOG_FILE is set, STRUCT_LOG_UDP_ADDR will be ignored.
/// Can only be called once
pub fn init_struct_log_from_env() -> Result<(), InitLoggerError> {
    if let Ok(file) = env::var("STRUCT_LOG_FILE") {
        init_file_struct_log(file)
    } else if let Ok(udp_address) = env::var("STRUCT_LOG_UDP_ADDR") {
        init_udp_struct_log(udp_address)
    } else {
        Ok(())
    }
}

/// Initializes struct logger sink that writes to specified file.
/// Can only be called once
pub fn init_file_struct_log(file_path: String) -> Result<(), InitLoggerError> {
    let logger = FileStructLog::start_new(file_path).map_err(InitLoggerError::IoError)?;
    let logger = Box::leak(Box::new(logger));
    set_struct_logger(logger).map_err(|_| InitLoggerError::StructLoggerAlreadySet)
}

/// Initializes struct logger sink that stream logs through UDP protocol.
/// Can only be called once
pub fn init_udp_struct_log(udp_address: String) -> Result<(), InitLoggerError> {
    let logger = UDPStructLog::start_new(udp_address).map_err(InitLoggerError::IoError)?;
    let logger = Box::leak(Box::new(logger));
    set_struct_logger(logger).map_err(|_| InitLoggerError::StructLoggerAlreadySet)
}

/// Initialize struct logger sink that prints all structured logs to stdout
/// Can only be called once
pub fn init_println_struct_log() -> Result<(), ()> {
    let logger = PrintStructLog {};
    let logger = Box::leak(Box::new(logger));
    set_struct_logger(logger)
}

#[derive(Debug)]
pub enum InitLoggerError {
    IoError(io::Error),
    StructLoggerAlreadySet,
}

// This is exact copy of similar function in log crate
fn struct_logger() -> &'static dyn StructLogSink {
    unsafe {
        if STRUCT_LOGGER_STATE.load(Ordering::SeqCst) != INITIALIZED {
            &NOP
        } else {
            STRUCT_LOGGER
        }
    }
}

struct NopStructLog {}

impl StructLogSink for NopStructLog {
    fn send(&self, _entry: StructuredLogEntry) {}
}

struct PrintStructLog {}

impl StructLogSink for PrintStructLog {
    fn send(&self, entry: StructuredLogEntry) {
        println!("{}", serde_json::to_string(&entry).unwrap());
    }
}

/// Sink that prints all structured logs to specified file
struct FileStructLog {
    sender: SyncSender<StructuredLogEntry>,
}

impl FileStructLog {
    /// Creates new FileStructLog and starts async thread to write results
    pub fn start_new(file_path: String) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(1_024);
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(file_path)?;
        let sink_thread = FileStructLogThread { receiver, file };
        thread::spawn(move || sink_thread.run());
        Ok(Self { sender })
    }
}

impl StructLogSink for FileStructLog {
    fn send(&self, entry: StructuredLogEntry) {
        if let Err(e) = self.sender.try_send(entry) {
            // Use log crate macro to avoid generation of structured log in this case
            // Otherwise we will have infinite loop
            log::error!("Failed to send structured log: {}", e);
        }
    }
}

struct FileStructLogThread {
    receiver: Receiver<StructuredLogEntry>,
    file: File,
}

impl FileStructLogThread {
    pub fn run(mut self) {
        for entry in self.receiver {
            let json = match serde_json::to_value(entry) {
                Err(e) => {
                    log::error!("Failed to serialize struct log entry: {}", e);
                    continue;
                }
                Ok(json) => json,
            };
            if let Err(e) = writeln!(&mut self.file, "{}", json) {
                log::error!("Failed to write struct log entry: {}", e);
            }
        }
    }
}

/// Sink that streams all structured logs to an address through UDP protocol
struct UDPStructLog {
    sender: SyncSender<StructuredLogEntry>,
}

impl UDPStructLog {
    /// Creates new UDPStructLog and starts async thread to send results
    pub fn start_new(udp_address: String) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(1_024);
        let socket = UdpSocket::bind("0.0.0.0:0").expect("couldn't bind to address");
        let sink_thread = UDPStructLogThread {
            receiver,
            socket,
            udp_address,
        };
        thread::spawn(move || sink_thread.run());
        Ok(Self { sender })
    }
}

impl StructLogSink for UDPStructLog {
    fn send(&self, entry: StructuredLogEntry) {
        if let Err(e) = self.sender.try_send(entry) {
            // Use log crate macro to avoid generation of structured log in this case
            // Otherwise we will have infinite loop
            log::error!("[Logging] Failed to send structured log: {}", e);
        }
    }
}

struct UDPStructLogThread {
    receiver: Receiver<StructuredLogEntry>,
    socket: UdpSocket,
    udp_address: String,
}

impl UDPStructLogThread {
    pub fn run(self) {
        for entry in self.receiver {
            let json = match serde_json::to_value(entry) {
                Err(e) => {
                    log::error!("[Logging] Failed to serialize struct log entry: {}", e);
                    continue;
                }
                Ok(json) => json,
            };
            match self
                .socket
                .send_to(json.to_string().as_bytes(), self.udp_address.clone())
            {
                Ok(_) => {
                    continue;
                }
                Err(e) => {
                    // do not break on error, move on to the next log message
                    println!("[Logging] Error while sending data to socket: {}", e);
                }
            }
        }
    }
}
