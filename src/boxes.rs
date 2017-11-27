//! Functionality related to libsodium crypto boxes.
//!
//! An open box consists of an unencrypted message and a nonce.
//!
//! A sealed box consists of the encrypted message bytes and a nonce.

use rust_sodium::crypto::box_::NONCEBYTES;

use errors::{Result, ResultExt, ErrorKind};
use crypto::{KeyStore, PublicKey, AuthToken};
use protocol::Nonce;
use protocol::messages::Message;

/// An open box (unencrypted message + nonce).
#[derive(Debug, PartialEq)]
pub struct OpenBox {
    pub message: Message,
    pub nonce: Nonce,
}

impl OpenBox {
    pub fn new(message: Message, nonce: Nonce) -> Self {
        OpenBox { message, nonce }
    }
}


impl OpenBox {
    /// Encode without encryption into a [`ByteBox`](struct.ByteBox.html).
    ///
    /// This should only be necessary for the server-hello message. All other
    /// messages are encrypted.
    pub fn encode(self) -> ByteBox {
        let bytes = self.message.to_msgpack();
        ByteBox::new(bytes, self.nonce)
    }

    /// Encrypt message for the `other_key` using public key cryptography.
    pub fn encrypt(self, keystore: &KeyStore, other_key: &PublicKey) -> ByteBox {
        let encrypted = keystore.encrypt(
            // The message bytes to be encrypted
            &self.message.to_msgpack(),
            // The nonce. The unsafe call to `clone()` is required because the
            // nonce needs to be used both for encrypting, as well as being
            // sent along with the message bytes.
            unsafe { self.nonce.clone() },
            // The public key of the recipient
            other_key
        );
        ByteBox::new(encrypted, self.nonce)
    }

    /// Encrypt token message using the `auth_token` using secret key cryptography.
    pub fn encrypt_token(self, auth_token: &AuthToken) -> ByteBox {
        let encrypted = auth_token.encrypt(
            // The message bytes to be encrypted
            &self.message.to_msgpack(),
            // The nonce. The unsafe call to `clone()` is required because the
            // nonce needs to be used both for encrypting, as well as being
            // sent along with the message bytes.
            unsafe { self.nonce.clone() }
        );
        ByteBox::new(encrypted, self.nonce)
    }
}


/// A byte box (message bytes + nonce). The bytes may or may not be encrypted.
#[derive(Debug, PartialEq)]
pub struct ByteBox {
    pub bytes: Vec<u8>,
    pub nonce: Nonce,
}

impl ByteBox {
    pub fn new(bytes: Vec<u8>, nonce: Nonce) -> Self {
        ByteBox { bytes, nonce }
    }

    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        ensure!(bytes.len() > NONCEBYTES, ErrorKind::Decode("message is too short".into()));
        let nonce = Nonce::from_bytes(&bytes[..24])
            .chain_err(|| ErrorKind::Decode("cannot decode nonce".into()))?;
        let bytes = bytes[24..].to_vec();
        Ok(Self::new(bytes, nonce))
    }

    /// Decode an unencrypted message into an [`OpenBox`](struct.OpenBox.html).
    ///
    /// This should only be necessary for the server-hello message. All other
    /// messages are encrypted.
    pub fn decode(self) -> Result<OpenBox> {
        let message = Message::from_msgpack(&self.bytes)
            .chain_err(|| ErrorKind::Decode("cannot decode message payload".into()))?;
        Ok(OpenBox::new(message, self.nonce))
    }

    /// Decrypt an encrypted message into an [`OpenBox`](struct.OpenBox.html).
    pub fn decrypt(self, keystore: &KeyStore, other_key: &PublicKey) -> Result<OpenBox> {
        let decrypted = keystore.decrypt(
            // The message bytes to be decrypted
            &self.bytes,
            // The nonce. The unsafe call to `clone()` is required because the
            // nonce needs to be used both for decrypting, as well as being
            // passed along with the message bytes.
            unsafe { self.nonce.clone() },
            // The public key of the recipient
            other_key
        ).chain_err(|| ErrorKind::Decode("cannot decode message payload".into()))?;

        trace!("Decrypted bytes: {:?}", decrypted);

        let message = Message::from_msgpack(&decrypted)
            .chain_err(|| ErrorKind::Decode("cannot decode message payload".into()))?;

        Ok(OpenBox::new(message, self.nonce))
    }

    pub fn into_bytes(self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(NONCEBYTES + self.bytes.len());
        bytes.extend(self.nonce.into_bytes().iter());
        bytes.extend(self.bytes.iter());
        bytes
    }
}


