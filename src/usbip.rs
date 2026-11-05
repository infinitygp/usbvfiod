//! The framing of USB/IP, as `usbvfiod` speaks it to an external device.
//!
//! The headers are those of `CMD_SUBMIT`, `RET_SUBMIT`, `CMD_UNLINK` and `RET_UNLINK` in the Linux
//! kernel's `Documentation/usb/usbip_protocol.rst`: 48 bytes, all fields big-endian. There is no
//! `OP_REQ_DEVLIST` or `OP_REQ_IMPORT` phase, because the identity and the speed of an external
//! device come from the hotplug command. `docs/users/external-devices.md` describes how the
//! controller uses the messages. This module only holds the bytes, so that the controller, the
//! tests and a device server can share them.

use std::fmt;

use thiserror::Error;

/// The length of every header.
pub const HEADER_LEN: usize = 48;

/// The status of a transfer that the device stalled (`EPIPE`).
pub const STATUS_STALL: i32 = -32;
/// The status of a transfer that failed on the bus (`EPROTO`).
pub const STATUS_TRANSACTION_ERROR: i32 = -71;
/// The status of a transfer that was cancelled (`ECONNRESET`).
pub const STATUS_CANCELLED: i32 = -104;

const COMMAND_SUBMIT: u32 = 0x0000_0001;
const COMMAND_UNLINK: u32 = 0x0000_0002;
const REPLY_SUBMIT: u32 = 0x0000_0003;
const REPLY_UNLINK: u32 = 0x0000_0004;

/// The direction of a transfer, seen from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
}

impl Direction {
    const fn to_wire(self) -> u32 {
        match self {
            Self::Out => 0,
            Self::In => 1,
        }
    }
}

/// `CMD_SUBMIT`: the host asks the device for a transfer. For an OUT transfer,
/// `transfer_buffer_length` bytes of data follow the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmdSubmit {
    pub seqnum: u32,
    pub direction: Direction,
    /// The endpoint number, 0 to 15, without the direction bit.
    pub ep: u8,
    pub transfer_buffer_length: u32,
    /// The setup packet of a control transfer on endpoint 0, zeros otherwise.
    pub setup: [u8; 8],
}

/// `RET_SUBMIT`: the device completes a transfer. For an IN transfer, `actual_length` bytes of
/// data follow the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetSubmit {
    pub seqnum: u32,
    /// 0 for success, a negative errno otherwise, see [`STATUS_STALL`].
    pub status: i32,
    pub actual_length: i32,
}

/// `CMD_UNLINK`: the host cancels the `CMD_SUBMIT` with the sequence number `unlink_seqnum`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmdUnlink {
    pub seqnum: u32,
    pub unlink_seqnum: u32,
}

/// `RET_UNLINK`: the device answers a `CMD_UNLINK`, with [`STATUS_CANCELLED`] if it cancelled the
/// transfer, which then gets no `RET_SUBMIT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetUnlink {
    pub seqnum: u32,
    pub status: i32,
}

/// The header of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Header {
    CmdSubmit(CmdSubmit),
    RetSubmit(RetSubmit),
    CmdUnlink(CmdUnlink),
    RetUnlink(RetUnlink),
}

/// A header that cannot be understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DecodeError {
    #[error("{0:#010x} is not the command of a header")]
    UnknownCommand(u32),
    #[error("{0} is not a direction")]
    UnknownDirection(u32),
    #[error("{0} is not an endpoint number")]
    UnknownEndpoint(u32),
    #[error("the transfer announces {0} isochronous packets, which are not supported")]
    IsochronousPackets(u32),
}

/// The `number_of_packets` of a transfer that is not isochronous: 0, or all ones like Linux does.
const fn is_not_isochronous(number_of_packets: u32) -> bool {
    number_of_packets == 0 || number_of_packets == u32::MAX
}

