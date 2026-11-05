//! One TCP connection's state machine (RFC 9293), with no I/O of its own.
//!
//! [`Tcb::on_segment`] takes what arrived, [`Tcb::on_timer`] takes the passage of
//! time and [`Tcb::next_segment`] gives what's to be sent, one at a time. It can all
//! run in a test, against a network that loses and reorders on purpose.
//!
//! What's here: retransmission with an RFC 6298 timer and go-back-N, fast retransmit,
//! slow start and congestion avoidance (RFC 5681), out-of-order reassembly, flow
//! control with zero-window probes, and every close state. What isn't: window scaling,
//! so a window tops out at 64 KiB, SACK, timestamps, delayed ACKs and Nagle.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::wire::{TCP_ACK, TCP_FIN, TCP_PSH, TCP_RST, TCP_SYN};

/// Largest segment this end can take: a 1500 byte MTU less the headers.
pub const LOCAL_MSS: u16 = 1460;
/// What's assumed when a peer's SYN doesn't say (RFC 9293).
const DEFAULT_MSS: u16 = 536;
/// Smaller than this from a peer is an attempt to make this host work hard.
const MIN_MSS: u16 = 88;
/// Most bytes queued for sending, sent or not.
pub const SEND_BUFFER: usize = 128 * 1024;
/// Most bytes held for the reader, and so the largest window offered.
pub const RECV_BUFFER: usize = 65535;
/// Most pieces held out of order.
const MAX_OUT_OF_ORDER: usize = 64;

/// Retransmission timeout before there's a measurement.
const INITIAL_RTO_MS: u64 = 1000;
/// Shortest retransmission timeout.
const MIN_RTO_MS: u64 = 200;
/// Longest retransmission timeout.
const MAX_RTO_MS: u64 = 30_000;
/// Times a segment is sent again before the connection is given up on.
const MAX_RETRIES: u32 = 10;
/// Times a SYN is sent again before the connection is given up on.
const MAX_SYN_RETRIES: u32 = 6;
/// How long TIME-WAIT lasts: twice a maximum segment lifetime of 15 s.
pub const TIME_WAIT_MS: u64 = 30_000;
/// How long a connection nobody can read from stays in FIN-WAIT-2.
const ORPHAN_FIN_WAIT_MS: u64 = 60_000;

/// `a` comes before `b`, in modular sequence space.
pub const fn seq_lt(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

/// `a` comes before `b` or is `b`.
pub const fn seq_le(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) <= 0
}

/// The states of RFC 9293. `LISTEN` belongs to the listener, which has no TCB.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// Gone, or never got anywhere.
    Closed,
    /// SYN sent, waiting for its answer.
    SynSent,
    /// A SYN came, and the answer's been sent.
    SynReceived,
    /// Data flows both ways.
    Established,
    /// This end has closed, and its FIN isn't acknowledged.
    FinWait1,
    /// This end's FIN is acknowledged, and the peer's still sending.
    FinWait2,
    /// The peer has closed, and this end still can send.
    CloseWait,
    /// Both ends have closed at once, and this end's FIN isn't acknowledged.
    Closing,
    /// The peer closed first, and this end's FIN isn't acknowledged.
    LastAck,
    /// All done, waiting out any strays.
    TimeWait,
}

impl State {
    /// The conventional name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Closed => "CLOSED",
            Self::SynSent => "SYN-SENT",
            Self::SynReceived => "SYN-RCVD",
            Self::Established => "ESTABLISHED",
            Self::FinWait1 => "FIN-WAIT-1",
            Self::FinWait2 => "FIN-WAIT-2",
            Self::CloseWait => "CLOSE-WAIT",
            Self::Closing => "CLOSING",
            Self::LastAck => "LAST-ACK",
            Self::TimeWait => "TIME-WAIT",
        }
    }
}

/// How a connection ended, when it didn't end well.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Failure {
    /// A reset answered the SYN.
    Refused,
    /// A reset came during the connection.
    Reset,
    /// Nothing got an answer, however many times it was sent.
    TimedOut,
}

/// A segment as far as TCP is concerned: the addressing is the caller's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Incoming<'a> {
    /// Sequence number.
    pub seq: u32,
    /// Acknowledgment number.
    pub ack: u32,
    /// Flags.
    pub flags: u8,
    /// Advertised window.
    pub window: u16,
    /// Maximum segment size option.
    pub mss: Option<u16>,
    /// Data.
    pub payload: &'a [u8],
}

/// A segment to send. Its data went into the buffer given to `next_segment`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outgoing {
    /// Sequence number.
    pub seq: u32,
    /// Acknowledgment number.
    pub ack: u32,
    /// Flags.
    pub flags: u8,
    /// Advertised window.
    pub window: u16,
    /// Maximum segment size option.
    pub mss: Option<u16>,
    /// How many bytes of data.
    pub len: usize,
}

/// The reset that answers a segment for which there's no connection (RFC 9293,
/// 3.10.7.1). `None` for a segment that's a reset itself.
pub const fn reset_for(segment: &Incoming<'_>) -> Option<Outgoing> {
    if segment.flags & TCP_RST != 0 {
        return None;
    }
    let (seq, ack, flags) = if segment.flags & TCP_ACK != 0 {
        (segment.ack, 0, TCP_RST)
    } else {
        let mut length = segment.payload.len() as u32;
        if segment.flags & TCP_SYN != 0 {
            length = length.wrapping_add(1);
        }
        if segment.flags & TCP_FIN != 0 {
            length = length.wrapping_add(1);
        }
        (0, segment.seq.wrapping_add(length), TCP_RST | TCP_ACK)
    };
    Some(Outgoing {
        seq,
        ack,
        flags,
        window: 0,
        mss: None,
        len: 0,
    })
}

/// One connection.
pub struct Tcb {
    state: State,
    failure: Option<Failure>,

    iss: u32,
    /// Oldest unacknowledged sequence number.
    snd_una: u32,
    /// Next sequence number to send.
    snd_nxt: u32,
    /// Highest sequence number ever sent, plus one. After a timeout `snd_nxt` goes
    /// back, and a retransmission is what's below this.
    snd_max: u32,
    snd_wnd: u32,
    /// Sequence and acknowledgment numbers of the last window update.
    snd_wl1: u32,
    snd_wl2: u32,
    /// Peer's largest segment.
    mss: u32,
    /// Bytes from `snd_una` on: the unacknowledged, then the unsent.
    send: VecDeque<u8>,
    /// `shutdown` was asked for.
    fin_queued: bool,
    /// The FIN has been acknowledged.
    fin_acked: bool,
    cwnd: u32,
    ssthresh: u32,
    dup_acks: u32,
    /// Send the oldest unacknowledged segment again, once.
    fast_retransmit: bool,
    /// Go past a zero window by one byte.
    probe: bool,
    /// Timeouts since anything was acknowledged.
    retries: u32,
    rto_ms: u64,
    srtt_ms: Option<u64>,
    rttvar_ms: u64,
    /// The segment being timed: when it ends in sequence space, and when it went.
    timing: Option<(u32, u64)>,
    /// When the retransmission timer fires.
    rto_deadline: Option<u64>,
    /// When a zero window gets probed.
    persist_deadline: Option<u64>,
    persist_ms: u64,

    irs: u32,
    rcv_nxt: u32,
    /// Right edge of the window this end has promised.
    rcv_adv: u32,
    /// In order and unread.
    recv: VecDeque<u8>,
    /// Out of order: `(seq, data)` sorted by sequence, not overlapping.
    out_of_order: Vec<(u32, Vec<u8>)>,
    out_of_order_bytes: usize,
    /// Where the peer's FIN is, when it came ahead of data.
    fin_seq: Option<u32>,
    /// The peer has finished, and `rcv_nxt` is past its FIN.
    fin_received: bool,
    /// Nothing more will be read: what arrives is acknowledged and dropped.
    orphaned: bool,

