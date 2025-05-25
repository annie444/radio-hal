//! Provides common helpers for implementing radio utilities
//!
//! ## <https://github.com/rust-iot/radio-hal>
//! ## Copyright 2020-2022 Ryan Kurte

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::prelude::v1::*;
use std::string::String;
use std::time::SystemTime;

use libc::{self};

#[cfg(all(not(feature = "defmt"), feature = "log"))]
use log::{debug, info};

#[cfg(feature = "defmt")]
use defmt::{debug, info};

use clap::Parser;
use embedded_hal::delay::DelayNs;
use humantime::Duration as HumanDuration;

use byteorder::{ByteOrder, NetworkEndian};
use pcap_file::{
    DataLink,
    pcap::{PcapHeader, PcapPacket, PcapWriter},
};
use rolling_stats::Stats;

use crate::{
    Power, Receive, ReceiveInfo, Rssi, Transmit,
    blocking::{BlockingError, BlockingOptions, BlockingReceive, BlockingTransmit},
};

/// Basic operations supported by the helpers package
#[derive(Clone, Parser, PartialEq, Debug)]
pub enum Operation {
    #[clap(name = "tx")]
    /// Transmit a packet
    Transmit(TransmitOptions),

    #[clap(name = "rx")]
    /// Receive a packet
    Receive(ReceiveOptions),

    #[clap(name = "rssi")]
    /// Poll RSSI on the configured channel
    Rssi(RssiOptions),

    #[clap(name = "echo")]
    /// Echo back received messages (useful with Link Test mode)
    Echo(EchoOptions),

    #[clap(name = "ping-pong")]
    /// Link test (ping-pong) mode
    LinkTest(PingPongOptions),
}

pub fn do_operation<T, I, E>(radio: &mut T, operation: Operation) -> Result<(), BlockingError<E>>
where
    T: Transmit<Error = E>
        + Power<Error = E>
        + Receive<Info = I, Error = E>
        + Rssi<Error = E>
        + Power<Error = E>
        + DelayNs,
    I: ReceiveInfo + Default + std::fmt::Debug,
    E: std::fmt::Debug,
{
    let mut buff = [0u8; 1024];

    // TODO: the rest
    match operation {
        Operation::Transmit(options) => do_transmit(radio, options)?,
        Operation::Receive(options) => do_receive(radio, &mut buff, options).map(|_| ())?,
        Operation::Echo(options) => do_echo(radio, &mut buff, options).map(|_| ())?,
        Operation::Rssi(options) => do_rssi(radio, options).map(|_| ())?,
        Operation::LinkTest(options) => do_ping_pong(radio, options).map(|_| ())?,
        //_ => warn!("unsuppored command: {:?}", opts.command),
    }

    Ok(())
}

/// Configuration for Transmit operation
#[derive(Clone, Parser, PartialEq, Debug)]
pub struct TransmitOptions {
    /// Data to be transmitted
    #[clap(long)]
    pub data: Vec<u8>,

    /// Power in dBm (range -18dBm to 13dBm)
    #[clap(long)]
    pub power: Option<i8>,

    /// Specify period for repeated transmission
    #[clap(long)]
    pub period: Option<HumanDuration>,

    #[clap(flatten)]
    pub blocking_options: BlockingOptions,
}

pub fn do_transmit<T, E>(radio: &mut T, options: TransmitOptions) -> Result<(), BlockingError<E>>
where
    T: Transmit<Error = E> + Power<Error = E> + DelayNs,
    E: core::fmt::Debug,
{
    // Set output power if specified
    if let Some(p) = options.power {
        radio.set_power(p)?;
    }

    loop {
        // Transmit packet
        radio.do_transmit(&options.data, options.blocking_options.clone())?;

        // Delay for repeated transmission or exit
        match &options.period {
            Some(p) => radio.delay_us(p.as_micros() as u32),
            None => break,
        }
    }

    Ok(())
}

/// Configuration for Receive operation
#[derive(Clone, Parser, PartialEq, Debug)]
pub struct ReceiveOptions {
    /// Run continuously
    #[clap(long = "continuous")]
    pub continuous: bool,

    #[clap(flatten)]
    pub pcap_options: PcapOptions,

    #[clap(flatten)]
    pub blocking_options: BlockingOptions,
}

#[derive(Clone, Parser, PartialEq, Debug)]

pub struct PcapOptions {
    /// Create and write capture output to a PCAP file
    #[clap(long, group = "1")]
    pub pcap_file: Option<String>,

    /// Create and write to a unix pipe for connection to wireshark
    #[clap(long, group = "1")]
    pub pcap_pipe: Option<String>,
}

