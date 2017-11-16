use std::convert::From;
use std::fmt;
use std::result::Result as StdResult;

use serde::ser::{Serialize, Serializer};
use serde::de::{Deserialize, Deserializer, Visitor, Error as SerdeError};

use boxes::{ByteBox};


/// The role of a peer.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Role {
    /// A SaltyRTC compliant client who wants to establish a WebRTC or ORTC
    /// peer-to-peer connection to a responder.
    Initiator,
    /// A SaltyRTC compliant client who wants to establish a WebRTC or ORTC
    /// peer-to-peer connection to an initiator.
    Responder,
}

impl Role {
    pub fn is_initiator(&self) -> bool {
        *self == Role::Initiator
    }

    pub fn is_responder(&self) -> bool {
        *self == Role::Responder
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Role::Initiator => write!(f, "Initiator"),
            Role::Responder => write!(f, "Responder"),
        }
    }
}


/// A peer identity.
///
/// On the network level, this is encoded as a single unsigned byte.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Identity {
    /// An unknown identity is initialized to `0x00`.
    Unknown,
    /// The initiator has the identity `0x01`.
    Initiator,
    /// The responder has an identity in the range `0x02-0xff`.
    Responder(u8),
    /// The server has the identity `0x00`.
    Server,
}

impl From<Address> for Identity {
    fn from(val: Address) -> Self {
        match val.0 {
            0x00 => Identity::Server,
            0x01 => Identity::Initiator,
            addr => Identity::Responder(addr),
        }
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Identity::Unknown => write!(f, "unknown"),
            Identity::Initiator => write!(f, "initiator"),
            Identity::Responder(id) => write!(f, "responder {}", id),
            Identity::Server => write!(f, "server"),
        }
    }
}


/// A client identity.
///
/// This is like the [`Identity`](enum.identity.html), but the `Server` value
/// is not allowed.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ClientIdentity {
    /// An unknown identity is initialized to `0x00`.
    Unknown,
    /// The initiator has the identity `0x01`.
    Initiator,
    /// The responder has an identity in the range `0x02-0xff`.
    Responder(u8),
}

impl fmt::Display for ClientIdentity {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ClientIdentity::Unknown => write!(f, "Unknown"),
            ClientIdentity::Initiator => write!(f, "Initiator"),
            ClientIdentity::Responder(ref val) => write!(f, "Responder[{:#04x}]", val),
        }
    }
}


/// An address.
///
/// This is an unsigned byte like the [`Identity`](enum.Identity.html),
/// but without any semantic information attached.
#[derive(Debug, PartialEq, Eq, Copy, Clone, Hash)]
pub struct Address(pub u8);

impl Address {
    /// Return whether this address is a valid server address.
    pub fn is_server(&self) -> bool {
        self.0 == 0x00
    }

    /// Return whether this address is a valid unknown address.
    pub fn is_unknown(&self) -> bool {
        self.0 == 0x00
    }

    /// Return whether this address is the initiator address.
    pub fn is_initiator(&self) -> bool {
        self.0 == 0x01
    }

    /// Return whether this address is in the responder range.
    pub fn is_responder(&self) -> bool {
        self.0 >= 0x02
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Address({:#04x})", self.0)
    }
}

impl From<ClientIdentity> for Address {
    /// Convert a [`ClientIdentity`](enum.ClientIdentity.html) into the
    /// corresponding address.
    ///
    /// Panics if a `Responder` with an out-of-range value is encountered.
    fn from(val: ClientIdentity) -> Self {
        Address(match val {
            ClientIdentity::Unknown => 0x00,
            ClientIdentity::Initiator => 0x01,
            ClientIdentity::Responder(address) => { assert!(address > 0x01); address },
        })
    }
}

impl From<Identity> for Address {
    /// Convert an [`Identity`](enum.Identity.html) into the
    /// corresponding address.
    ///
    /// Panics if a `Responder` with an out-of-range value is encountered.
    fn from(val: Identity) -> Self {
        Address(match val {
            Identity::Unknown | Identity::Server => 0x00,
            Identity::Initiator => 0x01,
            Identity::Responder(address) => { assert!(address > 0x01); address },
        })
    }
}

impl From<u8> for Address {
    /// Convert an u8 into the corresponding address.
    fn from(val: u8) -> Self {
        Address(val)
    }
}

/// Waiting for https://github.com/3Hren/msgpack-rust/issues/129
impl Serialize for Address {
    fn serialize<S>(&self, serializer: S) -> StdResult<S::Ok, S::Error>
            where S: Serializer {
        serializer.serialize_u8(self.0)
    }
}

struct AddressVisitor;

impl<'de> Visitor<'de> for AddressVisitor {
    type Value = Address;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an address byte")
    }

    fn visit_u8<E>(self, v: u8) -> StdResult<Self::Value, E> where E: SerdeError {
        Ok(Address(v))
    }
}

/// Waiting for https://github.com/3Hren/msgpack-rust/issues/129
impl<'de> Deserialize<'de> for Address {
    fn deserialize<D>(deserializer: D) -> StdResult<Self, D::Error>
            where D: Deserializer<'de> {
        deserializer.deserialize_u8(AddressVisitor)
    }
}


/// An enum returned when an incoming message is handled.
///
/// It can contain different actions that should be done to finish handling the
/// message.
///
/// TODO: This could be split up into actions for the signaling state and
/// actions for the network part.
#[derive(Debug, PartialEq)]
pub enum HandleAction {
    /// Send the specified message through the websocket.
    Reply(ByteBox),
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_identity_into_address() {
        let unknown = ClientIdentity::Unknown;
        let initiator = ClientIdentity::Initiator;
        let responder = ClientIdentity::Responder(0x13);

        assert_eq!(Address::from(unknown), Address(0x00));
        assert_eq!(Address::from(initiator), Address(0x01));
        assert_eq!(Address::from(responder), Address(0x13));
    }

    /// Converting an invalid `Responder` into an `Address` should panic.
    #[test]
    #[should_panic]
    fn client_identity_invalid_responder_into_address() {
        let responder_invalid = ClientIdentity::Responder(0x01);
        let _: Address = responder_invalid.into();
    }

    #[test]
    fn address_display() {
        assert_eq!(format!("{}", Address(0)), "Address(0x00)");
        assert_eq!(format!("{}", Address(3)), "Address(0x03)");
        assert_eq!(format!("{}", Address(255)), "Address(0xff)");
    }

    #[test]
    fn client_identity_display() {
        let unknown = ClientIdentity::Unknown;
        let initiator = ClientIdentity::Initiator;
        let responder = ClientIdentity::Responder(10);

        assert_eq!(format!("{}", unknown), "Unknown");
        assert_eq!(format!("{}", initiator), "Initiator");
        assert_eq!(format!("{}", responder), "Responder[0x0a]");
    }
}