    ack_pending: bool,
    /// A reset to send: its sequence number.
    reset_pending: Option<u32>,
    /// When TIME-WAIT, or an orphan's FIN-WAIT-2, ends.
    close_deadline: Option<u64>,
    /// Segments sent more than once.
    retransmits: u64,
}

impl Tcb {
    fn new(state: State, iss: u32) -> Self {
        Self {
            state,
            failure: None,
            iss,
            snd_una: iss,
            snd_nxt: iss,
            snd_max: iss,
            snd_wnd: 0,
            snd_wl1: 0,
            snd_wl2: 0,
            mss: u32::from(DEFAULT_MSS),
            send: VecDeque::new(),
            fin_queued: false,
            fin_acked: false,
            cwnd: 0,
            ssthresh: u32::MAX,
            dup_acks: 0,
            fast_retransmit: false,
            probe: false,
            retries: 0,
            rto_ms: INITIAL_RTO_MS,
            srtt_ms: None,
            rttvar_ms: 0,
            timing: None,
            rto_deadline: None,
            persist_deadline: None,
            persist_ms: INITIAL_RTO_MS,
            irs: 0,
            rcv_nxt: 0,
            rcv_adv: 0,
            recv: VecDeque::new(),
            out_of_order: Vec::new(),
            out_of_order_bytes: 0,
            fin_seq: None,
            fin_received: false,
            orphaned: false,
            ack_pending: false,
            reset_pending: None,
            close_deadline: None,
            retransmits: 0,
        }
    }

    /// Starts an active open: the first [`next_segment`](Self::next_segment) is a SYN.
    pub fn connect(iss: u32) -> Self {
        Self::new(State::SynSent, iss)
    }

    /// A SYN came to a listener. The first segment out is the SYN-ACK.
    pub fn accept(segment: &Incoming<'_>, iss: u32) -> Self {
        let mut tcb = Self::new(State::SynReceived, iss);
        tcb.irs = segment.seq;
        tcb.rcv_nxt = segment.seq.wrapping_add(1);
        tcb.rcv_adv = tcb.rcv_nxt.wrapping_add(RECV_BUFFER as u32);
        tcb.set_mss(segment.mss);
        tcb.snd_wnd = u32::from(segment.window);
        tcb.snd_wl1 = segment.seq;
        tcb.snd_wl2 = iss;
        tcb
    }

    fn set_mss(&mut self, option: Option<u16>) {
        let mss = option.unwrap_or(DEFAULT_MSS).clamp(MIN_MSS, LOCAL_MSS);
        self.mss = u32::from(mss);
        self.cwnd = 4 * self.mss;
    }

    /// The state.
    pub const fn state(&self) -> State {
        self.state
    }

    /// Why it ended, when something went wrong.
    pub const fn failure(&self) -> Option<Failure> {
        self.failure
    }

    /// Segments sent more than once, since this was last asked.
    pub fn take_retransmits(&mut self) -> u64 {
        core::mem::take(&mut self.retransmits)
    }

    /// True once there's no more to send, and nothing else will come in.
    pub const fn is_closed(&self) -> bool {
        matches!(self.state, State::Closed)
    }

    /// True once the connection has been set up.
    pub const fn is_connected(&self) -> bool {
        !matches!(self.state, State::Closed | State::SynSent | State::SynReceived)
    }

    /// Room for more with [`send`](Self::send).
    pub fn send_space(&self) -> usize {
        SEND_BUFFER.saturating_sub(self.send.len())
    }

    /// Whether this end may still send data.
    pub const fn can_send(&self) -> bool {
        matches!(
            self.state,
            State::SynSent | State::SynReceived | State::Established | State::CloseWait
        ) && !self.fin_queued
    }

    /// Queues data, as much as there's room for, and returns how much that was.
    pub fn send(&mut self, data: &[u8]) -> usize {
        if !self.can_send() {
            return 0;
        }
        let take = data.len().min(self.send_space());
        self.send.extend(&data[..take]);
        take
    }

    /// Bytes queued and not yet acknowledged.
    pub fn unacknowledged(&self) -> usize {
        self.send.len()
    }

    /// True when everything queued, the FIN too if there was one, is acknowledged.
    pub fn is_flushed(&self) -> bool {
        self.send.is_empty() && (!self.fin_queued || self.fin_acked)
    }

    /// Bytes ready for [`recv`](Self::recv).
    pub fn readable(&self) -> usize {
        self.recv.len()
    }

    /// True when the peer has finished and it's all been read.
    pub fn at_end(&self) -> bool {
        self.recv.is_empty() && (self.fin_received || self.is_closed())
    }

    /// Reads what's in order. Opening up the window can need telling the peer.
    pub fn recv(&mut self, out: &mut [u8]) -> usize {
        let before = self.advertised_free();
        let take = out.len().min(self.recv.len());
        for (slot, byte) in out.iter_mut().zip(self.recv.drain(..take)) {
            *slot = byte;
        }
        let after = self.advertised_free();
        // Enough new room to matter, and the peer doesn't know.
        if after > before && (before == 0 || after - before >= self.mss as usize) {
            self.ack_pending = true;
        }
        take
    }

    /// `shutdown`: this end has no more to send.
    pub fn shutdown(&mut self) {
        if self.fin_queued {
            return;
        }
        if matches!(self.state, State::SynReceived | State::Established | State::CloseWait) {
            self.fin_queued = true;
            self.state = match self.state {
                State::SynReceived => State::SynReceived,
                State::CloseWait => State::LastAck,
                _ => State::FinWait1,
            };
        } else if self.state == State::SynSent {
            // Nothing to say goodbye to.
            self.state = State::Closed;
        }
    }

    /// Nothing will read from this any more. What arrives is thrown away, and a
    /// FIN-WAIT-2 doesn't wait for ever.
    pub fn orphan(&mut self) {
        self.orphaned = true;
        self.recv.clear();
        self.recv.shrink_to_fit();
        self.shutdown();
    }

    /// Ends the connection, with a reset if the peer knows of it.
    pub fn abort(&mut self) {
        if !matches!(self.state, State::Closed | State::SynSent | State::TimeWait) {
            self.reset_pending = Some(self.snd_nxt);
        }
        self.state = State::Closed;
    }

    fn free_space(&self) -> usize {
        RECV_BUFFER.saturating_sub(self.recv.len())
    }

    /// Room to offer. Below a segment's worth it's zero, so that a peer isn't invited
    /// to send tiny ones (RFC 9293, 3.8.6.2.2).
    fn advertised_free(&self) -> usize {
        let free = self.free_space();
        if free < self.mss as usize {
            0
        } else {
            free
        }
    }

    fn window_to_advertise(&mut self) -> u16 {
        // Never pull the right edge back from what's been promised.
        let promised = self.rcv_adv.wrapping_sub(self.rcv_nxt) as usize;
        let promised = if seq_lt(self.rcv_adv, self.rcv_nxt) { 0 } else { promised };
        let window = self.advertised_free().max(promised).min(usize::from(u16::MAX));
        self.rcv_adv = self.rcv_nxt.wrapping_add(window as u32);
        window as u16
    }

    /// Takes one segment for this connection.
    pub fn on_segment(&mut self, segment: &Incoming<'_>, now: u64) {
        match self.state {
            State::Closed => {}
            State::SynSent => self.on_segment_syn_sent(segment, now),
            _ => self.on_segment_synchronized(segment, now),
        }
    }