impl PcapOptions {
    pub fn open(&self) -> Result<Option<PcapWriter<File>>, std::io::Error> {
        // Open file or pipe if specified
        let pcap_file = match (&self.pcap_file, &self.pcap_pipe) {
            // Open as file
            (Some(file), None) => {
                let f = File::create(file)?;
                Some(f)
            }
            // Open as pipe
            #[cfg(target_family = "unix")]
            (None, Some(pipe)) => {
                // Ensure file doesn't already exist
                let _ = std::fs::remove_file(pipe);

                // Create pipe
                let n = CString::new(pipe.as_str()).unwrap();
                let status = unsafe { libc::mkfifo(n.as_ptr(), 0o644) };

                // Manual status code handling
                // TODO: return io::Error
                if status != 0 {
                    panic!("Error creating fifo: {}", status);
                }

                // Open pipe
                let f = OpenOptions::new()
                    .write(true)
                    .open(pipe)
                    .expect("Error opening PCAP pipe");

                Some(f)
            }

            (None, None) => None,

            _ => unimplemented!(),
        };

        #[cfg(any(feature = "log", feature = "defmt"))]
        info!("pcap pipe open, awaiting connection");

        // Setup pcap writer and write header
        // (This is a blocking operation on pipes)
        let pcap_writer = match pcap_file {
            None => None,
            Some(f) => {
                // Setup pcap header
                let mut h = PcapHeader::default();
                h.datalink = DataLink::IEEE802_15_4;

                // Write header
                let w = PcapWriter::with_header(f, h).expect("Error writing to PCAP file");
                Some(w)
            }
        };

        Ok(pcap_writer)
    }
}

/// Receive from the radio using the provided configuration
pub fn do_receive<T, I, E>(
    radio: &mut T,
    mut buff: &mut [u8],
    options: ReceiveOptions,
) -> Result<usize, E>
where
    T: Receive<Info = I, Error = E> + DelayNs,
    I: std::fmt::Debug,
    E: std::fmt::Debug,
{
    // Create and open pcap file for writing
    let mut pcap_writer = options
        .pcap_options
        .open()
        .expect("Error opening pcap file / pipe");

    // Start receive mode
    radio.start_receive()?;

    loop {
        if radio.check_receive(true)? {
            let (n, i) = radio.get_received(&mut buff)?;

            match std::str::from_utf8(&buff[0..n as usize]) {
                Ok(s) => info!("Received: '{}' info: {:?}", s, i),
                #[cfg(not(feature = "defmt"))]
                Err(_) => info!("Received: '{:02x?}' info: {:?}", &buff[0..n as usize], i),
                #[cfg(feature = "defmt")]
                Err(_) => info!("Received: '{:?}' info: {:?}", &buff[0..n as usize], i),
            }

            if let Some(p) = &mut pcap_writer {
                let t = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap();

                p.write_packet(&PcapPacket::new(t, n as u32, &buff[0..n]))
                    .expect("Error writing pcap file");
            }

            if !options.continuous {
                return Ok(n);
            }

            radio.start_receive()?;
        }

        radio.delay_us(options.blocking_options.poll_interval.as_micros() as u32);
    }
}

/// Configuration for RSSI operation
#[derive(Clone, Parser, PartialEq, Debug)]
pub struct RssiOptions {
    /// Specify period for RSSI polling
    #[clap(long = "period", default_value = "1s")]
    pub period: HumanDuration,

    /// Run continuously
    #[clap(long = "continuous")]
    pub continuous: bool,
}

pub fn do_rssi<T, I, E>(radio: &mut T, options: RssiOptions) -> Result<(), E>
where
    T: Receive<Info = I, Error = E> + Rssi<Error = E> + DelayNs,
    I: std::fmt::Debug,
    E: std::fmt::Debug,
{
    // Enter receive mode
    radio.start_receive()?;

    // Poll for RSSI
    loop {
        let rssi = radio.poll_rssi()?;

        info!("rssi: {}", rssi);

        radio.check_receive(true)?;

        radio.delay_us(options.period.as_micros() as u32);

        if !options.continuous {
            break;
        }
    }

    Ok(())
}

/// Configuration for Echo operation
#[derive(Clone, Parser, PartialEq, Debug)]
pub struct EchoOptions {
    /// Run continuously
    #[clap(long = "continuous")]
    pub continuous: bool,

    /// Power in dBm (range -18dBm to 13dBm)
    #[clap(long = "power")]
    pub power: Option<i8>,

    /// Specify delay for response message
    #[clap(long = "delay", default_value = "100ms")]
    pub delay: HumanDuration,

    /// Append RSSI and LQI to repeated message
    #[clap(long = "append-info")]
    pub append_info: bool,

    #[clap(flatten)]
    pub blocking_options: BlockingOptions,
}

