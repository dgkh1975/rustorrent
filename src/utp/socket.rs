
use async_std::net::{UdpSocket, SocketAddr};
use async_std::io::{ErrorKind, Error};
use rand::Rng;

use std::time::{Duration, Instant};
use std::{iter::Iterator, collections::VecDeque};
use std::iter;

use super::{
    ConnectionId, Result, UtpError, Packet, PacketRef, PacketType,
    Header, Delay, Timestamp, SequenceNumber, HEADER_SIZE,
    UDP_IPV4_MTU, UDP_IPV6_MTU, DelayHistory,
};
use crate::udp_ext::WithTimeout;

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum State {
    /// not yet connected
	None,
	/// sent a syn packet, not received any acks
	SynSent,
	/// syn-ack received and in normal operation
	/// of sending and receiving data
	Connected,
	/// fin sent, but all packets up to the fin packet
	/// have not yet been acked. We might still be waiting
	/// for a FIN from the other end
	FinSent,
	// /// ====== states beyond this point =====
	// /// === are considered closing states ===
	// /// === and will cause the socket to ====
	// /// ============ be deleted =============
	// /// the socket has been gracefully disconnected
	// /// and is waiting for the client to make a
	// /// socket call so that we can communicate this
	// /// fact and actually delete all the state, or
	// /// there is an error on this socket and we're
	// /// waiting to communicate this to the client in
	// /// a callback. The error in either case is stored
	// /// in m_error. If the socket has gracefully shut
	// /// down, the error is error::eof.
	// ErrorWait,
	// /// there are no more references to this socket
	// /// and we can delete it
	// Delete
}

const BASE_HISTORY: usize = 10;
const INIT_CWND: u32 = 2;
const MIN_CWND: u32 = 2;
/// Sender's Maximum Segment Size
/// Set to Ethernet MTU
const MSS: u32 = 1400;
const TARGET: u32 = 100_000; //100;
const GAIN: u32 = 1;
const ALLOWED_INCREASE: u32 = 1;

pub struct UtpSocket {
    local: SocketAddr,
    remote: Option<SocketAddr>,
    udp: UdpSocket,
    recv_id: ConnectionId,
    send_id: ConnectionId,
    // seq_nr: u16,
    state: State,
    ack_number: SequenceNumber,
    seq_number: SequenceNumber,
    delay: Delay,

    /// advirtised window from the remote
    remote_window: u32,

    /// Packets sent but we didn't receive an ack for them
    inflight_packets: VecDeque<Packet>,

    // base_delays: VecDeque<Delay>,

    // current_delays: VecDeque<Delay>, // TODO: Use SliceDeque ?

    // last_rollover: Instant,

    // flight_size: u32,

    delay_history: DelayHistory,

    cwnd: u32,
    congestion_timeout: Duration,

    // /// SRTT (smoothed round-trip time)
    // srtt: u32,
    // /// RTTVAR (round-trip time variation)
    // rttvar: u32,
}

impl UtpSocket {
    fn new(local: SocketAddr, udp: UdpSocket) -> UtpSocket {
        let (recv_id, send_id) = ConnectionId::make_ids();

        // let mut base_delays = VecDeque::with_capacity(BASE_HISTORY);
        // base_delays.extend(iter::repeat(Delay::infinity()).take(BASE_HISTORY));

        UtpSocket {
            local,
            udp,
            recv_id,
            send_id,
            // base_delays,
            remote: None,
            state: State::None,
            ack_number: SequenceNumber::zero(),
            seq_number: SequenceNumber::random(),
            delay: Delay::default(),
            // current_delays: VecDeque::with_capacity(16),
            // last_rollover: Instant::now(),
            cwnd: INIT_CWND * MSS,
            congestion_timeout: Duration::from_secs(1),
            // flight_size: 0,
            // srtt: 0,
            // rttvar: 0,
            inflight_packets: VecDeque::with_capacity(64),
            remote_window: INIT_CWND * MSS,
            delay_history: DelayHistory::new(),
        }
    }

    pub async fn bind(addr: SocketAddr) -> Result<UtpSocket> {
        let udp = UdpSocket::bind(addr).await?;

        Ok(Self::new(addr, udp))
    }