    fn on_segment_syn_sent(&mut self, segment: &Incoming<'_>, now: u64) {
        let has_ack = segment.flags & TCP_ACK != 0;
        if has_ack && !(seq_lt(self.iss, segment.ack) && seq_le(segment.ack, self.snd_nxt)) {
            if segment.flags & TCP_RST == 0 {
                self.reset_pending = Some(segment.ack);
            }
            return;
        }
        if segment.flags & TCP_RST != 0 {
            if has_ack {
                self.fail(Failure::Refused);
            }
            return;
        }
        if segment.flags & TCP_SYN == 0 {
            return;
        }
        self.irs = segment.seq;
        self.rcv_nxt = segment.seq.wrapping_add(1);
        self.rcv_adv = self.rcv_nxt.wrapping_add(RECV_BUFFER as u32);
        self.set_mss(segment.mss);
        self.snd_wnd = u32::from(segment.window);
        self.snd_wl1 = segment.seq;
        self.snd_wl2 = segment.ack;
        self.ack_pending = true;
        if has_ack {
            self.snd_una = segment.ack;
            self.state = State::Established;
            self.sample_rtt(segment.ack, now);
            self.stop_retransmit_timer();
            self.retries = 0;
        } else {
            // Both ends opened at once: the next thing out is a SYN-ACK.
            self.state = State::SynReceived;
            self.snd_nxt = self.iss;
            self.stop_retransmit_timer();
        }
    }

    /// RFC 9293's check that a segment overlaps the receive window.
    fn acceptable(&self, segment: &Incoming<'_>) -> bool {
        let length = segment_length(segment);
        let window = self.rcv_adv.wrapping_sub(self.rcv_nxt);
        let window = if seq_lt(self.rcv_adv, self.rcv_nxt) { 0 } else { window };
        let start_in = seq_le(self.rcv_nxt, segment.seq) && seq_lt(segment.seq, self.rcv_nxt.wrapping_add(window));
        match (length, window) {
            (0, 0) => segment.seq == self.rcv_nxt,
            (0, _) => start_in,
            (_, 0) => false,
            (_, _) => {
                let end = segment.seq.wrapping_add(length - 1);
                start_in || (seq_le(self.rcv_nxt, end) && seq_lt(end, self.rcv_nxt.wrapping_add(window)))
            }
        }
    }

    fn on_segment_synchronized(&mut self, segment: &Incoming<'_>, now: u64) {
        // A repeat of the SYN that made this connection: its answer got lost.
        if self.state == State::SynReceived && segment.flags & TCP_SYN != 0 && segment.seq == self.irs {
            self.snd_nxt = self.iss;
            return;
        }
        if !self.acceptable(segment) {
            if segment.flags & TCP_RST == 0 {
                self.ack_pending = true;
            }
            // In TIME-WAIT, a FIN sent again means the last ACK was lost.
            if self.state == State::TimeWait && segment.flags & TCP_FIN != 0 {
                self.close_deadline = Some(now + TIME_WAIT_MS);
            }
            return;
        }

        if segment.flags & TCP_RST != 0 {
            // RFC 5961: only a reset exactly at the next sequence number counts.
            // Anything else in the window gets a challenge, and a real peer that's
            // lost the connection answers that with a reset that fits.
            if segment.seq == self.rcv_nxt {
                match self.state {
                    State::SynReceived | State::Established | State::FinWait1 | State::FinWait2 | State::CloseWait => {
                        self.fail(Failure::Reset)
                    }
                    _ => self.state = State::Closed,
                }
            } else {
                self.ack_pending = true;
            }
            return;
        }

        if segment.flags & TCP_SYN != 0 {
            // A challenge ACK (RFC 5961): a peer that really has restarted answers it
            // with a reset that fits.
            self.ack_pending = true;
            return;
        }

        if segment.flags & TCP_ACK == 0 {
            return;
        }

        if self.state == State::SynReceived {
            if seq_lt(self.snd_una, segment.ack) && seq_le(segment.ack, self.snd_max) {
                // A close asked for before now takes effect from here.
                self.state = if self.fin_queued { State::FinWait1 } else { State::Established };
                self.retries = 0;
                self.snd_wnd = u32::from(segment.window);
                self.snd_wl1 = segment.seq;
                self.snd_wl2 = segment.ack;
            } else {
                self.reset_pending = Some(segment.ack);
                return;
            }
        }

        if !self.on_ack(segment, now) {
            return;
        }

        self.take_data(segment);
        self.on_state_after_ack(now);
    }

    /// Processes the acknowledgment. `false` means to go no further with this segment.
    fn on_ack(&mut self, segment: &Incoming<'_>, now: u64) -> bool {
        let ack = segment.ack;
        if seq_lt(self.snd_max, ack) {
            // Acknowledges something never sent.
            self.ack_pending = true;
            return false;
        }

        if seq_lt(self.snd_una, ack) {
            self.on_ack_progress(ack, now);
        } else if ack == self.snd_una
            && segment.payload.is_empty()
            && segment.flags & TCP_FIN == 0
            && u32::from(segment.window) == self.snd_wnd
            && self.snd_max != self.snd_una
        {
            self.dup_acks += 1;
            if self.dup_acks == 3 {
                self.ssthresh = (self.flight() / 2).max(2 * self.mss);
                self.cwnd = self.ssthresh + 3 * self.mss;
                self.fast_retransmit = true;
                self.timing = None;
            } else if self.dup_acks > 3 {
                self.cwnd = self.cwnd.saturating_add(self.mss);
            }
        }

        // RFC 9293's rule for which segment gets to set the window.
        if seq_lt(self.snd_wl1, segment.seq)
            || (self.snd_wl1 == segment.seq && seq_le(self.snd_wl2, ack))
        {
            self.snd_wnd = u32::from(segment.window);
            self.snd_wl1 = segment.seq;
            self.snd_wl2 = ack;
        }
        if self.snd_wnd != 0 {
            self.persist_deadline = None;
            self.persist_ms = INITIAL_RTO_MS;
        } else if ack == self.snd_una {
            // It's there, and it just has no room: that's not a lost connection.
            self.retries = 0;
        }
        true
    }

    fn on_ack_progress(&mut self, ack: u32, now: u64) {
        let mut acked = ack.wrapping_sub(self.snd_una);
        self.sample_rtt(ack, now);
        // The SYN and FIN take up sequence numbers, not bytes in the buffer.
        if seq_lt(self.iss, ack) && self.snd_una == self.iss {
            acked -= 1;
        }
        let bytes = (acked as usize).min(self.send.len());
        self.send.drain(..bytes);
        self.snd_una = ack;
        if seq_lt(self.snd_nxt, ack) {
            self.snd_nxt = ack;
        }
        if self.fin_queued && self.send.is_empty() && acked as usize > bytes {
            self.fin_acked = true;
        }

        if self.dup_acks >= 3 {
            // Fast recovery ends here.
            self.cwnd = self.ssthresh;
        } else if self.cwnd < self.ssthresh {
            self.cwnd = self.cwnd.saturating_add(self.mss.min(acked));
        } else {
            let growth = u64::from(self.mss) * u64::from(self.mss) / u64::from(self.cwnd.max(1));
            self.cwnd = self.cwnd.saturating_add(growth.max(1) as u32);
        }
        self.dup_acks = 0;
        self.retries = 0;
        self.fast_retransmit = false;
        self.probe = false;
        if self.snd_una == self.snd_max {
            self.stop_retransmit_timer();
        } else {
            self.rto_deadline = Some(now + self.rto_ms);
        }
    }

    fn on_state_after_ack(&mut self, now: u64) {
        match self.state {
            State::FinWait1 if self.fin_acked => {
                self.state = State::FinWait2;
                if self.orphaned {
                    self.close_deadline = Some(now + ORPHAN_FIN_WAIT_MS);
                }
                self.check_fin(now);
            }
            State::Closing if self.fin_acked => {
                self.state = State::TimeWait;
                self.close_deadline = Some(now + TIME_WAIT_MS);
            }
            State::LastAck if self.fin_acked => self.state = State::Closed,
            _ => self.check_fin(now),
        }
    }