pub fn do_echo<T, I, E>(
    radio: &mut T,
    mut buff: &mut [u8],
    options: EchoOptions,
) -> Result<usize, BlockingError<E>>
where
    T: Receive<Info = I, Error = E> + Transmit<Error = E> + Power<Error = E> + DelayNs,
    I: ReceiveInfo + std::fmt::Debug,
    E: std::fmt::Debug,
{
    // Set output power if specified
    if let Some(p) = options.power {
        radio.set_power(p)?;
    }

    // Start receive mode
    radio.start_receive()?;

    loop {
        if radio.check_receive(true)? {
            // Fetch received packet
            let (mut n, i) = radio.get_received(&mut buff)?;

            // Parse out string if possible, otherwise print hex
            match std::str::from_utf8(&buff[0..n as usize]) {
                Ok(s) => info!("Received: '{}' info: {:?}", s, i),
                #[cfg(not(feature = "defmt"))]
                Err(_) => info!("Received: '{:02x?}' info: {:?}", &buff[0..n as usize], i),
                #[cfg(feature = "defmt")]
                Err(_) => info!("Received: '{:?}' info: {:?}", &buff[0..n as usize], i),
            }

            // Append info if provided
            if options.append_info {
                NetworkEndian::write_i16(&mut buff[n..], i.rssi());
                n += 2;
            }

            // Wait for turnaround delay
            radio.delay_us(options.delay.as_micros() as u32);

            // Transmit respobnse
            radio.do_transmit(&buff[..n], options.blocking_options.clone())?;

            // Exit if non-continuous
            if !options.continuous {
                return Ok(n);
            }
        }

        // Wait for poll delay
        radio.delay_us(options.blocking_options.poll_interval.as_micros() as u32);
    }
}

/// Configuration for Echo operation
#[derive(Clone, Parser, PartialEq, Debug)]
pub struct PingPongOptions {
    /// Specify the number of rounds to tx/rx
    #[clap(long, default_value = "100")]
    pub rounds: u32,

    /// Power in dBm (range -18dBm to 13dBm)
    #[clap(long)]
    pub power: Option<i8>,

    /// Specify delay for response message
    #[clap(long, default_value = "100ms")]
    pub delay: HumanDuration,

    /// Parse RSSI and other info from response messages
    /// (echo server must have --append-info set)
    #[clap(long)]
    pub parse_info: bool,

    #[clap(flatten)]
    pub blocking_options: BlockingOptions,
}

pub struct LinkTestInfo {
    pub sent: u32,
    pub received: u32,
    pub local_rssi: Stats<f32>,
    pub remote_rssi: Stats<f32>,
}

pub fn do_ping_pong<T, I, E>(
    radio: &mut T,
    options: PingPongOptions,
) -> Result<LinkTestInfo, BlockingError<E>>
where
    T: Receive<Info = I, Error = E> + Transmit<Error = E> + Power<Error = E> + DelayNs,
    I: ReceiveInfo,
    E: std::fmt::Debug,
{
    let mut link_info = LinkTestInfo {
        sent: options.rounds,
        received: 0,
        local_rssi: Stats::new(),
        remote_rssi: Stats::new(),
    };

    let mut buff = [0u8; 32];

    // Set output power if specified
    if let Some(p) = options.power {
        radio.set_power(p)?;
    }

    for i in 0..options.rounds {
        // Encode message
        NetworkEndian::write_u32(&mut buff[0..], i as u32);
        let n = 4;

        #[cfg(any(feature = "log", feature = "defmt"))]
        debug!("Sending message {}", i);

        // Send message
        radio.do_transmit(&buff[0..n], options.blocking_options.clone())?;

        // Await response
        let (n, info) = match radio.do_receive(&mut buff, options.blocking_options.clone()) {
            Ok(r) => r,
            Err(BlockingError::Timeout) => {
                #[cfg(any(feature = "log", feature = "defmt"))]
                debug!("Timeout awaiting response {}", i);
                continue;
            }
            Err(e) => return Err(e),
        };

        let receive_index = NetworkEndian::read_u32(&buff[0..n]);
        if receive_index != i {
            #[cfg(any(feature = "log", feature = "defmt"))]
            debug!("Invalid receive index");
            continue;
        }

        // Parse info if provided
        let remote_rssi = match options.parse_info {
            true => Some(NetworkEndian::read_i16(&buff[4..n])),
            false => None,
        };

        #[cfg(any(feature = "log", feature = "defmt"))]
        debug!(
            "Received response {} with local rssi: {} and remote rssi: {:?}",
            receive_index,
            info.rssi(),
            remote_rssi
        );

        link_info.received += 1;
        link_info.local_rssi.update(info.rssi() as f32);
        if let Some(rssi) = remote_rssi {
            link_info.remote_rssi.update(rssi as f32);
        }

        // Wait for send delay
        radio.delay_us(options.delay.as_micros() as u32);
    }

    Ok(link_info)
}
