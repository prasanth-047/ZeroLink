use std::time::{Duration, Instant};

use crate::error::{Result, ZeroLinkError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadioDomain {
    BleL2capOnly,
    DualRadioActive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkReason {
    FileDownload,
    PhotoSync,
    ScreenCast,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Disconnected,
    BleConnecting,
    BleConnectedIdle {
        last_signaling_activity: Instant,
    },
    BleSignaling {
        packet_type: u8,
        started_at: Instant,
    },
    WifiWaking {
        reason: BulkReason,
        requested_at: Instant,
    },
    WifiActiveTransfer {
        reason: BulkReason,
        target_ip: String,
        port: u16,
        last_chunk_at: Instant,
    },
    WifiTearingDown {
        finished_at: Instant,
    },
}

#[derive(Debug, Clone)]
pub enum Event {
    BleConnected,
    BleDisconnected,
    HandshakeCompleted,
    SignalingStart { packet_type: u8 },
    SignalingEnd,
    RequestBulkTransfer { reason: BulkReason },
    WifiWakeAcknowledged { target_ip: String, port: u16 },
    BulkChunkTransferred,
    BulkTransferFinished,
    WifiTimeout,
}

pub struct DualRadioStateMachine {
    current_state: State,
    wifi_inactivity_timeout: Duration,
}

impl DualRadioStateMachine {
    pub fn new(wifi_inactivity_timeout: Duration) -> Self {
        Self {
            current_state: State::Disconnected,
            wifi_inactivity_timeout,
        }
    }

    pub fn current_state(&self) -> &State {
        &self.current_state
    }

    pub fn radio_domain(&self) -> RadioDomain {
        match &self.current_state {
            State::WifiWaking { .. }
            | State::WifiActiveTransfer { .. }
            | State::WifiTearingDown { .. } => RadioDomain::DualRadioActive,
            _ => RadioDomain::BleL2capOnly,
        }
    }

    /// Processes an event and executes valid state transitions according to power budget rules.
    pub fn handle_event(&mut self, event: Event) -> Result<()> {
        let now = Instant::now();
        let new_state = match (&self.current_state, event) {
            (State::Disconnected, Event::BleConnected) => State::BleConnecting,

            (State::BleConnecting, Event::HandshakeCompleted) => State::BleConnectedIdle {
                last_signaling_activity: now,
            },

            (State::BleConnectedIdle { .. }, Event::SignalingStart { packet_type }) => {
                State::BleSignaling {
                    packet_type,
                    started_at: now,
                }
            }

            (State::BleSignaling { .. }, Event::SignalingEnd) => State::BleConnectedIdle {
                last_signaling_activity: now,
            },

            (State::BleConnectedIdle { .. }, Event::RequestBulkTransfer { reason }) => {
                State::WifiWaking {
                    reason,
                    requested_at: now,
                }
            }

            (
                State::WifiWaking { reason, .. },
                Event::WifiWakeAcknowledged { target_ip, port },
            ) => State::WifiActiveTransfer {
                reason: *reason,
                target_ip,
                port,
                last_chunk_at: now,
            },

            (
                State::WifiActiveTransfer {
                    reason,
                    target_ip,
                    port,
                    ..
                },
                Event::BulkChunkTransferred,
            ) => State::WifiActiveTransfer {
                reason: *reason,
                target_ip: target_ip.clone(),
                port: *port,
                last_chunk_at: now,
            },

            (State::WifiActiveTransfer { .. }, Event::BulkTransferFinished) => {
                State::WifiTearingDown { finished_at: now }
            }

            (State::WifiActiveTransfer { .. }, Event::WifiTimeout)
            | (State::WifiWaking { .. }, Event::WifiTimeout) => {
                State::WifiTearingDown { finished_at: now }
            }

            (State::WifiTearingDown { .. }, Event::SignalingEnd)
            | (State::WifiTearingDown { .. }, Event::BulkTransferFinished) => {
                State::BleConnectedIdle {
                    last_signaling_activity: now,
                }
            }

            // Universal disconnect handler
            (_, Event::BleDisconnected) => State::Disconnected,

            (from, invalid_event) => {
                return Err(ZeroLinkError::InvalidStateTransition {
                    from: format!("{:?}", from),
                    to: format!("{:?}", invalid_event),
                });
            }
        };

        self.current_state = new_state;
        Ok(())
    }

    /// Evaluates whether active Wi-Fi or pending Wi-Fi wake has exceeded maximum allowable inactivity time.
    pub fn check_timeouts(&mut self) -> Result<bool> {
        let now = Instant::now();
        match &self.current_state {
            State::WifiActiveTransfer { last_chunk_at, .. } => {
                if now.duration_since(*last_chunk_at) > self.wifi_inactivity_timeout {
                    self.handle_event(Event::WifiTimeout)?;
                    return Ok(true);
                }
            }
            State::WifiWaking { requested_at, .. } => {
                if now.duration_since(*requested_at) > self.wifi_inactivity_timeout {
                    self.handle_event(Event::WifiTimeout)?;
                    return Ok(true);
                }
            }
            _ => {}
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_machine_power_lifecycle() {
        let mut sm = DualRadioStateMachine::new(Duration::from_secs(5));
        assert_eq!(sm.radio_domain(), RadioDomain::BleL2capOnly);

        sm.handle_event(Event::BleConnected).unwrap();
        sm.handle_event(Event::HandshakeCompleted).unwrap();
        assert_eq!(sm.radio_domain(), RadioDomain::BleL2capOnly);

        // Wake Wi-Fi for photo sync
        sm.handle_event(Event::RequestBulkTransfer {
            reason: BulkReason::PhotoSync,
        })
        .unwrap();
        assert_eq!(sm.radio_domain(), RadioDomain::DualRadioActive);

        sm.handle_event(Event::WifiWakeAcknowledged {
            target_ip: "192.168.1.50".to_string(),
            port: 8443,
        })
        .unwrap();
        assert_eq!(sm.radio_domain(), RadioDomain::DualRadioActive);

        // Bulk transfer completes
        sm.handle_event(Event::BulkTransferFinished).unwrap();
        sm.handle_event(Event::SignalingEnd).unwrap();

        // Radio immediately drops back to low-power BLE domain
        assert_eq!(sm.radio_domain(), RadioDomain::BleL2capOnly);
    }
}