    /// Takes in a segment's data and FIN, in order or not.
    fn take_data(&mut self, segment: &Incoming<'_>) {
        let receiving = matches!(self.state, State::Established | State::FinWait1 | State::FinWait2);
        if !segment.payload.is_empty() {
            self.ack_pending = true;
            if receiving {
                self.store_data(segment.seq, segment.payload);
                self.drain_in_order();
            }
        }
        if segment.flags & TCP_FIN != 0 {
            self.ack_pending = true;
            if receiving || self.state == State::SynReceived {
                self.fin_seq = Some(segment.seq.wrapping_add(segment.payload.len() as u32));
            }
        }
    }

    /// The peer's FIN takes effect when everything before it has arrived.
    fn check_fin(&mut self, now: u64) {
        let Some(fin) = self.fin_seq else {
            return;
        };
        if fin != self.rcv_nxt || self.fin_received {
            return;
        }
        self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
        self.fin_received = true;
        self.ack_pending = true;
        match self.state {
            State::SynReceived | State::Established => self.state = State::CloseWait,
            State::FinWait1 => self.state = State::Closing,
            State::FinWait2 => {
                self.state = State::TimeWait;
                self.close_deadline = Some(now + TIME_WAIT_MS);
            }
            _ => {}
        }
    }

    /// Holds data that fits the window, in sequence order.
    fn store_data(&mut self, seq: u32, data: &[u8]) {
        // What's already been received.
        let (mut seq, mut data) = (seq, data);
        if seq_lt(seq, self.rcv_nxt) {
            let skip = self.rcv_nxt.wrapping_sub(seq) as usize;
            if skip >= data.len() {
                return;
            }
            data = &data[skip..];
            seq = self.rcv_nxt;
        }
        // What's beyond the window goes when it fits.
        let window = self.free_space().min(usize::from(u16::MAX));
        let offset = seq.wrapping_sub(self.rcv_nxt) as usize;
        if offset >= window {
            return;
        }
        let data = &data[..data.len().min(window - offset)];
        if data.is_empty() {
            return;
        }
        if seq == self.rcv_nxt {
            self.deliver(data);
            return;
        }
        if self.out_of_order.len() >= MAX_OUT_OF_ORDER || self.out_of_order_bytes + data.len() > window {
            return;
        }
        // Kept sorted by start. Overlaps are harmless: `drain_in_order` trims them.
        let at = self
            .out_of_order
            .iter()
            .position(|(start, _)| seq_le(seq, *start))
            .unwrap_or(self.out_of_order.len());
        if let Some((start, existing)) = self.out_of_order.get(at) {
            if *start == seq && existing.len() >= data.len() {
                return;
            }
        }
        self.out_of_order_bytes += data.len();
        self.out_of_order.insert(at, (seq, data.to_vec()));
    }

    fn deliver(&mut self, data: &[u8]) {
        self.rcv_nxt = self.rcv_nxt.wrapping_add(data.len() as u32);
        if !self.orphaned {
            self.recv.extend(data);
        }
    }

    /// Moves out-of-order data that now fits, in order.
    fn drain_in_order(&mut self) {
        while let Some(start) = self.out_of_order.first().map(|entry| entry.0) {
            if seq_lt(self.rcv_nxt, start) {
                break;
            }
            let (_, data) = self.out_of_order.remove(0);
            self.out_of_order_bytes -= data.len();
            let skip = self.rcv_nxt.wrapping_sub(start) as usize;
            if skip < data.len() {
                self.deliver(&data[skip..]);
            }
        }
    }

    fn fail(&mut self, failure: Failure) {
        self.failure = Some(failure);
        self.state = State::Closed;
        self.send.clear();
        self.rto_deadline = None;
        self.persist_deadline = None;
    }

    fn flight(&self) -> u32 {
        self.snd_max.wrapping_sub(self.snd_una)
    }

    fn stop_retransmit_timer(&mut self) {
        self.rto_deadline = None;
        self.rto_ms = self.computed_rto();
    }

    fn computed_rto(&self) -> u64 {
        match self.srtt_ms {
            Some(srtt) => (srtt + (4 * self.rttvar_ms).max(10)).clamp(MIN_RTO_MS, MAX_RTO_MS),
            None => INITIAL_RTO_MS,
        }
    }

    /// RFC 6298's estimator, for a timed segment that's now acknowledged.
    fn sample_rtt(&mut self, ack: u32, now: u64) {
        let Some((end, sent)) = self.timing else {
            return;
        };
        if seq_lt(ack, end) {
            return;
        }
        self.timing = None;
        let sample = now.saturating_sub(sent);
        match self.srtt_ms {
            None => {
                self.srtt_ms = Some(sample);
                self.rttvar_ms = sample / 2;
            }
            Some(srtt) => {
                self.rttvar_ms = (3 * self.rttvar_ms + srtt.abs_diff(sample)) / 4;
                self.srtt_ms = Some((7 * srtt + sample) / 8);
            }
        }
        self.rto_ms = self.computed_rto();
    }

    /// Lets time pass: retransmission, persist and close timers.
    pub fn on_timer(&mut self, now: u64) {
        if let Some(deadline) = self.close_deadline {
            if now >= deadline {
                self.state = State::Closed;
                self.close_deadline = None;
                return;
            }
        }
        if let Some(deadline) = self.rto_deadline {
            if now >= deadline {
                self.on_retransmit_timeout(now);
                if self.is_closed() {
                    return;
                }
            }
        }
        if let Some(deadline) = self.persist_deadline {
            if now >= deadline {
                self.probe = true;
                self.persist_deadline = None;
                self.persist_ms = (self.persist_ms * 2).min(MAX_RTO_MS);
            }
        }
    }

    fn on_retransmit_timeout(&mut self, now: u64) {
        self.retries += 1;
        let limit = if matches!(self.state, State::SynSent | State::SynReceived) {
            MAX_SYN_RETRIES
        } else {
            MAX_RETRIES
        };
        if self.retries > limit {
            self.reset_pending = (self.state != State::SynSent).then_some(self.snd_nxt);
            self.fail(Failure::TimedOut);
            return;
        }
        // RFC 5681: back to slow start. Karn: nothing's measured from a repeat.
        self.ssthresh = (self.flight() / 2).max(2 * self.mss);
        self.cwnd = self.mss;
        self.dup_acks = 0;
        self.timing = None;
        self.rto_ms = (self.rto_ms * 2).min(MAX_RTO_MS);
        self.snd_nxt = self.snd_una;
        self.rto_deadline = Some(now + self.rto_ms);
        // A peer's advertisement of nothing can't be retried past without this.
        if self.snd_wnd == 0 {
            self.probe = true;
        }
    }

    /// The earliest time [`on_timer`](Self::on_timer) has anything to do.
    pub fn next_deadline(&self) -> Option<u64> {
        [self.rto_deadline, self.persist_deadline, self.close_deadline]
            .into_iter()
            .flatten()
            .min()
    }

    /// True when [`next_segment`](Self::next_segment) would produce something.
    pub fn wants_to_send(&self) -> bool {
        self.ack_pending
            || self.reset_pending.is_some()
            || self.fast_retransmit
            || self.probe
            || matches!(self.state, State::SynSent | State::SynReceived) && self.snd_nxt == self.iss
            || self.data_ready() != 0
            || self.fin_ready()
    }

    /// Unsent bytes in the send window.
    fn data_ready(&self) -> usize {
        if !self.is_connected() {
            return 0;
        }
        let window = self.snd_wnd.min(self.cwnd);
        let flight = self.snd_nxt.wrapping_sub(self.snd_una);
        let room = window.saturating_sub(flight);
        (room as usize).min(self.unsent())
    }