    /// Addr must match the ip familly of the bind address (ipv4 / ipv6)
    pub async fn connect(&mut self, addr: SocketAddr) -> Result<()> {
        if addr.is_ipv4() != self.local.is_ipv4() {
            return Err(UtpError::FamillyMismatch);
        }

        self.udp.connect(addr).await?;

        let mut buffer = [0; 1500];
        let mut len = None;

        let mut header = Header::new(PacketType::Syn);
        header.set_connection_id(self.recv_id);
        header.set_seq_number(self.seq_number);
        header.set_window_size(1_048_576);
        self.seq_number += 1;

        for _ in 0..3 {
            header.update_timestamp();
            println!("SENDING {:#?}", header);

            self.udp.send(header.as_bytes()).await?;

            match self.udp.recv_from_timeout(&mut buffer, Duration::from_secs(1)).await {
                Ok((n, addr)) => {
                    len = Some(n);
                    self.remote = Some(addr);
                    self.state = State::SynSent;
                    break;
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                    continue;
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        };

        if let Some(len) = len {
            println!("CONNECTED", );
            let packet = PacketRef::ref_from_buffer(&buffer[..len])?;
            self.dispatch(packet).await?;
            return Ok(());
        }

        Err(Error::new(ErrorKind::TimedOut, "connect timed out").into())
    }

    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        let packet_size = self.packet_size();
        let packets = data.chunks(packet_size).map(Packet::new);

        for packet in packets {
            self.send_packet(packet).await?;
        }

        self.wait_for_reception().await
    }

    async fn wait_for_reception(&mut self) -> Result<()> {
        let last_seq = self.seq_number - 1;

        let mut is_last_acked = self.is_packet_acked(last_seq);

        while !is_last_acked {
            println!("LOOP IS ACKED", );
            self.receive_packet().await?;
            is_last_acked = self.is_packet_acked(last_seq);
        }

        Ok(())
    }

    fn is_packet_acked(&self, n: SequenceNumber) -> bool {
        !self.inflight_packets.iter().any(|p| p.get_seq_number() == n)
    }

    async fn send_packet(&mut self, mut packet: Packet) -> Result<()> {

        let packet_size = packet.size();
        let mut inflight_size = self.inflight_size();
        let mut window = self.cwnd.min(self.remote_window) as usize;

        while packet_size + inflight_size > window {
            self.receive_packet().await?;

            inflight_size = self.inflight_size();
            window = self.cwnd.min(self.remote_window) as usize;
        }

        packet.set_ack_number(self.ack_number);
        packet.set_seq_number(self.seq_number);
        packet.set_connection_id(self.send_id);
        packet.set_window_size(1_048_576);
        self.seq_number += 1;
        packet.update_timestamp();

        println!("SENDING {:#?}", &*packet);

        self.udp.send(packet.as_bytes()).await?;

        self.inflight_packets.push_back(packet);

        Ok(())
    }

    async fn receive_packet(&mut self) -> Result<()> {
        let mut buffer = [0; 1500];

        let mut timeout = self.congestion_timeout;
        let mut len = None;

        for _ in 0..3 {
            match self.udp.recv_timeout(&mut buffer, timeout).await {
                Ok(n) => {
                    len = Some(n);
                    break;
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                    timeout *= 2;
                    continue;
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }

        if let Some(len) = len {
            let packet = PacketRef::ref_from_buffer(&buffer[..len])?;
            self.dispatch(packet).await?;
            return Ok(());
        };

        Err(Error::new(ErrorKind::TimedOut, "timed out").into())
    }

    /// Returns the number of bytes currently in flight (sent but not acked)
    fn inflight_size(&self) -> usize {
        self.inflight_packets.iter().map(Packet::size).sum()
    }

    fn packet_size(&self) -> usize {
        let is_ipv4 = self.remote.map(|r| r.is_ipv4()).unwrap_or(true);

        // TODO: Change this when MTU discovery is implemented
        if is_ipv4 {
            UDP_IPV4_MTU - HEADER_SIZE
        } else {
            UDP_IPV6_MTU - HEADER_SIZE
        }
    }

    async fn dispatch(&mut self, packet: PacketRef<'_>) -> Result<()> {
        println!("DISPATCH HEADER: {:#?}", packet.header());

        self.delay = Delay::since(packet.get_timestamp());

        match (packet.get_type()?, self.state) {
            (PacketType::Syn, State::None) => {
                self.state = State::Connected;
                // Set self.remote
                let connection_id = packet.get_connection_id();
                self.recv_id = connection_id + 1;
                self.send_id = connection_id;
                self.seq_number = SequenceNumber::random();
                self.ack_number = packet.get_seq_number();
            }
            (PacketType::Syn, _) => {
            }
            (PacketType::State, State::SynSent) => {
                self.state = State::Connected;
                // Related:
                // https://engineering.bittorrent.com/2015/08/27/drdos-udp-based-protocols-and-bittorrent/
                // https://www.usenix.org/system/files/conference/woot15/woot15-paper-adamsky.pdf
                // https://github.com/bittorrent/libutp/commit/13d33254262d46b638d35c4bc1a2f76cea885760
                self.ack_number = packet.get_seq_number() - 1;
                self.remote_window = packet.get_window_size();
                println!("CONNECTED !", );
            }
            (PacketType::State, State::Connected) => {
                self.handle_state(packet);
                // let current_delay = packet.get_timestamp_diff();
                // let base_delay = std::cmp::min();
                // current_delay = acknowledgement.delay
                // base_delay = min(base_delay, current_delay)
                // queuing_delay = current_delay - base_delay
                // off_target = (TARGET - queuing_delay) / TARGET
                // cwnd += GAIN * off_target * bytes_newly_acked * MSS / cwnd
                // Ack received
            }
            (PacketType::State, _) => {
                // Wrong Packet
            }
            (PacketType::Data, _) => {
            }
            (PacketType::Fin, _) => {
            }
            (PacketType::Reset, _) => {
            }
        }

        Ok(())
    }

    // fn update_base_delay(&mut self, delay: Delay) {
    //     // # Maintain BASE_HISTORY delay-minima.
    //     // # Each minimum is measured over a period of a minute.
    //     // # 'now' is the current system time
    //     // if round_to_minute(now) != round_to_minute(last_rollover)
    //     //     last_rollover = now
    //     //     delete first item in base_delays list
    //     //     append delay to base_delays list
    //     // else
    //     //     base_delays.tail = MIN(base_delays.tail, delay)
    //     if self.last_rollover.elapsed() >= Duration::from_secs(1) {
    //         self.last_rollover = Instant::now();
    //         self.base_delays.pop_front();
    //         self.base_delays.push_back(delay);
    //     } else {
    //         let last = self.base_delays.pop_back().unwrap();
    //         self.base_delays.push_back(last.min(delay));
    //     }
    // }

    // fn update_current_delay(&mut self, delay: Delay) {
    //     //  # Maintain a list of CURRENT_FILTER last delays observed.
    //     // delete first item in current_delays list
    //     // append delay to current_delays list

    //     // TODO: Pop delays before the last RTT
    //     self.current_delays.pop_front();
    //     self.current_delays.push_back(delay);
    // }

    // fn filter_current_delays(&self) -> Delay {
    //     // TODO: Test other algos

    //     // We're using the exponentially weighted moving average (EWMA) function
    //     // Magic number from https://github.com/VividCortex/ewma
    //     let alpha = 0.032_786_885;
    //     let mut samples = self.current_delays.iter().map(|d| d.as_num() as f64);
    //     let first = samples.next().unwrap_or(0.0);
    //     (samples.fold(
    //         first,
    //         |acc, delay| alpha * delay + (acc * (1.0 - alpha))
    //     ) as i64).into()
    // }

    fn on_data_loss(&mut self) {
        // on data loss:
        // # at most once per RTT
        // cwnd = min (cwnd, max (cwnd/2, MIN_CWND * MSS))
        // if data lost is not to be retransmitted:
        //     flightsize = flightsize - bytes_not_to_be_retransmitted
        let cwnd = self.cwnd;
        self.cwnd = cwnd.min((cwnd / 2).max(MIN_CWND * MSS));
        // TODO:
        // if data lost is not to be retransmitted:
        //     flightsize = flightsize - bytes_not_to_be_retransmitted
    }

    fn on_congestion_timeout_expired(&mut self) {
        // if no ACKs are received within a CTO:
        // # extreme congestion, or significant RTT change.
        // # set cwnd to 1MSS and backoff the congestion timer.
        // cwnd = 1 * MSS
        self.cwnd = MSS;
        self.congestion_timeout *= 2;
    }

    fn handle_state(&mut self, packet: PacketRef<'_>) {
        let ack_number = packet.get_ack_number();
        let acked = self.inflight_packets.iter().find(|p| p.get_seq_number() == ack_number);
        let ackeds = self.inflight_packets.iter().filter(|p| p.get_seq_number().cmp_less_equal(ack_number));

        let nbytes = acked.unwrap().size();
        println!("NBYTES {:?}", nbytes);

        let delay = packet.get_timestamp_diff();
        if !delay.is_zero() {
            println!("ADDING DELAY {:?}", delay);
            self.delay_history.add_delay(delay);
        }

        println!("HISTORY: {:#?}", self.delay_history);

        // self.handle_ack(&packet, nbytes);

        self.inflight_packets.pop_front();
    }

    fn handle_ack(&mut self, packet: &PacketRef<'_>, bytes_newly_acked: usize) {
        // flightsize is the amount of data outstanding before this ACK
        //    was received and is updated later;
        // bytes_newly_acked is the number of bytes that this ACK
        //    newly acknowledges, and it MAY be set to MSS.
        println!("BEFORE CWND {:?}", self.cwnd);

        let delay = packet.get_timestamp_diff();
        // self.update_base_delay(delay);
        // self.update_current_delay(delay);

        // const std::int64_t window_factor = (std::int64_t(acked_bytes) * (1 << 16)) / in_flight;
	    // const std::int64_t delay_factor = (std::int64_t(target_delay - delay) * (1 << 16)) / target_delay;

        //let window_factor = bytes_newly_acked / self.inflight_size();
        //let delay_factor = TARGET -

        // let queuing_delay = self.filter_current_delays()
        //     - *self.base_delays.iter().min().unwrap();
        // let queuing_delay: i64 = queuing_delay.into();

        // let off_target = (TARGET as f64 - queuing_delay as f64) / TARGET as f64;

        //println!("FILTER {:?}", self.filter_current_delays());

        // TODO: Compute bytes_newly_acked;
        //let bytes_newly_acked = 61;

        // let cwnd = self.cwnd as f64 + ((GAIN as f64 * off_target as f64 * bytes_newly_acked as f64 * MSS as f64) / self.cwnd as f64);
        // let max_allowed_cwnd = self.inflight_size() + (ALLOWED_INCREASE * MSS) as usize;

        // println!("CWND {:?} MAX_ALLOWED {:?}", cwnd, max_allowed_cwnd);

        // let cwnd = (cwnd as u32).min(max_allowed_cwnd as u32);

        // println!("DELAY {:?} QUEUING_DELAY {:?} OFF_TARGET {:?}", delay, queuing_delay, off_target);

        // self.cwnd = cwnd.max(MIN_CWND * MSS);

        // println!("FINAL CWND {:?}", self.cwnd);
        //self.flight_size -= bytes_newly_acked;

//        let cwnd = std::cmp::min(cwnd, max_allowed_cwnd);

       // for each delay sample in the acknowledgement:
       //     delay = acknowledgement.delay
       //     update_base_delay(delay)
       //     update_current_delay(delay)

       // queuing_delay = FILTER(current_delays) - MIN(base_delays)
       // off_target = (TARGET - queuing_delay) / TARGET
       // cwnd += GAIN * off_target * bytes_newly_acked * MSS / cwnd
       // max_allowed_cwnd = flightsize + ALLOWED_INCREASE * MSS
       // cwnd = min(cwnd, max_allowed_cwnd)
       // cwnd = max(cwnd, MIN_CWND * MSS)
       // flightsize = flightsize - bytes_newly_acked
       // update_CTO()
    }

    fn update_congestion_timeout(&mut self) {
        // TODO
    }

    async fn send_ack(&mut self) -> Result<()> {
        let mut header = Header::new(PacketType::State);
        header.set_connection_id(self.send_id);
        header.set_seq_number(self.seq_number);
        header.set_ack_number(self.ack_number);
        self.seq_number += 1;

        Ok(())
    }
}