fn put(header: &mut [u8; HEADER_LEN], offset: usize, value: u32) {
    header[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

const fn get(header: &[u8; HEADER_LEN], offset: usize) -> u32 {
    u32::from_be_bytes([
        header[offset],
        header[offset + 1],
        header[offset + 2],
        header[offset + 3],
    ])
}

impl Header {
    /// The 48 bytes of the header.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut header = [0u8; HEADER_LEN];
        match self {
            Self::CmdSubmit(submit) => {
                put(&mut header, 0, COMMAND_SUBMIT);
                put(&mut header, 4, submit.seqnum);
                // devid stays 0.
                put(&mut header, 12, submit.direction.to_wire());
                put(&mut header, 16, u32::from(submit.ep));
                // transfer_flags, start_frame, number_of_packets and interval stay 0.
                put(&mut header, 24, submit.transfer_buffer_length);
                header[40..].copy_from_slice(&submit.setup);
            }
            Self::RetSubmit(ret) => {
                put(&mut header, 0, REPLY_SUBMIT);
                put(&mut header, 4, ret.seqnum);
                put(&mut header, 20, ret.status.cast_unsigned());
                put(&mut header, 24, ret.actual_length.cast_unsigned());
            }
            Self::CmdUnlink(unlink) => {
                put(&mut header, 0, COMMAND_UNLINK);
                put(&mut header, 4, unlink.seqnum);
                put(&mut header, 20, unlink.unlink_seqnum);
            }
            Self::RetUnlink(ret) => {
                put(&mut header, 0, REPLY_UNLINK);
                put(&mut header, 4, ret.seqnum);
                put(&mut header, 20, ret.status.cast_unsigned());
            }
        }
        header
    }

    /// Read a header. The fields that this protocol does not use are not checked, except that a
    /// transfer must not be isochronous: packet descriptors would follow it.
    pub fn decode(header: &[u8; HEADER_LEN]) -> Result<Self, DecodeError> {
        let seqnum = get(header, 4);
        match get(header, 0) {
            COMMAND_SUBMIT => {
                let direction = match get(header, 12) {
                    0 => Direction::Out,
                    1 => Direction::In,
                    other => return Err(DecodeError::UnknownDirection(other)),
                };
                let ep = get(header, 16);
                let ep = u8::try_from(ep)
                    .ok()
                    .filter(|ep| *ep < 16)
                    .ok_or(DecodeError::UnknownEndpoint(ep))?;
                if !is_not_isochronous(get(header, 32)) {
                    return Err(DecodeError::IsochronousPackets(get(header, 32)));
                }
                let mut setup = [0u8; 8];
                setup.copy_from_slice(&header[40..]);
                Ok(Self::CmdSubmit(CmdSubmit {
                    seqnum,
                    direction,
                    ep,
                    transfer_buffer_length: get(header, 24),
                    setup,
                }))
            }
            REPLY_SUBMIT => {
                if !is_not_isochronous(get(header, 32)) {
                    return Err(DecodeError::IsochronousPackets(get(header, 32)));
                }
                Ok(Self::RetSubmit(RetSubmit {
                    seqnum,
                    status: get(header, 20).cast_signed(),
                    actual_length: get(header, 24).cast_signed(),
                }))
            }
            COMMAND_UNLINK => Ok(Self::CmdUnlink(CmdUnlink {
                seqnum,
                unlink_seqnum: get(header, 20),
            })),
            REPLY_UNLINK => Ok(Self::RetUnlink(RetUnlink {
                seqnum,
                status: get(header, 20).cast_signed(),
            })),
            other => Err(DecodeError::UnknownCommand(other)),
        }
    }
}

impl fmt::Display for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CmdSubmit(submit) => write!(
                f,
                "CMD_SUBMIT {} ep {} {:?} {} bytes",
                submit.seqnum, submit.ep, submit.direction, submit.transfer_buffer_length
            ),
            Self::RetSubmit(ret) => write!(
                f,
                "RET_SUBMIT {} status {} {} bytes",
                ret.seqnum, ret.status, ret.actual_length
            ),
            Self::CmdUnlink(unlink) => write!(
                f,
                "CMD_UNLINK {} of {}",
                unlink.seqnum, unlink.unlink_seqnum
            ),
            Self::RetUnlink(ret) => {
                write!(f, "RET_UNLINK {} status {}", ret.seqnum, ret.status)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cmd_submit_has_the_documented_layout() {
        let header = Header::CmdSubmit(CmdSubmit {
            seqnum: 0x0102_0304,
            direction: Direction::In,
            ep: 3,
            transfer_buffer_length: 0x40,
            setup: [0x80, 6, 0, 1, 0, 0, 0x12, 0],
        })
        .encode();

        assert_eq!(
            header,
            [
                0, 0, 0, 1, // command
                1, 2, 3, 4, // seqnum
                0, 0, 0, 0, // devid
                0, 0, 0, 1, // direction
                0, 0, 0, 3, // ep
                0, 0, 0, 0, // transfer_flags
                0, 0, 0, 0x40, // transfer_buffer_length
                0, 0, 0, 0, // start_frame
                0, 0, 0, 0, // number_of_packets
                0, 0, 0, 0, // interval
                0x80, 6, 0, 1, 0, 0, 0x12, 0, // setup
            ]
        );
    }

    #[test]
    fn a_ret_submit_has_the_documented_layout() {
        let header = Header::RetSubmit(RetSubmit {
            seqnum: 7,
            status: STATUS_STALL,
            actual_length: 18,
        })
        .encode();

        assert_eq!(header[..8], [0, 0, 0, 3, 0, 0, 0, 7]);
        assert_eq!(header[20..24], [0xff, 0xff, 0xff, 0xe0], "status -32");
        assert_eq!(header[24..28], [0, 0, 0, 18]);
        assert!(header[8..20].iter().chain(&header[28..]).all(|b| *b == 0));
    }

    #[test]
    fn unlink_messages_have_the_documented_layout() {
        let unlink = Header::CmdUnlink(CmdUnlink {
            seqnum: 9,
            unlink_seqnum: 4,
        })
        .encode();
        assert_eq!(unlink[..8], [0, 0, 0, 2, 0, 0, 0, 9]);
        assert_eq!(unlink[20..24], [0, 0, 0, 4]);
        assert!(unlink[24..].iter().all(|b| *b == 0), "24 bytes of padding");

        let ret = Header::RetUnlink(RetUnlink {
            seqnum: 9,
            status: STATUS_CANCELLED,
        })
        .encode();
        assert_eq!(ret[..8], [0, 0, 0, 4, 0, 0, 0, 9]);
        assert_eq!(ret[20..24], [0xff, 0xff, 0xff, 0x98], "status -104");
        assert!(ret[24..].iter().all(|b| *b == 0));
    }

    #[test]
    fn every_header_decodes_to_what_was_encoded() {
        for header in [
            Header::CmdSubmit(CmdSubmit {
                seqnum: u32::MAX,
                direction: Direction::Out,
                ep: 15,
                transfer_buffer_length: 65_536,
                setup: [0; 8],
            }),
            Header::RetSubmit(RetSubmit {
                seqnum: 1,
                status: STATUS_TRANSACTION_ERROR,
                actual_length: 0,
            }),
            Header::CmdUnlink(CmdUnlink {
                seqnum: 2,
                unlink_seqnum: 1,
            }),
            Header::RetUnlink(RetUnlink {
                seqnum: 2,
                status: 0,
            }),
        ] {
            assert_eq!(Header::decode(&header.encode()), Ok(header));
        }
    }

    #[test]
    fn headers_that_make_no_sense_are_refused() {
        let mut header = Header::CmdSubmit(CmdSubmit {
            seqnum: 1,
            direction: Direction::In,
            ep: 1,
            transfer_buffer_length: 8,
            setup: [0; 8],
        })
        .encode();

        let mut broken = header;
        broken[3] = 9;
        assert_eq!(Header::decode(&broken), Err(DecodeError::UnknownCommand(9)));
        let mut broken = header;
        broken[15] = 2;
        assert_eq!(
            Header::decode(&broken),
            Err(DecodeError::UnknownDirection(2))
        );
        let mut broken = header;
        broken[19] = 16;
        assert_eq!(
            Header::decode(&broken),
            Err(DecodeError::UnknownEndpoint(16))
        );
        header[35] = 2;
        assert_eq!(
            Header::decode(&header),
            Err(DecodeError::IsochronousPackets(2))
        );
        // Linux marks a transfer that is not isochronous with all ones.
        header[32..36].copy_from_slice(&u32::MAX.to_be_bytes());
        Header::decode(&header).unwrap();
    }
}