    fn data_end(&self) -> u32 {
        self.snd_una.wrapping_add(self.send.len() as u32)
    }

    fn unsent(&self) -> usize {
        // Negative past the end: the FIN has taken its sequence number.
        let unsent = self.data_end().wrapping_sub(self.snd_nxt) as i32;
        unsent.max(0) as usize
    }

    fn fin_ready(&self) -> bool {
        self.fin_queued && !self.fin_acked && self.is_connected() && self.snd_nxt == self.data_end()
    }

    /// Copies out `len` bytes starting at sequence number `seq`.
    fn copy_out(&self, seq: u32, out: &mut [u8]) {
        let from = seq.wrapping_sub(self.snd_una) as usize;
        for (slot, byte) in out.iter_mut().zip(self.send.iter().skip(from)) {
            *slot = *byte;
        }
    }

    /// Takes the next segment to send, with its data in `out`. `out` must have room
    /// for a full-sized one, [`LOCAL_MSS`] bytes.
    pub fn next_segment(&mut self, now: u64, out: &mut [u8]) -> Option<Outgoing> {
        if let Some(seq) = self.reset_pending.take() {
            return Some(Outgoing {
                seq,
                ack: self.rcv_nxt,
                flags: TCP_RST | TCP_ACK,
                window: 0,
                mss: None,
                len: 0,
            });
        }
        if self.state == State::Closed {
            return None;
        }
        // The SYN or SYN-ACK, for the first time or the next.
        if matches!(self.state, State::SynSent | State::SynReceived) && self.snd_nxt == self.iss {
            let again = self.snd_max != self.iss;
            self.snd_nxt = self.iss.wrapping_add(1);
            self.snd_max = self.snd_nxt;
            self.note_sent(now, self.snd_nxt, again);
            self.ack_pending = false;
            let (ack, flags) = if self.state == State::SynSent {
                (0, TCP_SYN)
            } else {
                (self.rcv_nxt, TCP_SYN | TCP_ACK)
            };
            return Some(Outgoing {
                seq: self.iss,
                ack,
                flags,
                window: self.window_to_advertise(),
                mss: Some(LOCAL_MSS),
                len: 0,
            });
        }
        if !self.is_connected() {
            return None;
        }
        if self.fast_retransmit {
            self.fast_retransmit = false;
            return self.retransmit_oldest(now, out);
        }
        // A zero-window probe: one byte past the end of the window.
        let window_shut = self.snd_wnd == 0 && self.snd_nxt == self.snd_una;
        if self.probe && window_shut && self.unsent() != 0 {
            self.probe = false;
            self.persist_deadline = Some(now + self.persist_ms);
            return self.send_data(now, 1, out);
        }
        self.probe = false;

        let ready = self.data_ready();
        if ready != 0 {
            return self.send_data(now, ready.min(self.mss as usize).min(out.len()), out);
        }
        if self.snd_wnd == 0 && self.unsent() != 0 && self.snd_nxt == self.snd_una && self.persist_deadline.is_none() {
            self.persist_deadline = Some(now + self.persist_ms);
        }

        if self.fin_ready() {
            let seq = self.snd_nxt;
            self.snd_nxt = self.snd_nxt.wrapping_add(1);
            let again = seq_lt(seq, self.snd_max);
            if !again {
                self.snd_max = self.snd_nxt;
            }
            self.note_sent(now, self.snd_nxt, again);
            self.ack_pending = false;
            return Some(Outgoing {
                seq,
                ack: self.rcv_nxt,
                flags: TCP_FIN | TCP_ACK,
                window: self.window_to_advertise(),
                mss: None,
                len: 0,
            });
        }

        if self.ack_pending {
            self.ack_pending = false;
            // From the top of what's been sent, even when going back over it: an ACK
            // with an old sequence number is outside the peer's window and goes unread.
            return Some(Outgoing {
                seq: self.snd_max,
                ack: self.rcv_nxt,
                flags: TCP_ACK,
                window: self.window_to_advertise(),
                mss: None,
                len: 0,
            });
        }
        None
    }

    fn note_sent(&mut self, now: u64, end: u32, again: bool) {
        if again {
            self.retransmits += 1;
            self.timing = None;
        } else if self.timing.is_none() {
            self.timing = Some((end, now));
        }
        if self.rto_deadline.is_none() {
            self.rto_deadline = Some(now + self.rto_ms);
        }
    }

    fn send_data(&mut self, now: u64, count: usize, out: &mut [u8]) -> Option<Outgoing> {
        let seq = self.snd_nxt;
        let count = count.min(out.len());
        self.copy_out(seq, &mut out[..count]);
        self.snd_nxt = self.snd_nxt.wrapping_add(count as u32);
        let again = seq_lt(seq, self.snd_max);
        if seq_lt(self.snd_max, self.snd_nxt) {
            self.snd_max = self.snd_nxt;
        }
        self.note_sent(now, self.snd_nxt, again);
        self.ack_pending = false;
        let last = self.snd_nxt == self.data_end();
        Some(Outgoing {
            seq,
            ack: self.rcv_nxt,
            flags: TCP_ACK | if last { TCP_PSH } else { 0 },
            window: self.window_to_advertise(),
            mss: None,
            len: count,
        })
    }

    /// Sends the oldest unacknowledged data again, leaving `snd_nxt` alone.
    fn retransmit_oldest(&mut self, now: u64, out: &mut [u8]) -> Option<Outgoing> {
        let count = self.send.len().min(self.mss as usize).min(out.len());
        self.retransmits += 1;
        self.timing = None;
        self.rto_deadline = Some(now + self.rto_ms);
        if count == 0 {
            // Only the FIN is outstanding.
            self.ack_pending = false;
            return Some(Outgoing {
                seq: self.snd_una,
                ack: self.rcv_nxt,
                flags: TCP_FIN | TCP_ACK,
                window: self.window_to_advertise(),
                mss: None,
                len: 0,
            });
        }
        self.copy_out(self.snd_una, &mut out[..count]);
        self.ack_pending = false;
        Some(Outgoing {
            seq: self.snd_una,
            ack: self.rcv_nxt,
            flags: TCP_ACK,
            window: self.window_to_advertise(),
            mss: None,
            len: count,
        })
    }
}

