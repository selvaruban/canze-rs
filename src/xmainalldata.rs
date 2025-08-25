extern crate ctrlc;
use bluer::{
    rfcomm::{SocketAddr, Stream},
    Address,
};
use clap::Parser;
use ini::Ini;
use simplelog::*;
use std::io::{self, Error, ErrorKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;
use serde::Serialize;
use reqwest::Client;

/// secs between polling
pub const POLL_INTERVAL_SECS: f32 = 10.0;
/// secs between polling when car is in sleep mode or is not in range
pub const CAR_SLEEP_INTERVAL_SECS: f32 = 100.0;

const INIT: &[&str] = &["ATZ", "ATE0", "ATAL", "ATCP18", "ATFCSD300000", "ATSP6"];
const _EOM1: u8 = b'\r';
const EOM2: u8 = b'>';
const _EOM3: u8 = b'?';

// Just a generic Result type to ease error handling for us. Errors in multithreaded
// async contexts needs some extra restrictions
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Serialize)]
struct CarData {
    // The API expects this name.
    #[serde(rename = "battery_level_percentage")]
    soc: Option<f32>,
    #[serde(rename = "external_temp_celsius")]
    external_temp: Option<f32>,
    soh: Option<f32>,
    battery_capacity_wh: Option<u64>,
}

/// Simple daemon to read Renault Zoe (adapted for MG4) basic parameters using
/// bluetooth dongle and save it in the InfluxDB database
#[derive(Parser, Debug)]
#[clap(version, about, long_about = None)]
struct Args {
    /// Enable debug info
    #[clap(short, long)]
    debug: bool,

    /// Config file path
    #[clap(short, long, parse(from_os_str), default_value = "/etc/canze-rs.conf")]
    config: std::path::PathBuf,
}

pub struct Parameter {
    name: String,
    desc: String,
    unit: Option<&'static str>,
    cmd: u32,
    reg_address: u16,
    reg_address2: u16,
    convert: Box<dyn Fn(u32) -> io::Result<f32>>,
}

impl Parameter {
    pub fn new(
        name: &'static str,
        desc: &'static str,
        unit: Option<&'static str>,
        cmd: u32,
        reg_address: u16,
        reg_address2: u16,
        convert: Box<dyn Fn(u32) -> io::Result<f32>>,
    ) -> Self {
        Self {
            name: String::from(name),
            desc: String::from(desc),
            unit,
            cmd,
            reg_address,
            reg_address2,
            convert,
        }
    }
}

fn create_params_table() -> Vec<Parameter> {
    // ⚠️ This function has been updated with the MG4's CAN IDs and conversion logic.
    vec![
        Parameter::new(
            "soc",
            "State of Charge",
            Some("%"),
            0x22b046, // The CAN ID for the MG4's SOC.
            0x000,    // Placeholder.
            0x000,    // Placeholder.
            Box::new(|val| {
                // Conversion logic: INT16(A:B)/10.0, then apply +4.4% correction.
                let soc_value = (val as i16) as f32 / 10.0;
                Ok(soc_value + 4.4)
            }),
        ),
        Parameter::new(
            "external_temp_celsius",
            "External Temperature",
            Some("°C"),
            0x22e01b, // The correct CAN ID for external temperature
            0x000,
            0x000,
            Box::new(|val| {
                // Conversion logic: INT16(A:B) / 10.0 - 40
                let temp_value = (val as i16) as f32 / 10.0 - 40.0;
                Ok(temp_value)
            }),
        ),
        Parameter::new(
            "soh",
            "State of Health",
            Some("%"),
            0x22b061, // The CAN ID for the MG4's SOH
            0x000,
            0x000,
            Box::new(|val| {
                // Conversion logic: INT16(A:B) / 100
                let soh_value = (val as i16) as f32 / 100.0;
                Ok(soh_value)
            }),
        ),
    ]
}