#[cfg(test)]
mod tests {
    use protocol::cookie::Cookie;
    use protocol::csn::CombinedSequenceSnapshot;
    use protocol::types::Address;

    use super::*;


    /// Return a test nonce.
    fn create_test_nonce() -> Nonce {
        Nonce::new(
            Cookie::new([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]),
            Address(17),
            Address(18),
            CombinedSequenceSnapshot::new(258, 50_595_078),
        )
    }

    /// Return bytes of a server-hello message.
    fn create_test_msg_bytes() -> Vec<u8> {
        vec![
            // Fixmap with two entries
            0x82,
            // Key: type
            0xa4, 0x74, 0x79, 0x70, 0x65,
            // Val: server-hello
            0xac, 0x73, 0x65, 0x72, 0x76, 0x65, 0x72, 0x2d, 0x68, 0x65, 0x6c, 0x6c, 0x6f,
            // Key: key
            0xa3, 0x6b, 0x65, 0x79,
            // Val: Binary 32 bytes
            0xc4, 0x20,
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x00,
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x00,
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x00,
            0x63, 0xff,
        ]
    }

    #[test]
    fn byte_box_from_slice() {
        let bytes = [
            1, 2, 3, 4, 5, 6, 7, 8,
            8, 7, 6, 5, 4, 3, 2, 1,
            1, 2, 3, 4, 5, 6, 7, 8,
            9, 10,
        ];
        let bbox = ByteBox::from_slice(&bytes).unwrap();
        assert_eq!(bbox.nonce.csn().overflow_number(), (3 << 8) + 4);
        assert_eq!(bbox.nonce.csn().sequence_number(), (5 << 24) + (6 << 16) + (7 << 8) + 8);
        assert_eq!(bbox.bytes, vec![9, 10]);
    }

    #[test]
    fn byte_box_from_slice_too_short() {
        let bytes_only_nonce = [1, 2, 3, 4, 5, 6, 7, 8,
                                8, 7, 6, 5, 4, 3, 2, 1,
                                1, 2, 3, 4, 5, 6, 7, 8];
        let bytes_not_even_nonce = [1, 2, 3, 4, 5, 6, 7, 8];

        let err1 = ByteBox::from_slice(&bytes_only_nonce).unwrap_err();
        let err2 = ByteBox::from_slice(&bytes_not_even_nonce).unwrap_err();
        assert_eq!(format!("{}", err1), "decoding error: message is too short");
        assert_eq!(format!("{}", err2), "decoding error: message is too short");
    }

    #[test]
    fn byte_box_decode() {
        let nonce = create_test_nonce();
        let bbox = ByteBox::new(create_test_msg_bytes(), nonce);
        let obox = bbox.decode().unwrap();
        assert_eq!(obox.message.get_type(), "server-hello");
    }

    #[test]
    fn byte_box_decrypt() {
        let nonce = create_test_nonce();
        let bytes = create_test_msg_bytes();
        let keystore_tx = KeyStore::new().unwrap();
        let keystore_rx = KeyStore::new().unwrap();
        let encrypted = keystore_tx.encrypt(&bytes, unsafe { nonce.clone() }, keystore_rx.public_key());
        let bbox = ByteBox::new(encrypted, nonce);
        let obox = bbox.decrypt(&keystore_rx, keystore_tx.public_key()).unwrap();
        assert_eq!(obox.message.get_type(), "server-hello");
    }
}