/// How much sequence space a segment takes.
fn segment_length(segment: &Incoming<'_>) -> u32 {
    let mut length = segment.payload.len() as u32;
    if segment.flags & TCP_SYN != 0 {
        length = length.wrapping_add(1);
    }
    if segment.flags & TCP_FIN != 0 {
        length = length.wrapping_add(1);
    }
    length
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Random(u64);

    impl Random {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn chance(&mut self, percent: u64) -> bool {
            self.next() % 100 < percent
        }
    }

    #[derive(Clone)]
    struct Packet {
        at: u64,
        to_a: bool,
        seq: u32,
        ack: u32,
        flags: u8,
        window: u16,
        mss: Option<u16>,
        data: Vec<u8>,
    }

    /// How badly the wire behaves.
    #[derive(Clone, Copy, Default)]
    struct Wire {
        /// Percent of packets lost.
        loss: u64,
        /// Percent of packets delayed past those behind them.
        reorder: u64,
        /// Percent of packets that arrive twice.
        duplicate: u64,
        /// Base one-way delay.
        delay_ms: u64,
    }

    /// Two ends of a connection and the wire between them.
    struct Net {
        a: Tcb,
        b: Tcb,
        wire: Wire,
        random: Random,
        in_flight: Vec<Packet>,
        now: u64,
        sent: usize,
    }

    impl Net {
        fn new(wire: Wire, seed: u64, iss_a: u32, iss_b: u32) -> (Self, Option<Incoming<'static>>) {
            let a = Tcb::connect(iss_a);
            (
                Self {
                    a,
                    b: Tcb::new(State::Closed, iss_b),
                    wire,
                    random: Random(seed | 1),
                    in_flight: Vec::new(),
                    now: 0,
                    sent: 0,
                },
                None,
            )
        }

        /// A connection, established with `b` accepting A's SYN, on a good wire.
        fn established(wire: Wire, seed: u64, iss_a: u32, iss_b: u32) -> Self {
            let (mut net, _) = Self::new(Wire::default(), seed, iss_a, iss_b);
            let mut out = [0u8; 2048];
            let syn = net.a.next_segment(0, &mut out).unwrap();
            assert_eq!(syn.flags, TCP_SYN);
            net.b = Tcb::accept(
                &Incoming {
                    seq: syn.seq,
                    ack: 0,
                    flags: syn.flags,
                    window: syn.window,
                    mss: syn.mss,
                    payload: &[],
                },
                iss_b,
            );
            net.wire = Wire { delay_ms: 1, ..Wire::default() };
            for _ in 0..50 {
                net.step();
            }
            assert_eq!(net.a.state(), State::Established);
            assert_eq!(net.b.state(), State::Established);
            net.wire = wire;
            net
        }

        fn drain(&mut self, to_a: bool) {
            let mut out = [0u8; 2048];
            loop {
                let (tcb, now) = if to_a { (&mut self.b, self.now) } else { (&mut self.a, self.now) };
                let Some(segment) = tcb.next_segment(now, &mut out) else {
                    return;
                };
                self.sent += 1;
                let packet = Packet {
                    at: 0,
                    to_a,
                    seq: segment.seq,
                    ack: segment.ack,
                    flags: segment.flags,
                    window: segment.window,
                    mss: segment.mss,
                    data: out[..segment.len].to_vec(),
                };
                self.launch(packet);
            }
        }

        fn launch(&mut self, mut packet: Packet) {
            if self.random.chance(self.wire.loss) {
                return;
            }
            let mut delay = self.wire.delay_ms;
            if self.random.chance(self.wire.reorder) {
                delay += self.random.next() % 40;
            }
            packet.at = self.now + delay;
            if self.random.chance(self.wire.duplicate) {
                let mut copy = packet.clone();
                copy.at += self.random.next() % 20;
                self.in_flight.push(copy);
            }
            self.in_flight.push(packet);
        }

        /// One millisecond of everything.
        fn step(&mut self) {
            self.now += 1;
            let now = self.now;
            let mut index = 0;
            while index < self.in_flight.len() {
                if self.in_flight[index].at > now {
                    index += 1;
                    continue;
                }
                let packet = self.in_flight.remove(index);
                let segment = Incoming {
                    seq: packet.seq,
                    ack: packet.ack,
                    flags: packet.flags,
                    window: packet.window,
                    mss: packet.mss,
                    payload: &packet.data,
                };
                if packet.to_a {
                    self.a.on_segment(&segment, now);
                    self.drain(false);
                } else {
                    self.b.on_segment(&segment, now);
                    self.drain(true);
                }
            }
            self.a.on_timer(now);
            self.b.on_timer(now);
            self.drain(false);
            self.drain(true);
        }
    }

    fn pattern(length: usize, salt: u32) -> Vec<u8> {
        (0..length as u32)
            .map(|index| (index.wrapping_mul(2_654_435_761).wrapping_add(salt) >> 11) as u8)
            .collect()
    }

    /// Sends `data` from A to B through the wire, reading in `chunk`-sized pieces, and
    /// closes both ends. Returns what B read.
    fn transfer(net: &mut Net, data: &[u8], chunk: usize) -> Vec<u8> {
        let mut sent = 0;
        let mut got = Vec::new();
        let mut buffer = vec![0u8; chunk];
        let mut shutdown = false;
        for _ in 0..20_000_000u64 {
            if sent < data.len() {
                sent += net.a.send(&data[sent..]);
            } else if !shutdown {
                net.a.shutdown();
                shutdown = true;
            }
            loop {
                let count = net.b.recv(&mut buffer);
                if count == 0 {
                    break;
                }
                got.extend_from_slice(&buffer[..count]);
            }
            if net.b.at_end() && net.b.state() == State::CloseWait {
                net.b.shutdown();
            }
            net.drain(false);
            net.drain(true);
            net.step();
            if net.a.state() == State::TimeWait && net.b.is_closed() {
                return got;
            }
            assert!(!net.a.is_closed(), "A gave up: {:?}", net.a.failure());
            assert_eq!(net.b.failure(), None);
        }
        panic!(
            "no finish: got {} of {}, A {:?}, B {:?}",
            got.len(),
            data.len(),
            net.a.state(),
            net.b.state()
        );
    }

    fn exchange_checked(wire: Wire, seed: u64, size: usize) {
        let data = pattern(size, seed as u32);
        let mut net = Net::established(wire, seed, 1000, 500_000);
        let got = transfer(&mut net, &data, 4096);
        assert!(got == data, "{} bytes differ (seed {seed})", got.len());
        // Time-wait runs out by itself.
        for _ in 0..TIME_WAIT_MS + 100 {
            net.step();
        }
        assert!(net.a.is_closed(), "{:?} {:?} {}", net.a.state(), net.a.close_deadline, net.now);
    }

    #[test]
    fn a_clean_transfer_needs_no_retransmission() {
        let data = pattern(1 << 20, 1);
        let mut net = Net::established(Wire { delay_ms: 5, ..Wire::default() }, 3, 1000, 500_000);
        assert_eq!(transfer(&mut net, &data, 8192), data);
        assert_eq!(net.a.retransmits, 0);
    }

    #[test]
    fn data_survives_loss() {
        for (seed, loss) in [(11, 1), (12, 5), (13, 10), (14, 25)] {
            let wire = Wire { loss, delay_ms: 5, ..Wire::default() };
            exchange_checked(wire, seed, 200_000);
        }
    }

    #[test]
    fn data_survives_reordering() {
        for seed in 20..24 {
            let wire = Wire { reorder: 30, delay_ms: 2, ..Wire::default() };
            exchange_checked(wire, seed, 500_000);
        }
    }

    #[test]
    fn data_survives_duplicates() {
        let wire = Wire { duplicate: 30, delay_ms: 2, ..Wire::default() };
        exchange_checked(wire, 31, 500_000);
    }

    #[test]
    fn data_survives_all_of_it_at_once() {
        for seed in 40..50 {
            let wire = Wire { loss: 10, reorder: 20, duplicate: 10, delay_ms: 3 };
            exchange_checked(wire, seed, 100_000);
        }
    }

    #[test]
    fn sequence_numbers_wrap_around() {
        let data = pattern(300_000, 9);
        let wire = Wire { loss: 3, reorder: 10, delay_ms: 2, ..Wire::default() };
        let mut net = Net::established(wire, 5, u32::MAX - 2000, u32::MAX - 100);
        assert_eq!(transfer(&mut net, &data, 4096), data);
    }

    #[test]
    fn data_flows_both_ways_at_once() {
        let there = pattern(100_000, 1);
        let back = pattern(150_000, 2);
        let wire = Wire { loss: 5, reorder: 10, delay_ms: 2, ..Wire::default() };
        let mut net = Net::established(wire, 77, 1, 2);
        let (mut a_sent, mut b_sent) = (0, 0);
        let (mut a_got, mut b_got) = (Vec::new(), Vec::new());
        let mut buffer = [0u8; 4096];
        for _ in 0..30_000_000u64 {
            a_sent += net.a.send(&there[a_sent..]);
            b_sent += net.b.send(&back[b_sent..]);
            let count = net.a.recv(&mut buffer);
            a_got.extend_from_slice(&buffer[..count]);
            let count = net.b.recv(&mut buffer);
            b_got.extend_from_slice(&buffer[..count]);
            net.step();
            if a_got.len() == back.len() && b_got.len() == there.len() {
                break;
            }
        }
        assert!(a_got == back && b_got == there);
    }

    #[test]
    fn a_slow_reader_shuts_the_window_and_it_reopens() {
        // A reader that stops is a window of zero, which the sender must keep probing.
        let data = pattern(200_000, 4);
        let mut net = Net::established(Wire { delay_ms: 1, ..Wire::default() }, 8, 1, 2);
        let mut sent = net.a.send(&data);
        for _ in 0..60_000 {
            net.step();
            sent += net.a.send(&data[sent..]);
        }
        assert_eq!(net.b.readable(), RECV_BUFFER - RECV_BUFFER % 1460 + RECV_BUFFER % 1460);
        assert!(net.a.is_connected(), "a shut window isn't a lost connection");
        assert!(net.a.persist_deadline.is_some() || net.a.rto_deadline.is_some());
        let mut got = Vec::new();
        let mut buffer = [0u8; 700];
        for _ in 0..5_000_000 {
            let count = net.b.recv(&mut buffer);
            got.extend_from_slice(&buffer[..count]);
            sent += net.a.send(&data[sent..]);
            net.step();
            if got.len() == data.len() {
                break;
            }
        }
        assert!(got == data, "read {} of {}", got.len(), data.len());
    }

    #[test]
    fn a_lost_window_update_is_recovered_by_a_probe() {
        let data = pattern(100_000, 4);
        let mut net = Net::established(Wire { delay_ms: 1, ..Wire::default() }, 8, 1, 2);
        let mut sent = net.a.send(&data);
        for _ in 0..3000 {
            net.step();
            sent += net.a.send(&data[sent..]);
        }
        // The update announcing the room vanishes.
        let mut buffer = vec![0u8; RECV_BUFFER];
        let read = net.b.recv(&mut buffer);
        assert!(read > 0);
        let mut out = [0u8; 2048];
        while net.b.next_segment(net.now, &mut out).is_some() {}
        let mut got = buffer[..read].to_vec();
        let mut small = [0u8; 4096];
        for _ in 0..200_000 {
            sent += net.a.send(&data[sent..]);
            net.step();
            let count = net.b.recv(&mut small);
            got.extend_from_slice(&small[..count]);
            if got.len() == data.len() {
                break;
            }
        }
        assert!(got == data);
    }

    #[test]
    fn a_silent_peer_times_out_with_a_reason() {
        let mut net = Net::established(Wire { loss: 100, ..Wire::default() }, 1, 1, 2);
        net.a.send(b"into the void");
        for _ in 0..600_000 {
            net.step();
            if net.a.is_closed() {
                break;
            }
        }
        assert_eq!(net.a.failure(), Some(Failure::TimedOut));
    }

    #[test]
    fn an_unanswered_syn_is_sent_several_times_and_times_out() {
        let mut tcb = Tcb::connect(100);
        let mut out = [0u8; 16];
        let mut sends = 0;
        let mut now = 0;
        while !tcb.is_closed() && now < 600_000 {
            while let Some(segment) = tcb.next_segment(now, &mut out) {
                if segment.flags == TCP_SYN {
                    assert_eq!(segment.seq, 100);
                    sends += 1;
                }
            }
            now += 10;
            tcb.on_timer(now);
        }
        assert_eq!(sends, MAX_SYN_RETRIES + 1);
        assert_eq!(tcb.failure(), Some(Failure::TimedOut));
    }

    fn incoming(seq: u32, ack: u32, flags: u8) -> Incoming<'static> {
        Incoming {
            seq,
            ack,
            flags,
            window: 4096,
            mss: None,
            payload: &[],
        }
    }

    #[test]
    fn a_reset_answering_a_syn_is_a_refusal() {
        let mut tcb = Tcb::connect(100);
        let mut out = [0u8; 16];
        tcb.next_segment(0, &mut out).unwrap();
        // Without the ACK of the SYN, it's not about this connection.
        tcb.on_segment(&incoming(0, 0, TCP_RST), 1);
        assert_eq!(tcb.state(), State::SynSent);
        tcb.on_segment(&incoming(0, 999, TCP_RST | TCP_ACK), 1);
        assert_eq!(tcb.state(), State::SynSent);
        tcb.on_segment(&incoming(0, 101, TCP_RST | TCP_ACK), 1);
        assert_eq!(tcb.failure(), Some(Failure::Refused));
    }

    #[test]
    fn a_reset_at_the_right_place_ends_it_and_elsewhere_gets_a_challenge() {
        let mut net = Net::established(Wire::default(), 1, 1000, 5000);
        let next = net.a.rcv_nxt;
        net.a.on_segment(&incoming(next + 10, 0, TCP_RST), 1);
        assert!(net.a.is_connected());
        let mut out = [0u8; 16];
        let challenge = net.a.next_segment(1, &mut out).unwrap();
        assert_eq!((challenge.flags, challenge.ack), (TCP_ACK, next));
        net.a.on_segment(&incoming(next.wrapping_sub(40_000), 0, TCP_RST), 1);
        assert!(net.a.is_connected(), "outside the window");
        net.a.on_segment(&incoming(next, 0, TCP_RST), 1);
        assert_eq!(net.a.failure(), Some(Failure::Reset));
        assert_eq!(net.a.send(b"x"), 0);
    }

    #[test]
    fn a_syn_on_a_live_connection_gets_a_challenge() {
        let mut net = Net::established(Wire::default(), 1, 1000, 5000);
        let next = net.a.rcv_nxt;
        net.a.on_segment(&incoming(next, 0, TCP_SYN), 1);
        assert!(net.a.is_connected());
        let mut out = [0u8; 16];
        assert_eq!(net.a.next_segment(1, &mut out).unwrap().flags, TCP_ACK);
    }

    #[test]
    fn an_ack_for_data_never_sent_changes_nothing() {
        let mut net = Net::established(Wire::default(), 1, 1000, 5000);
        net.a.send(b"hello");
        let una = net.a.snd_una;
        let next = net.a.rcv_nxt;
        net.a.on_segment(&incoming(next, una.wrapping_add(1000), TCP_ACK), 1);
        assert_eq!(net.a.snd_una, una);
        assert_eq!(net.a.unacknowledged(), 5);
    }

    #[test]
    fn a_duplicate_syn_gets_the_syn_ack_again() {
        let syn = incoming(700, 0, TCP_SYN);
        let mut tcb = Tcb::accept(&syn, 9000);
        let mut out = [0u8; 16];
        assert_eq!(tcb.next_segment(0, &mut out).unwrap().flags, TCP_SYN | TCP_ACK);
        assert!(tcb.next_segment(0, &mut out).is_none());
        tcb.on_segment(&syn, 1);
        let again = tcb.next_segment(1, &mut out).unwrap();
        assert_eq!((again.flags, again.seq, again.ack), (TCP_SYN | TCP_ACK, 9000, 701));
    }

    #[test]
    fn both_ends_closing_at_once_goes_through_closing() {
        let mut net = Net::established(Wire { delay_ms: 5, ..Wire::default() }, 1, 1000, 5000);
        net.a.shutdown();
        net.b.shutdown();
        assert_eq!((net.a.state(), net.b.state()), (State::FinWait1, State::FinWait1));
        net.drain(false);
        net.drain(true);
        let mut saw_closing = false;
        for _ in 0..100 {
            net.step();
            saw_closing |= net.a.state() == State::Closing;
        }
        assert!(saw_closing);
        assert_eq!((net.a.state(), net.b.state()), (State::TimeWait, State::TimeWait));
    }

    #[test]
    fn a_half_closed_connection_keeps_receiving() {
        let mut net = Net::established(Wire { delay_ms: 1, ..Wire::default() }, 1, 1000, 5000);
        net.a.shutdown();
        for _ in 0..20 {
            net.step();
        }
        assert_eq!((net.a.state(), net.b.state()), (State::FinWait2, State::CloseWait));
        assert_eq!(net.a.send(b"x"), 0, "A said it had finished");
        assert_eq!(net.b.send(b"still here"), 10);
        for _ in 0..20 {
            net.step();
        }
        let mut out = [0u8; 32];
        let count = net.a.recv(&mut out);
        assert_eq!(&out[..count], b"still here");
        assert!(!net.a.at_end());
        net.b.shutdown();
        for _ in 0..20 {
            net.step();
        }
        assert!(net.a.at_end());
        assert_eq!((net.a.state(), net.b.state()), (State::TimeWait, State::Closed));
    }

    #[test]
    fn a_lost_fin_is_sent_again() {
        let mut net = Net::established(Wire { loss: 100, delay_ms: 1, ..Wire::default() }, 1, 1000, 5000);
        net.a.shutdown();
        net.drain(false);
        for _ in 0..1500 {
            net.step();
        }
        net.wire = Wire { delay_ms: 1, ..Wire::default() };
        for _ in 0..2500 {
            net.step();
        }
        assert_eq!((net.a.state(), net.b.state()), (State::FinWait2, State::CloseWait));
        assert!(net.a.retransmits > 0);
    }

    #[test]
    fn a_fin_ahead_of_missing_data_waits_for_the_data() {
        let mut net = Net::established(Wire::default(), 1, 1000, 5000);
        let next = net.a.rcv_nxt;
        let una = net.a.snd_una;
        net.a.on_segment(
            &Incoming {
                seq: next + 3,
                ack: una,
                flags: TCP_ACK | TCP_FIN,
                window: 4096,
                mss: None,
                payload: b"def",
            },
            1,
        );
        assert_eq!(net.a.state(), State::Established);
        assert_eq!(net.a.readable(), 0);
        net.a.on_segment(
            &Incoming {
                seq: next,
                ack: una,
                flags: TCP_ACK,
                window: 4096,
                mss: None,
                payload: b"abc",
            },
            1,
        );
        assert_eq!(net.a.state(), State::CloseWait);
        let mut out = [0u8; 16];
        let count = net.a.recv(&mut out);
        assert_eq!(&out[..count], b"abcdef");
        assert!(net.a.at_end());
    }

    #[test]
    fn overlapping_retransmissions_deliver_each_byte_once() {
        let mut net = Net::established(Wire::default(), 1, 1000, 5000);
        let next = net.a.rcv_nxt;
        let una = net.a.snd_una;
        let segment = |seq: u32, payload: &'static [u8]| Incoming {
            seq,
            ack: una,
            flags: TCP_ACK,
            window: 4096,
            mss: None,
            payload,
        };
        net.a.on_segment(&segment(next + 4, b"efgh"), 1);
        net.a.on_segment(&segment(next + 2, b"cdefgh"), 1);
        net.a.on_segment(&segment(next + 8, b"ij"), 1);
        net.a.on_segment(&segment(next, b"abcd"), 1);
        net.a.on_segment(&segment(next, b"abcdefghij"), 1);
        let mut out = [0u8; 32];
        let count = net.a.recv(&mut out);
        assert_eq!(&out[..count], b"abcdefghij");
    }

    #[test]
    fn data_past_the_window_is_cut_and_memory_stays_bounded() {
        let mut net = Net::established(Wire::default(), 1, 1000, 5000);
        let next = net.a.rcv_nxt;
        let una = net.a.snd_una;
        let big = vec![7u8; 1460];
        // Far past the window, and a flood of little gaps.
        for index in 0..100_000u32 {
            net.a.on_segment(
                &Incoming {
                    seq: next.wrapping_add(index.wrapping_mul(1500).wrapping_add(10)),
                    ack: una,
                    flags: TCP_ACK,
                    window: 4096,
                    mss: None,
                    payload: &big,
                },
                1,
            );
        }
        assert!(net.a.out_of_order.len() <= MAX_OUT_OF_ORDER);
        assert!(net.a.out_of_order_bytes <= RECV_BUFFER);
        assert_eq!(net.a.readable(), 0);
    }

    #[test]
    fn the_peer_mss_is_honoured_and_clamped() {
        let mut syn = incoming(1, 0, TCP_SYN);
        syn.mss = Some(1);
        assert_eq!(Tcb::accept(&syn, 1).mss, u32::from(MIN_MSS));
        syn.mss = Some(u16::MAX);
        assert_eq!(Tcb::accept(&syn, 1).mss, u32::from(LOCAL_MSS));
        syn.mss = None;
        assert_eq!(Tcb::accept(&syn, 1).mss, u32::from(DEFAULT_MSS));
    }

    #[test]
    fn the_timer_estimate_follows_the_measurement() {
        let mut net = Net::established(Wire { delay_ms: 40, ..Wire::default() }, 1, 1000, 5000);
        let data = pattern(1 << 21, 3);
        let got = transfer(&mut net, &data, 4096);
        assert_eq!(got.len(), data.len());
        let srtt = net.a.srtt_ms.unwrap();
        assert!((60..=120).contains(&srtt), "smoothed {srtt} ms for a real 80");
        assert!(net.a.computed_rto() >= MIN_RTO_MS);
    }

    #[test]
    fn a_reset_answers_what_has_no_connection() {
        let reset = reset_for(&incoming(5, 77, TCP_ACK)).unwrap();
        assert_eq!((reset.seq, reset.flags), (77, TCP_RST));
        let reset = reset_for(&incoming(5, 0, TCP_SYN)).unwrap();
        assert_eq!((reset.seq, reset.ack, reset.flags), (0, 6, TCP_RST | TCP_ACK));
        assert!(reset_for(&incoming(5, 0, TCP_RST)).is_none());
    }

    /// Anything at all in any state. Nothing may panic or overflow a buffer.
    #[test]
    fn random_segments_never_panic_in_any_state() {
        let mut random = Random(0x1234_5678);
        let payloads: [&[u8]; 3] = [&[], &[1; 100], &[2; 1460]];
        for round in 0..20_000u64 {
            let mut tcb = match round % 3 {
                0 => Tcb::connect(random.next() as u32),
                1 => Tcb::accept(&incoming(random.next() as u32, 0, TCP_SYN), random.next() as u32),
                _ => Net::established(Wire::default(), round | 1, random.next() as u32, random.next() as u32).a,
            };
            tcb.send(&[9; 3000]);
            let mut out = [0u8; 2048];
            let mut now = 0;
            for _ in 0..12 {
                let near = random.chance(70);
                let base = tcb.rcv_nxt;
                let segment = Incoming {
                    seq: if near { base.wrapping_add(random.next() as u32 % 2000) } else { random.next() as u32 },
                    ack: if near { tcb.snd_una.wrapping_add(random.next() as u32 % 3000) } else { random.next() as u32 },
                    flags: random.next() as u8,
                    window: random.next() as u16,
                    mss: if random.chance(50) { Some(random.next() as u16) } else { None },
                    payload: payloads[random.next() as usize % 3],
                };
                tcb.on_segment(&segment, now);
                now += random.next() % 2000;
                tcb.on_timer(now);
                for _ in 0..8 {
                    if tcb.next_segment(now, &mut out).is_none() {
                        break;
                    }
                }
                let mut sink = [0u8; 512];
                tcb.recv(&mut sink);
            }
        }
    }

    #[test]
    fn scheduled_timers_are_reported() {
        let mut tcb = Tcb::connect(1);
        assert_eq!(tcb.next_deadline(), None);
        let mut out = [0u8; 8];
        tcb.next_segment(100, &mut out).unwrap();
        assert_eq!(tcb.next_deadline(), Some(100 + INITIAL_RTO_MS));
    }
}