fn logging_init(debug: bool) {
    let conf = ConfigBuilder::new()
        .set_time_format("%F, %H:%M:%S%.3f".to_string())
        .set_write_log_enable_colors(true)
        .build();

    let mut loggers = vec![];

    let console_logger: Box<dyn SharedLogger> = TermLogger::new(
        if debug {
            LevelFilter::Debug
        } else {
            LevelFilter::Info
        },
        conf.clone(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    );
    loggers.push(console_logger);

    CombinedLogger::init(loggers).expect("Cannot initialize logging subsystem");
}

fn get_config_string(conf: Ini, option_name: &str, section: Option<&str>) -> io::Result<String> {
    conf.section(Some(section.unwrap_or("general").to_owned()))
        .and_then(|x| x.get(option_name).cloned())
        .ok_or(Error::new(
            ErrorKind::Other,
            format!("No config entry for: `{option_name}`"),
        ))
}

pub async fn send_cmd(stream: &mut Stream, cmd: String) -> io::Result<Option<Vec<u8>>> {
    let mut buffer = vec![0u8; 512];
    let mut output_cmd: Vec<u8> = vec![];
    let out: Option<Vec<u8>>;

    output_cmd.extend(cmd.as_bytes());
    output_cmd.push(b'\r');
    debug!("write: {}", String::from_utf8_lossy(&output_cmd));
    if let Err(e) = stream.write_all(&output_cmd).await {
        error!("write error: {:?}", e);
        return Err(e.into());
    }

    let mut packet = BufReader::new(stream);
    let retval = packet.read_until(EOM2, &mut buffer);
    match timeout(Duration::from_secs_f32(5.0), retval).await {
        Ok(res) => match res {
            Ok(len) => {
                if len == 0 {
                    error!("file read error: 0 bytes");
                    return Err(Error::new(ErrorKind::Other, "0 bytes read"));
                }
                out = Some(buffer.clone());
                trace!("Response: {:?}", buffer);
                let ascii = String::from_utf8_lossy(&buffer);
                debug!("Response ASCII (len={}): {}", len, ascii);
                if ascii.contains("NO DATA") {
                    return Err(Error::new(ErrorKind::Other, "no data"));
                }
                if ascii.contains("7F 22 12") {
                    return Err(Error::new(ErrorKind::Other, "Service Not Supported"));
                }
            }
            Err(e) => {
                error!("file read error: {}", e);
                return Err(e.into());
            }
        },
        Err(e) => {
            error!("response timeout: {}", e);
            return Err(e.into());
        }
    }

    Ok(out)
}

// Function to get a single parameter value and handle the communication logic
pub async fn get_param_value(stream: &mut Stream, p: &Parameter) -> io::Result<f32> {
    let cmd = format!("ATSH{:02x}\r", p.reg_address2);
    send_cmd(stream, cmd).await?;
    let cmd = format!("ATCRA{:02x}\r", p.reg_address);
    send_cmd(stream, cmd).await?;
    let cmd = format!("ATFCSH{:02x}\r", p.reg_address2);
    send_cmd(stream, cmd).await?;
    let cmd = format!("10C0\r");
    let _ = send_cmd(stream, cmd).await;
    let cmd = format!("{:02x}\r", p.cmd);
    let out = send_cmd(stream, cmd).await?.unwrap();
    let mut raw_string = String::from_utf8_lossy(&out);
    raw_string = raw_string
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>()
        .into();

    if raw_string.len() < 6 {
        return Err(Error::new(ErrorKind::Other, "response empty or too short!"));
    }
    let val = u32::from_str_radix(&raw_string[raw_string.len() - 6..raw_string.len()], 16);
    if let Err(_) = val {
        return Err(Error::new(ErrorKind::Other, "conversion error!"));
    }
    
    // Use the associated parameter converter for a value
    let converted = (p.convert)(val.unwrap())?;
    
    Ok(converted)
}

// New function to send all collected data in a single POST request
async fn rest_save_car_data(client: &mut reqwest::Client, data: CarData) -> Result<()> {
    let endpoint = "http://localhost/battery";
    
    // Log the data being sent
    info!("Sending data to API: {:?}", data);

    let response = client
        .post(endpoint)
        .json(&data)
        .send()
        .await?;

    info!("Response: {}", response.text().await?);

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    logging_init(args.debug);
    info!("<b><blue>canze-rs</> started");
    info!("Using config file: <b><blue>{:?}</>", args.config);
    let conf = match Ini::load_from_file(args.config) {
        Ok(c) => c,
        Err(e) => {
            error!("Cannot open config file: {}", e);
            return Ok(());
        }
    };
    let mac = get_config_string(conf.clone(), "mac", None)?;

    //parse target mac address for bluetooth
    let target_addr: Address = mac.parse().expect("invalid address");
    let target_sa = SocketAddr::new(target_addr, 1u8);

    //Ctrl-C / SIGTERM support
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    let params = create_params_table();
    let mut poll_interval = Instant::now();
    let mut client = Client::new();

    'connect: loop {
        if !running.load(Ordering::SeqCst) {
            info!("🛑 Ctrl-C or SIGTERM signal detected, exiting...");
            break;
        }

        tokio::time::sleep(Duration::from_secs(10)).await;
        info!("Connecting to: {:?}", &target_sa);
        let res = Stream::connect(target_sa).await;
        let mut stream = if let Ok(s) = res {
            s
        } else {
            info!("Cannot connect (BT dongle not in range?)");
            continue;
        };

        // the following code is a workaround for a problem described here:
        // https://github.com/bluez/bluer/discussions/130#discussioncomment-8845113
        debug!("Local address before: {:?}", stream.as_ref().local_addr()?);
        let mut i = 0;
        while stream.as_ref().local_addr()?.addr == bluer::Address::any() {
            debug!("Waiting for local address...");
            tokio::time::sleep(Duration::from_secs(1)).await;
            i += 1;
            if i > 5 {
                break;
            }
        }

        info!("Successfully connected to OBD dongle!");
        info!("Local address: {:?}", stream.as_ref().local_addr()?);
        info!("Security: {:?}", stream.as_ref().security()?);

        info!("connected, poll interval: {}s", POLL_INTERVAL_SECS);

        for s in INIT {
            if let Err(_) = send_cmd(&mut stream, s.to_string()).await {
                info!("INIT error, reconnect");
                continue 'connect;
            }
        }

        'inner: loop {
            if !running.load(Ordering::SeqCst) {
                continue 'connect;
            }

            if poll_interval.elapsed() > Duration::from_secs(0) {
                poll_interval = Instant::now() + Duration::from_secs_f32(POLL_INTERVAL_SECS);

                let mut collected_data = CarData {
                    soc: None,
                    external_temp: None,
                    soh: None,
                    battery_capacity_wh: None,
                };

                for p in &params {
                    debug!("Trying to obtain: {} ({})", p.desc, p.name);
                    let result = get_param_value(&mut stream, p).await;
                    
                    match result {
                        Ok(val) => {
                            info!("{} ({}): {} {}", p.desc, p.name, val, p.unit.unwrap_or_default());
                            match p.name.as_str() {
                                "soc" => collected_data.soc = Some(val),
                                "external_temp_celsius" => collected_data.external_temp = Some(val),
                                "soh" => {
                                    collected_data.soh = Some(val);
                                    let nominal_capacity_wh: f32 = 61700.0; // 61.7 kWh
                                    let soh_percentage = val;
                                    let calculated_capacity = (soh_percentage / 100.0) * nominal_capacity_wh;
                                    collected_data.battery_capacity_wh = Some(calculated_capacity.round() as u64);
                                },
                                _ => {}
                            }
                        }
                        Err(e) => {
                            info!("GET PARAM error for: {}: {:?}", p.name, e);
                            if e.kind() == std::io::ErrorKind::AddrNotAvailable {
                                info!("CAN network down / car is sleeping... waiting 100s");
                                poll_interval = Instant::now() + Duration::from_secs_f32(CAR_SLEEP_INTERVAL_SECS);
                                continue 'inner;
                            }
                            if e.kind() == std::io::ErrorKind::BrokenPipe
                                || e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::NotConnected
                            {
                                info!("Broken pipe/TimedOut/NotConnected detected ... trying to reconnect");
                                continue 'connect;
                            }
                        }
                    }
                }
                
                // After collecting all data, send a single request
                if collected_data.soc.is_some() || collected_data.external_temp.is_some() || collected_data.soh.is_some() {
                    let _ = rest_save_car_data(&mut client, collected_data).await;
                }

                debug!("Got all params, sleeping 10 secs for next cycle");
            }

            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    Ok(())
}
