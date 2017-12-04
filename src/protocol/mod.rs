//! Protocol state machines.
//!
//! These state machines handle all state transitions independently of the
//! connection. Instead of executing side effects (like sending a response
//! message to the peer through the websocket), a
//! [`HandleAction`](types/enum.HandleAction.html) is returned.
//!
//! All state is contained in the [context structs](context/index.html),
//! depending on the role.
//!
//! This allows for better decoupling between protocol logic and network code,
//! and makes it possible to easily add tests.

use std::collections::{HashMap, HashSet};

use boxes::{ByteBox, OpenBox};
use crypto::{KeyStore, AuthToken, PublicKey};
use errors::{SignalingError, SignalingResult};

pub(crate) mod context;
pub(crate) mod cookie;
pub(crate) mod csn;
pub(crate) mod messages;
pub(crate) mod nonce;
pub(crate) mod send_error;
pub(crate) mod state;
pub(crate) mod types;

use self::context::{PeerContext, ServerContext, InitiatorContext, ResponderContext};
pub(crate) use self::cookie::{Cookie};
use self::messages::{Message, ServerHello, ServerAuth, ClientHello, ClientAuth, NewResponder};
use self::messages::{Token, Key};
pub(crate) use self::nonce::{Nonce};
pub use self::types::{Role};
pub(crate) use self::types::{HandleAction};
use self::types::{ClientIdentity, Address};
use self::state::{SignalingState, ServerHandshakeState, InitiatorHandshakeState};


/// The signaling implementation.
///
/// This enum contains all the signaling logic. Role specific logic is
/// delegated to the inner signaling type.
pub(crate) enum Signaling {
    Initiator(InitiatorSignaling),
    Responder(ResponderSignaling),
}

impl From<InitiatorSignaling> for Signaling {
    fn from(val: InitiatorSignaling) -> Self {
        Signaling::Initiator(val)
    }
}

impl From<ResponderSignaling> for Signaling {
    fn from(val: ResponderSignaling) -> Self {
        Signaling::Responder(val)
    }
}

/// Make it possible to reference the enum variants directly
use Signaling::{Initiator, Responder};

/// Macro to simplify repetitive enum matching.
macro_rules! on_inner {
    ($self:expr, $var:pat, $expr:expr) => {{
        match *$self {
            Signaling::Initiator($var) => $expr,
            Signaling::Responder($var) => $expr,
        }
    }}
}

impl Signaling {
    /// Create a new initiator signaling instance.
    pub(crate) fn new_initiator(permanent_key: KeyStore) -> Self {
        Signaling::Initiator(InitiatorSignaling::new(permanent_key))
    }

    /// Create a new responder signaling instance.
    pub(crate) fn new_responder(permanent_key: KeyStore,
                                initiator_pubkey: PublicKey,
                                auth_token: Option<AuthToken>) -> Self {
        Signaling::Responder(ResponderSignaling::new(permanent_key, initiator_pubkey, auth_token))
    }

    /// Return our role, either initiator or responder.
    pub(crate) fn role(&self) -> Role {
        match *self {
            Signaling::Initiator(_) => Role::Initiator,
            Signaling::Responder(_) => Role::Responder,
        }
    }

    /// Return the signaling state.
    fn signaling_state(&self) -> SignalingState {
        on_inner!(self, ref s, s.signaling_state)
    }

    /// Update the signaling state.
    fn set_signaling_state(&mut self, state: SignalingState) -> SignalingResult<()> {
        macro_rules! update { () => {{
            trace!("Signaling state transition: {:?} -> {:?}", self.signaling_state(), state);
            on_inner!(self, ref mut s, s.signaling_state = state);
            Ok(())
        }} }
        macro_rules! ignore { () => {{
            trace!("Ignoring signaling state transition: {:?} -> {:?}", self.signaling_state(), state);
            Ok(())
        }} };
        macro_rules! fail { () => {{
            Err(SignalingError::InvalidStateTransition(
                format!("Signaling state: {:?} -> {:?}", self.signaling_state(), state)
            ))
        }} };

        match self.signaling_state() {
            SignalingState::ServerHandshake => match state {
                SignalingState::ServerHandshake => ignore!(),
                _ => update!(),
            },
            SignalingState::PeerHandshake => {
                match state {
                    SignalingState::ServerHandshake => fail!(),
                    SignalingState::PeerHandshake => ignore!(),
                    _ => update!(),
                }
            },
            SignalingState::Task => fail!(),
        }
    }

    /// Return our assigned client identity.
    fn identity(&self) -> ClientIdentity {
        on_inner!(self, ref s, s.identity)
    }

    /// Set the client identity.
    fn set_identity(&mut self, identity: ClientIdentity) {
        on_inner!(self, ref mut s, s.identity = identity);
    }

    /// Return our permanent keypair.
    fn permanent_key(&self) -> &KeyStore {
        on_inner!(self, ref s, &s.permanent_key)
    }

    /// Return our auth token.
    pub(crate) fn auth_token(&self) -> Option<&AuthToken> {
        on_inner!(self, ref s, s.auth_token.as_ref())
    }

    /// Return the server context.
    fn server(&self) -> &ServerContext {
        on_inner!(self, ref s, &s.server)
    }

    /// Return the mutable server context.
    fn server_mut(&mut self) -> &mut ServerContext {
        on_inner!(self, ref mut s, &mut s.server)
    }

    /// Return the responder with the specified address (if present).
    fn responder_with_address_mut(&mut self, addr: &Address) -> Option<&mut ResponderContext> {
        match *self {
            Initiator(ref mut s) => s.responders.get_mut(addr),
            Responder(_) => {
                warn!("Called responder_with_address_mut on a responder!");
                None
            }
        }
    }

    /// Handle an incoming message.
    pub(crate) fn handle_message(&mut self, bbox: ByteBox) -> SignalingResult<Vec<HandleAction>> {
        // Validate the nonce
        match self.validate_nonce(&bbox.nonce) {
            // It's valid! Carry on.
            ValidationResult::Ok => {},

            // Drop and ignore some of the messages
            ValidationResult::DropMsg(warning) => {
                warn!("Invalid nonce: {}", warning);
                return Ok(vec![]);
            },

            // Nonce is invalid, fail the signaling
            ValidationResult::Fail(reason) => return Err(SignalingError::InvalidNonce(reason)),
        }

        // Decode message
        let obox: OpenBox = self.decode_msg(bbox)?;

        match self.signaling_state() {
            SignalingState::ServerHandshake =>
                self.handle_server_message(obox),

            SignalingState::PeerHandshake if obox.nonce.source().is_server() =>
                self.handle_server_message(obox),
            SignalingState::PeerHandshake =>
                match *self {
                    Signaling::Initiator(ref mut sig) => sig.handle_peer_message(obox),
                    Signaling::Responder(ref mut sig) => sig.handle_peer_message(obox),
                },

            SignalingState::Task =>
                unimplemented!("TODO: Handle task messages"),
        }
    }

    /// Determine the next server handshake state based on the incoming
    /// server-to-client message and the current state.
    ///
    /// This method call may have some side effects, like updates in the peer
    /// context (cookie, CSN, etc).
    fn handle_server_message(&mut self, obox: OpenBox) -> SignalingResult<Vec<HandleAction>> {
        let old_state = self.server().handshake_state().clone();
        match (old_state, obox.message) {
            // Valid state transitions
            (ServerHandshakeState::New, Message::ServerHello(msg)) =>
                self.handle_server_hello(msg),
            (ServerHandshakeState::ClientInfoSent, Message::ServerAuth(msg)) =>
                self.handle_server_auth(msg),
            (ServerHandshakeState::Done, Message::NewResponder(msg)) =>
                on_inner!(self, ref mut s, s.handle_new_responder(msg)),
            (ServerHandshakeState::Done, Message::DropResponder(_msg)) =>
                unimplemented!("Handling DropResponder messages not yet implemented"),
            (ServerHandshakeState::Done, Message::SendError(_msg)) =>
                unimplemented!("Handling SendError messages not yet implemented"),

            // Any undefined state transition results in an error
            (s, message) => Err(SignalingError::InvalidStateTransition(
                format!("Got {} message from server in {:?} state", message.get_type(), s)
            )),
        }
    }

    /// Validate the nonce
    fn validate_nonce(&mut self, nonce: &Nonce) -> ValidationResult {
		// A client MUST check that the destination address targets its
		// assigned identity (or `0x00` during authentication).
        if self.identity() == ClientIdentity::Unknown
                && !nonce.destination().is_unknown()
                && self.server().handshake_state() != ServerHandshakeState::New {
            // The first message received with a destination address different
            // to `0x00` SHALL be accepted as the client's assigned identity.
            // However, the client MUST validate that the identity fits its
            // role – initiators SHALL ONLY accept `0x01` and responders SHALL
            // ONLY an identity from the range `0x02..0xff`. The identity MUST
            // be stored as the client's assigned identity.
            match self.role() {
                Role::Initiator => {
                    if nonce.destination().is_initiator() {
                        self.set_identity(ClientIdentity::Initiator);
                        debug!("Assigned identity: {}", &self.identity());
                    } else {
                        let msg = format!("cannot assign address {} to a client with role {}", nonce.destination(), self.role());
                        return ValidationResult::Fail(msg);
                    }
                },
                Role::Responder => {
                    if nonce.destination().is_responder() {
                        self.set_identity(ClientIdentity::Responder(nonce.destination().0));
                        debug!("Assigned identity: {}", &self.identity());
                    } else {
                        let msg = format!("cannot assign address {} to a client with role {}", nonce.destination(), self.role());
                        return ValidationResult::Fail(msg);
                    }
                },
            };
        }
        if nonce.destination() != self.identity().into() {
            let msg = format!("Bad destination: {} (our identity is {})", nonce.destination(), self.identity());
            return ValidationResult::Fail(msg);
        }

        // An initiator SHALL ONLY process messages from the server (0x00). As
        // soon as the initiator has been assigned an identity, it MAY ALSO accept
        // messages from other responders (0x02..0xff). Other messages SHALL be
        // discarded and SHOULD trigger a warning.
        //
        // A responder SHALL ONLY process messages from the server (0x00). As soon
        // as the responder has been assigned an identity, it MAY ALSO accept
        // messages from the initiator (0x01). Other messages SHALL be discarded
        // and SHOULD trigger a warning.
        match nonce.source() {
            // From server
            Address(0x00) => {},

            // From initiator
            Address(0x01) => {
                match self.identity() {
                    // We're the responder: OK
                    ClientIdentity::Responder(_) => {},
                    // Otherwise: Not OK
                    _ => {
                        let msg = format!("bad source: {} (our identity is {})", nonce.source(), self.identity());
                        return ValidationResult::DropMsg(msg);
                    },
                }
            },

            // From responder
            Address(0x02...0xff) => {
                match self.identity() {
                    // We're the initiator: OK
                    ClientIdentity::Initiator => {},
                    // Otherwise: Not OK
                    _ => {
                        let msg = format!("bad source: {} (our identity is {})", nonce.source(), self.identity());
                        return ValidationResult::DropMsg(msg);
                    },
                }
            },

            // Required due to https://github.com/rust-lang/rfcs/issues/1550
            Address(_) => unreachable!(),
        };

        // Find peer
        // TODO: Also consider signaling state, see InitiatorSignaling.java getPeerWithId
        let peer: &mut PeerContext = match nonce.source().0 {
            0x00 => self.server_mut(),
            0x01 => unimplemented!(),
            addr @ 0x02...0xff => {
                match self.responder_with_address_mut(&nonce.source()) {
                    Some(responder) => responder,
                    None => return ValidationResult::Fail(format!("could not find responder with address {}", addr)),
                }
            },
            _ => unreachable!(),
        };

        let peer_identity = peer.identity();

        // Validate CSN
        //
        // In case this is the first message received from the sender, the peer:
        //
        // * MUST check that the overflow number of the source peer is 0 and,
        // * if the peer has already sent a message to the sender, MUST check
        //   that the sender's cookie is different than its own cookie, and
        // * MUST store the combined sequence number for checks on further messages.
        // * The above number(s) SHALL be stored and updated separately for
        //   each other peer by its identity (source address in this case).
        //
        // Otherwise, the peer:
        //
        // * MUST check that the combined sequence number of the source peer
        //   has been increased by 1 and has not reset to 0.
        {
            let mut csn_pair = peer.csn_pair().borrow_mut();

            // If we already have the CSN of the peer,
            // ensure that it has been increased properly.
            if let Some(ref mut csn) = csn_pair.theirs {
                let previous = csn;
                let current = nonce.csn();
                if current < previous {
                    let msg = format!("{} CSN is lower than last time", peer_identity);
                    return ValidationResult::Fail(msg);
                } else if current == previous {
                    let msg = format!("{} CSN hasn't been incremented", peer_identity);
                    return ValidationResult::Fail(msg);
                } else {
                    *previous = current.clone();
                }
            }

            // Otherwise, this is the first message from that peer.
            if csn_pair.theirs.is_none() {
                // Validate the overflow number...
                if nonce.csn().overflow_number() != 0 {
                    let msg = format!("First message from {} must have set the overflow number to 0", peer_identity);
                    return ValidationResult::Fail(msg);
                }
                // ...and store the CSN.
                csn_pair.theirs = Some(nonce.csn().clone());
            }
        }

        // Validate cookie
        //
        // In case this is the first message received from the sender:
        //
        // * If the peer has already sent a message to the sender, it MUST
        //   check that the sender's cookie is different than its own cookie, and
        // * MUST store cookie for checks on further messages
        // * The above number(s) SHALL be stored and updated separately for
        //   each other peer by its identity (source address in this case).
        //
        // Otherwise, the peer:
        //
        // * MUST ensure that the 16 byte cookie of the sender has not changed
        {
            let cookie_pair = peer.cookie_pair_mut();
            match cookie_pair.theirs {
                None => {
                    // This is the first message from that peer,
                    // validate the cookie...
                    if *nonce.cookie() == cookie_pair.ours {
                        let msg = format!("Cookie from {} is identical to our own cookie", peer_identity);
                        return ValidationResult::Fail(msg);
                    }
                    // ...and store it.
                    cookie_pair.theirs = Some(nonce.cookie().clone());
                },
                Some(ref cookie) => {
                    // Ensure that the cookie has not changed
                    if nonce.cookie() != cookie {
                        let msg = format!("Cookie from {} has changed", peer_identity);
                        return ValidationResult::Fail(msg);
                    }
                },
            }
        }

        ValidationResult::Ok
    }

    /// Decode or decrypt a binary message depending on the state
    fn decode_msg(&self, bbox: ByteBox) -> SignalingResult<OpenBox> {
        if self.server().handshake_state() == ServerHandshakeState::New {
            // If we're in state `New`, message must be unencrypted.
            bbox.decode()
        } else {
            // Otherwise, decrypt
            match self.server().permanent_key {
                Some(ref pubkey) => bbox.decrypt(&self.permanent_key(), pubkey),
                None => Err(SignalingError::Crash("Missing server permanent key".into())),
            }
        }
    }

    /// Handle an incoming [`ServerHello`](messages/struct.ServerHello.html) message.
    fn handle_server_hello(&mut self, msg: ServerHello) -> SignalingResult<Vec<HandleAction>> {
        debug!("Received server-hello");

        let mut actions = Vec::with_capacity(2);

        // Set the server public permanent key
        trace!("Server permanent key is {:?}", msg.key);
        if self.server().permanent_key.is_some() {
            return Err(SignalingError::Protocol(
                "Got a server-hello message, but server permanent key is already set".to_string()
            ));
        }
        self.server_mut().permanent_key = Some(msg.key);

        // Reply with client-hello message if we're a responder
        if self.role() == Role::Responder {
            let client_hello = {
                let key = self.permanent_key().public_key();
                ClientHello::new(*key).into_message()
            };
            let client_hello_nonce = Nonce::new(
                // Cookie
                self.server().cookie_pair().ours.clone(),
                // Src
                self.identity().into(),
                // Dst
                self.server().identity().into(),
                // Csn
                self.server().csn_pair().borrow_mut().ours.increment()?,
            );
            let reply = OpenBox::new(client_hello, client_hello_nonce);
            debug!("Enqueuing client-hello");
            actions.push(HandleAction::Reply(reply.encode()));
        }

        // Send client-auth message
        let client_auth = ClientAuth {
            your_cookie: self.server().cookie_pair().theirs.clone().unwrap(),
            subprotocols: vec![::SUBPROTOCOL.into()],
            ping_interval: 0, // TODO
            your_key: None, // TODO
        }.into_message();
        let client_auth_nonce = Nonce::new(
            self.server().cookie_pair().ours.clone(),
            self.identity().into(),
            self.server().identity().into(),
            self.server().csn_pair().borrow_mut().ours.increment()?,
        );
        let reply = OpenBox::new(client_auth, client_auth_nonce);
        match self.server().permanent_key {
            Some(ref pubkey) => {
                debug!("Enqueuing client-auth");
                actions.push(HandleAction::Reply(reply.encrypt(&self.permanent_key(), pubkey)));
            },
            None => return Err(SignalingError::Crash("Missing server permanent key".into())),
        };

        // TODO: Can we prevent confusing an incoming and an outgoing nonce?
        self.server_mut().set_handshake_state(ServerHandshakeState::ClientInfoSent);
        Ok(actions)
    }

    /// Handle an incoming [`ServerAuth`](messages/struct.ServerAuth.html) message.
    fn handle_server_auth(&mut self, msg: ServerAuth) -> SignalingResult<Vec<HandleAction>> {
        debug!("Received server-auth");

        // When the client receives a 'server-auth' message, it MUST
        // have accepted and set its identity as described in the
        // Receiving a Signalling Message section.
        if self.identity() == ClientIdentity::Unknown {
            return Err(SignalingError::Crash(
                "No identity assigned when receiving server-auth message".into()
            ));
        }

        // It MUST check that the cookie provided in the your_cookie
        // field contains the cookie the client has used in its
        // previous and messages to the server.
        if msg.your_cookie != self.server().cookie_pair().ours {
            trace!("Our cookie as sent by server: {:?}", msg.your_cookie);
            trace!("Our actual cookie: {:?}", self.server().cookie_pair().ours);
            return Err(SignalingError::InvalidMessage(
                "Cookie sent in server-auth message does not match our cookie".into()
            ));
        }

        // If the client has knowledge of the server's public permanent
        // key, it SHALL decrypt the signed_keys field by using the
        // message's nonce, the client's private permanent key and the
        // server's public permanent key. The decrypted message MUST
        // match the concatenation of the server's public session key
        // and the client's public permanent key (in that order). If
        // the signed_keys is present but the client does not have
        // knowledge of the server's permanent key, it SHALL log a
        // warning.
        // TODO: Implement

        // Moreover, the client MUST do some checks depending on its role
        let actions = on_inner!(self, ref mut s, s.handle_server_auth(&msg))?;

        info!("Server handshake completed");
        self.server_mut().set_handshake_state(ServerHandshakeState::Done);
        self.set_signaling_state(SignalingState::PeerHandshake)?;
        Ok(actions)
    }

    /// Return the inner `InitiatorSignaling` instance.
    ///
    /// Panics if we're not an initiator
    #[cfg(test)]
    fn as_initiator(&self) -> &InitiatorSignaling {
        match *self {
            Signaling::Initiator(ref s) => &s,
            Signaling::Responder(_) => panic!("Called .as_initiator() on a `Signaling::Responder`"),
        }
    }

    /// Return the inner `ResponderSignaling` instance.
    ///
    /// Panics if we're not an responder
    #[cfg(test)]
    fn as_responder(&self) -> &ResponderSignaling {
        match *self {
            Signaling::Responder(ref s) => &s,
            Signaling::Initiator(_) => panic!("Called .as_responder() on a `Signaling::Initiator`"),
        }
    }
}


/// Result of the nonce validation.
pub(crate) enum ValidationResult {
    Ok,
    DropMsg(String),
    Fail(String),
}


/// Signaling data for the initiator.
pub(crate) struct InitiatorSignaling {
    // The signaling state
    pub(crate) signaling_state: SignalingState,

    // Our permanent keypair
    pub(crate) permanent_key: KeyStore,

    // An optional auth token
    pub(crate) auth_token: Option<AuthToken>,

    // The assigned client identity
    pub(crate) identity: ClientIdentity,

    // The server context
    pub(crate) server: ServerContext,

    // The list of responders
    pub(crate) responders: HashMap<Address, ResponderContext>,

    // The chosen responder
    pub(crate) responder: Option<ResponderContext>,
}

impl InitiatorSignaling {
    pub(crate) fn new(permanent_key: KeyStore) -> Self {
        InitiatorSignaling {
            signaling_state: SignalingState::ServerHandshake,
            identity: ClientIdentity::Unknown,
            server: ServerContext::new(),
            permanent_key: permanent_key,
            auth_token: Some(AuthToken::new()),
            responders: HashMap::new(),
            responder: None,
        }
    }

    /// Determine the next peer handshake state based on the incoming
    /// client-to-client message and the current state.
    ///
    /// This method call may have some side effects, like updates in the peer
    /// context (cookie, CSN, etc).
    fn handle_peer_message(&mut self, obox: OpenBox) -> SignalingResult<Vec<HandleAction>> {
        // Find responder
        let source = obox.nonce.source();
        let responder = match self.responders.get(&source) {
            Some(responder) => responder,
            None => return Err(SignalingError::Crash(format!("Did not find responder with address {}", source))),
        };

        // State transitions
        let old_state = responder.handshake_state();
        match (old_state, obox.message) {
            // Valid state transitions
            // TODO
            //(ResponderHandshakeState::New, Message::ServerHello(msg)) => self.handle_server_hello(msg),

            // Any undefined state transition results in an error
            (s, message) => Err(SignalingError::InvalidStateTransition(
                format!("Got {} message from responder {} in {:?} state", message.get_type(), source, s)
            )),
        }
    }

    fn handle_server_auth(&mut self, msg: &ServerAuth) -> SignalingResult<Vec<HandleAction>> {
        // In case the client is the initiator, it SHALL check
        // that the responders field is set and contains an
        // Array of responder identities.
        if msg.initiator_connected.is_some() {
            return Err(SignalingError::InvalidMessage(
                "We're the initiator, but the `initiator_connected` field in the server-auth message is set".into()
            ));
        }
        let responders = match msg.responders {
            Some(ref responders) => responders,
            None => return Err(SignalingError::InvalidMessage(
                "`responders` field in server-auth message not set".into()
            )),
        };

        // The responder identities MUST be validated and SHALL
        // neither contain addresses outside the range
        // 0x02..0xff
        let responders_set: HashSet<Address> = responders.iter().cloned().collect();
        if responders_set.contains(&Address(0x00)) || responders_set.contains(&Address(0x01)) {
            return Err(SignalingError::InvalidMessage(
                "`responders` field in server-auth message may not contain addresses <0x02".into()
            ));
        }

        // ...nor SHALL an address be repeated in the
        // Array.
        if responders.len() != responders_set.len() {
            return Err(SignalingError::InvalidMessage(
                "`responders` field in server-auth message may not contain duplicates".into()
            ));
        }

        // An empty Array SHALL be considered valid. However,
        // Nil SHALL NOT be considered a valid value of that
        // field.
        // -> Already covered by Rust's type system.

        // It SHOULD store the responder's identities in its
        // internal list of responders.
        for address in responders_set {
            self.responders.insert(address, ResponderContext::new(address));
        }

        // Additionally, the initiator MUST keep its path clean
        // by following the procedure described in the Path
        // Cleaning section.
        // TODO: Implement

        Ok(vec![])
    }

    /// Handle an incoming [`NewResponder`](messages/struct.NewResponder.html) message.
    fn handle_new_responder(&mut self, msg: NewResponder) -> SignalingResult<Vec<HandleAction>> {
        debug!("Received new-responder");

        // An initiator who receives a 'new-responder' message SHALL validate
        // that the id field contains a valid responder address (0x02..0xff).
        if !msg.id.is_responder() {
            return Err(SignalingError::InvalidMessage(
                "`id` field in new-responder message is not a valid responder address".into()
            ));
        }

        // It SHOULD store the responder's identity in its internal list of responders.
        // If a responder with the same id already exists, all currently cached
        // information about and for the previous responder (such as cookies
        // and the sequence number) MUST be deleted first.
        if self.responders.contains_key(&msg.id) {
            warn!("Overwriting responder context for address {:?}", msg.id);
        } else {
            info!("Registering new responder with address {:?}", msg.id);
        }
        self.responders.insert(msg.id, ResponderContext::new(msg.id));

        // Furthermore, the initiator MUST keep its path clean by following the
        // procedure described in the Path Cleaning section.
        // TODO: Implement

        Ok(vec![])
    }

}

/// Signaling data for the responder.
pub(crate) struct ResponderSignaling {
    // The signaling state
    pub(crate) signaling_state: SignalingState,

    // Our permanent keypair
    pub(crate) permanent_key: KeyStore,

    // Our session keypair
    pub(crate) session_key: Option<KeyStore>,

    // An optional auth token
    pub(crate) auth_token: Option<AuthToken>,

    // The assigned client identity
    pub(crate) identity: ClientIdentity,

    // The server context
    pub(crate) server: ServerContext,

    // The initiator context
    pub(crate) initiator: InitiatorContext,
}

impl ResponderSignaling {
    pub(crate) fn new(permanent_key: KeyStore,
                      initiator_pubkey: PublicKey,
                      auth_token: Option<AuthToken>) -> Self {
        ResponderSignaling {
            signaling_state: SignalingState::ServerHandshake,
            permanent_key: permanent_key,
            session_key: None,
            auth_token: auth_token,
            identity: ClientIdentity::Unknown,
            server: ServerContext::new(),
            initiator: InitiatorContext::new(initiator_pubkey),
        }
    }

    /// Determine the next peer handshake state based on the incoming
    /// client-to-client message and the current state.
    ///
    /// This method call may have some side effects, like updates in the peer
    /// context (cookie, CSN, etc).
    fn handle_peer_message(&mut self, obox: OpenBox) -> SignalingResult<Vec<HandleAction>> {
        let old_state = self.initiator.handshake_state();
        match (old_state, obox.message) {
            // Valid state transitions
            // TODO
            //(ResponderHandshakeState::New, Message::ServerHello(msg)) => self.handle_server_hello(msg),

            // Any undefined state transition results in an error
            (s, message) => Err(SignalingError::InvalidStateTransition(
                format!("Got {} message from initiator in {:?} state", message.get_type(), s)
            )),
        }
    }

    fn handle_server_auth(&mut self, msg: &ServerAuth) -> SignalingResult<Vec<HandleAction>> {
        // In case the client is the responder, it SHALL check
        // that the initiator_connected field contains a
        // boolean value.
        if msg.responders.is_some() {
            return Err(SignalingError::InvalidMessage(
                "We're a responder, but the `responders` field in the server-auth message is set".into()
            ));
        }
        let mut actions: Vec<HandleAction> = vec![];
        match msg.initiator_connected {
            Some(true) => {
                if let Some(ref token) = self.auth_token {
                    actions.push(self.send_token(&token)?);
                } else {
                    debug!("No auth token set");
                }
                self.generate_session_key()?;
                actions.push(self.send_key()?);
                self.initiator.set_handshake_state(InitiatorHandshakeState::KeySent);
            },
            Some(false) => {
                debug!("No initiator connected so far");
            },
            None => return Err(SignalingError::InvalidMessage(
                "We're a responder, but the `initiator_connected` field in the server-auth message is not set".into()
            )),
        }
        Ok(actions)
    }

    /// Handle an incoming [`NewResponder`](messages/struct.NewResponder.html) message.
    fn handle_new_responder(&mut self, msg: NewResponder) -> SignalingResult<Vec<HandleAction>> {
        Err(SignalingError::Protocol("Received 'new-responder' message as responder".into()))
    }

    fn generate_session_key(&mut self) -> SignalingResult<()> {
        if self.session_key.is_some() {
            return Err(
                SignalingError::Crash("Cannot generate new session key: It has already been generated".into())
            );
        }

        // The client MUST generate a session key pair (a new NaCl key pair for
        // public key authenticated encryption) for further communication with
        // the other client.
        //
        // Note: This *could* cause a panic if libsodium initialization fails, but
        // that's not possible in practice because libsodium should already
        // have been initialized previously.
        let mut session_key = KeyStore::new().expect("Libsodium initialization failed");
        while session_key == self.permanent_key {
            warn!("Session keypair == permanent keypair! This is highly unlikely. Regenerating...");
            session_key = KeyStore::new().expect("Libsodium initialization failed");
        }
        self.session_key = Some(session_key);
        Ok(())
    }

    /// Build a `Token` message.
    fn send_token(&self, token: &AuthToken) -> SignalingResult<HandleAction> {
        // The responder MUST set the public key (32 bytes) of the permanent
        // key pair in the key field of this message.
        let msg: Message = Token::new(self.permanent_key.public_key().to_owned()).into_message();
        let nonce = Nonce::new(
            self.initiator.cookie_pair().ours.clone(),
            self.identity.into(),
            self.initiator.identity().into(),
            self.initiator.csn_pair().borrow_mut().ours.increment()?,
        );
        let obox = OpenBox::new(msg, nonce);

        // The message SHALL be NaCl secret key encrypted by the token the
        // initiator created and issued to the responder.
        let bbox = obox.encrypt_token(&token);

        // TODO: In case the initiator has successfully decrypted the 'token'
        // message, the secret key MUST be invalidated immediately and SHALL
        // NOT be used for any other message.

        debug!("Enqueuing token");
        Ok(HandleAction::Reply(bbox))
    }

    /// Build a `Key` message.
    fn send_key(&self) -> SignalingResult<HandleAction> {
        // It MUST set the public key (32 bytes) of that key pair in the key field.
        let msg: Message = match self.session_key {
            Some(ref session_key) => Key::new(session_key.public_key().to_owned()).into_message(),
            None => return Err(SignalingError::Crash("Missing session keypair".into())),
        };
        let nonce = Nonce::new(
            self.initiator.cookie_pair().ours.clone(),
            self.identity.into(),
            self.initiator.identity().into(),
            self.initiator.csn_pair().borrow_mut().ours.increment()?,
        );
        let obox = OpenBox::new(msg, nonce);

        // The message SHALL be NaCl public-key encrypted by the client's
        // permanent key pair and the other client's permanent key pair.
        let bbox = obox.encrypt(&self.permanent_key, &self.initiator.permanent_key);

        debug!("Enqueuing key");
        Ok(HandleAction::Reply(bbox))
    }
}


#[cfg(test)]
mod tests {
    use self::cookie::{Cookie, CookiePair};
    use self::csn::{CombinedSequenceSnapshot};
    use self::messages::{ServerHello, ServerAuth};
    use self::types::{Identity};

    use super::*;

    mod validate_nonce {

        use super::*;

        fn create_test_nonce() -> Nonce {
            Nonce::new(
                Cookie::new([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]),
                Address(17),
                Address(18),
                CombinedSequenceSnapshot::new(258, 50_595_078),
            )
        }

        /// A client MUST check that the destination address targets its assigned
        /// identity (or 0x00 during authentication).
        #[test]
        fn first_message_wrong_destination() {
            let ks = KeyStore::new().unwrap();
            let mut s = Signaling::new_initiator(ks);

            let msg = ServerHello::random().into_message();
            let cs = CombinedSequenceSnapshot::random();
            let nonce = Nonce::new(Cookie::random(), Address(0), Address(1), cs);
            let obox = OpenBox::new(msg, nonce);
            let bbox = obox.encode();

            assert_eq!(s.server().handshake_state(), ServerHandshakeState::New);
            assert_eq!(
                s.handle_message(bbox),
                Err(SignalingError::InvalidNonce(
                    "Bad destination: Address(0x01) (our identity is Unknown)".into()
                ))
            );
        }

        /// An initiator SHALL ONLY process messages from the server (0x00). As
        /// soon as the initiator has been assigned an identity, it MAY ALSO accept
        /// messages from other responders (0x02..0xff). Other messages SHALL be
        /// discarded and SHOULD trigger a warning.
        #[test]
        fn wrong_source_initiator() {
            let ks = KeyStore::new().unwrap();
            let mut s = Signaling::new_initiator(ks);

            let make_msg = |src: u8, dest: u8| {
                let msg = ServerHello::random().into_message();
                let cs = CombinedSequenceSnapshot::random();
                let nonce = Nonce::new(Cookie::random(), Address(src), Address(dest), cs);
                let obox = OpenBox::new(msg, nonce);
                let bbox = obox.encode();
                bbox
            };

            // Handling messages from initiator is always invalid (messages are ignored)
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::New);
            let actions = s.handle_message(make_msg(0x01, 0x00)).unwrap();
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::New);
            assert_eq!(actions, vec![]);

            // Handling messages from responder is invalid as long as identity
            // hasn't been assigned (messages are ignored)
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::New);
            let actions = s.handle_message(make_msg(0xff, 0x00)).unwrap();
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::New);
            assert_eq!(actions, vec![]);

            // Handling messages from the server is always valid
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::New);
            let actions = s.handle_message(make_msg(0x00, 0x00)).unwrap();
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::ClientInfoSent);
            // Send only client-auth
            assert_eq!(actions.len(), 1);

            // Handling messages from responder is valid as soon as the identity
            // has been assigned.
            // TODO once state transition has been implemented
    //        s.server_mut().handshake_state = ServerHandshakeState::Done;
    //        s.identity = ClientIdentity::Initiator;
    //        assert_eq!(s.server().handshake_state(), ServerHandshakeState::Done);
    //        let actions = s.handle_message(make_msg(0xff, 0x01)).unwrap();
    //        assert_eq!(s.server().handshake_state(), ServerHandshakeState::Done);
    //        assert_eq!(actions, vec![]);
        }

        /// A responder SHALL ONLY process messages from the server (0x00). As soon
        /// as the responder has been assigned an identity, it MAY ALSO accept
        /// messages from the initiator (0x01). Other messages SHALL be discarded
        /// and SHOULD trigger a warning.
        #[test]
        fn wrong_source_responder() {
            let ks = KeyStore::new().unwrap();
            let initiator_pubkey = PublicKey::from_slice(&[0u8; 32]).unwrap();
            let mut s = Signaling::new_responder(ks, initiator_pubkey, None);

            let make_msg = |src: u8, dest: u8| {
                let msg = ServerHello::random().into_message();
                let cs = CombinedSequenceSnapshot::random();
                let nonce = Nonce::new(Cookie::random(), Address(src), Address(dest), cs);
                let obox = OpenBox::new(msg, nonce);
                let bbox = obox.encode();
                bbox
            };

            // Handling messages from a responder is always invalid (messages are ignored)
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::New);
            let actions = s.handle_message(make_msg(0x03, 0x00)).unwrap();
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::New);
            assert_eq!(actions, vec![]);

            // Handling messages from initiator is invalid as long as identity
            // hasn't been assigned (messages are ignored)
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::New);
            let actions = s.handle_message(make_msg(0x01, 0x00)).unwrap();
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::New);
            assert_eq!(actions, vec![]);

            // Handling messages from the server is always valid
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::New);
            let actions = s.handle_message(make_msg(0x00, 0x00)).unwrap();
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::ClientInfoSent);
            // Send client-hello and client-auth
            assert_eq!(actions.len(), 2);

            // Handling messages from initiator is valid as soon as the identity
            // has been assigned.
            // TODO once state transition has been implemented
    //        s.server_mut().handshake_state = ServerHandshakeState::Done;
    //        s.identity = ClientIdentity::Initiator;
    //        assert_eq!(s.server().handshake_state(), ServerHandshakeState::Done);
    //        let actions = s.handle_message(make_msg(0x01, 0x03)).unwrap();
    //        assert_eq!(s.server().handshake_state(), ServerHandshakeState::Done);
    //        assert_eq!(actions, vec![]);
        }

        /// In case this is the first message received from the sender, the peer
        /// MUST check that the overflow number of the source peer is 0
        #[test]
        fn first_message_bad_overflow_number() {
            let ks = KeyStore::new().unwrap();
            let mut s = Signaling::new_initiator(ks);

            let msg = ServerHello::random().into_message();
            let cs = CombinedSequenceSnapshot::new(1, 1234);
            let nonce = Nonce::new(Cookie::random(), Address(0), Address(0), cs);
            let obox = OpenBox::new(msg, nonce);
            let bbox = obox.encode();

            assert_eq!(s.server().handshake_state(), ServerHandshakeState::New);
            assert_eq!(
                s.handle_message(bbox),
                Err(SignalingError::InvalidNonce(
                    "First message from server must have set the overflow number to 0".into()
                ))
            );
        }

        /// The peer MUST check that the combined sequence number of the source
        /// peer has been increased by 1 and has not reset to 0.
        #[test]
        fn sequence_number_incremented() {
            // TODO: Write once ServerAuth message has been implemented
        }

        /// In case this is the first message received from the sender, the
        /// peer MUST check that the sender's cookie is different than its own
        /// cookie.
        #[test]
        fn cookie_differs_from_own() {
            let ks = KeyStore::new().unwrap();
            let mut s = Signaling::new_initiator(ks);

            let msg = ServerHello::random().into_message();
            let cookie = s.server().cookie_pair.ours.clone();
            let nonce = Nonce::new(cookie, Address(0), Address(0), CombinedSequenceSnapshot::random());
            let obox = OpenBox::new(msg, nonce);
            let bbox = obox.encode();

            assert_eq!(s.server().handshake_state(), ServerHandshakeState::New);
            assert_eq!(
                s.handle_message(bbox),
                Err(SignalingError::InvalidNonce(
                    "Cookie from server is identical to our own cookie".into()
                ))
            );
        }

        /// The peer MUST check that the cookie of the sender does not change.
        #[test]
        fn cookie_did_not_change() {
            // TODO: Write once ServerAuth message has been implemented
        }
    }

    mod signaling_messages {

        use super::*;

        struct TestContext {
            pub our_ks: KeyStore,
            pub server_ks: KeyStore,
            pub our_cookie: Cookie,
            pub server_cookie: Cookie,
            pub signaling: Signaling,
        }

        fn make_test_signaling(role: Role,
                               identity: ClientIdentity,
                               handshake_state: ServerHandshakeState,
                               auth_token: Option<AuthToken>) -> TestContext {
            let our_ks = KeyStore::new().unwrap();
            let server_ks = KeyStore::new().unwrap();
            let our_cookie = Cookie::random();
            let server_cookie = Cookie::random();
            let mut signaling = match role {
                Role::Initiator => Signaling::new_initiator(KeyStore::from_private_key(our_ks.private_key().clone())),
                Role::Responder => {
                    let initiator_pubkey = PublicKey::from_slice(&[0u8; 32]).unwrap();
                    Signaling::new_responder(KeyStore::from_private_key(our_ks.private_key().clone()), initiator_pubkey, auth_token)
                },
            };
            signaling.set_identity(identity);
            signaling.server_mut().set_handshake_state(handshake_state);
            signaling.server_mut().cookie_pair = CookiePair {
                ours: our_cookie.clone(),
                theirs: Some(server_cookie.clone()),
            };
            signaling.server_mut().permanent_key = Some(server_ks.public_key().clone());
            TestContext {
                our_ks: our_ks,
                server_ks: server_ks,
                our_cookie: our_cookie,
                server_cookie: server_cookie,
                signaling: signaling,
            }
        }

        fn make_test_msg(msg: Message, ctx: &TestContext, dest_address: Address) -> ByteBox {
            let nonce = Nonce::new(ctx.server_cookie.clone(), Address(0), dest_address, CombinedSequenceSnapshot::random());
            let obox = OpenBox::new(msg, nonce);
            obox.encrypt(&ctx.server_ks, ctx.our_ks.public_key())
        }

        /// Assert that handling the specified byte box fails in ClientInfoSent
        /// state with the specified error.
        fn assert_client_info_sent_fail(ctx: &mut TestContext, bbox: ByteBox, error: SignalingError) {
            assert_eq!(ctx.signaling.server().handshake_state(), ServerHandshakeState::ClientInfoSent);
            assert_eq!(ctx.signaling.handle_message(bbox), Err(error))
        }

        // When the client receives a 'server-auth' message, it MUST have
        // accepted and set its identity as described in the Receiving a
        // Signalling Message section.
        #[test]
        fn server_auth_no_identity() {
            // Initialize signaling class
            let ctx = make_test_signaling(Role::Responder, ClientIdentity::Unknown,
                                          ServerHandshakeState::ClientInfoSent, None);

            // Prepare a ServerAuth message
            let msg = ServerAuth::for_responder(ctx.our_cookie.clone(), None, false).into_message();
            let bbox = make_test_msg(msg, &ctx, Address(13));

            // Handle message
            let mut s = ctx.signaling;
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::ClientInfoSent);
            let actions = s.handle_message(bbox).unwrap();
            assert_eq!(s.identity(), ClientIdentity::Responder(13));
            assert_eq!(actions, vec![]);
        }

        // The peer MUST check that the cookie provided in the your_cookie
        // field contains the cookie the client has used in its
        // previous and messages to the server.
        #[test]
        fn server_auth_your_cookie() {
            // Initialize signaling class
            let mut ctx = make_test_signaling(Role::Initiator, ClientIdentity::Initiator,
                                              ServerHandshakeState::ClientInfoSent, None);

            // Prepare a ServerAuth message
            let msg = ServerAuth::for_initiator(Cookie::random(), None, vec![]).into_message();
            let bbox = make_test_msg(msg, &ctx, Address(1));

            // Handle message
            assert_client_info_sent_fail(&mut ctx, bbox,
                SignalingError::InvalidMessage(
                    "Cookie sent in server-auth message does not match our cookie".into()));
        }

        #[test]
        fn server_auth_initiator_wrong_fields() {
            // Initialize signaling class
            let mut ctx = make_test_signaling(Role::Initiator, ClientIdentity::Initiator,
                                              ServerHandshakeState::ClientInfoSent, None);

            // Prepare a ServerAuth message
            let msg = ServerAuth::for_responder(ctx.our_cookie.clone(), None, true).into_message();
            let bbox = make_test_msg(msg, &ctx, Address(1));

            // Handle message
            assert_client_info_sent_fail(&mut ctx, bbox,
                SignalingError::InvalidMessage(
                    "We're the initiator, but the `initiator_connected` field in the server-auth message is set".into()));
        }

        #[test]
        fn server_auth_initiator_missing_fields() {
            // Initialize signaling class
            let mut ctx = make_test_signaling(Role::Initiator, ClientIdentity::Initiator,
                                              ServerHandshakeState::ClientInfoSent, None);

            // Prepare a ServerAuth message
            let msg = ServerAuth {
                your_cookie: ctx.our_cookie.clone(),
                signed_keys: None,
                responders: None,
                initiator_connected: None,
            }.into_message();
            let bbox = make_test_msg(msg, &ctx, Address(1));

            // Handle message
            assert_client_info_sent_fail(&mut ctx, bbox,
                SignalingError::InvalidMessage(
                    "`responders` field in server-auth message not set".into()));
        }

        #[test]
        fn server_auth_initiator_duplicate_fields() {
            // Initialize signaling class
            let mut ctx = make_test_signaling(Role::Initiator, ClientIdentity::Initiator,
                                              ServerHandshakeState::ClientInfoSent, None);

            // Prepare a ServerAuth message
            let msg = ServerAuth::for_initiator(ctx.our_cookie.clone(), None, vec![Address(2), Address(3), Address(3)]).into_message();
            let bbox = make_test_msg(msg, &ctx, Address(1));

            // Handle message
            assert_client_info_sent_fail(&mut ctx, bbox,
                SignalingError::InvalidMessage(
                    "`responders` field in server-auth message may not contain duplicates".into()));
        }

        #[test]
        fn server_auth_initiator_invalid_fields() {
            // Initialize signaling class
            let mut ctx = make_test_signaling(Role::Initiator, ClientIdentity::Initiator,
                                              ServerHandshakeState::ClientInfoSent, None);

            // Prepare a ServerAuth message
            let msg = ServerAuth::for_initiator(ctx.our_cookie.clone(), None, vec![Address(1), Address(2), Address(3)]).into_message();
            let bbox = make_test_msg(msg, &ctx, Address(1));

            // Handle message
            assert_client_info_sent_fail(&mut ctx, bbox,
                SignalingError::InvalidMessage(
                    "`responders` field in server-auth message may not contain addresses <0x02".into()));
        }

        /// The client SHOULD store the responder's identities in its internal
        /// list of responders.
        #[test]
        fn server_auth_initiator_stored_responder() {
            // Initialize signaling class
            let ctx = make_test_signaling(Role::Initiator, ClientIdentity::Initiator,
                                          ServerHandshakeState::ClientInfoSent, None);

            // Prepare a ServerAuth message
            let msg = ServerAuth::for_initiator(ctx.our_cookie.clone(), None, vec![Address(2), Address(3)]).into_message();
            let bbox = make_test_msg(msg, &ctx, Address(1));

            // Handle message
            let mut s = ctx.signaling;
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::ClientInfoSent);
            match s {
                Initiator(ref i) => assert_eq!(i.responders.len(), 0),
                Responder(_) => panic!("Invalid inner signaling type"),
            };
            let actions = s.handle_message(bbox).unwrap();
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::Done);
            match s {
                Initiator(ref i) => assert_eq!(i.responders.len(), 2),
                Responder(_) => panic!("Invalid inner signaling type"),
            };
            assert_eq!(actions, vec![]);
        }

        /// The client SHALL check that the initiator_connected field contains
        /// a boolean value.
        #[test]
        fn server_auth_responder_validate_initiator_connected() {
            // Initialize signaling class
            let mut ctx = make_test_signaling(Role::Responder, ClientIdentity::Responder(4),
                                              ServerHandshakeState::ClientInfoSent, None);

            // Prepare a ServerAuth message
            let msg = ServerAuth {
                your_cookie: ctx.our_cookie.clone(),
                signed_keys: None,
                responders: None,
                initiator_connected: None,
            }.into_message();
            let bbox = make_test_msg(msg, &ctx, Address(4));

            // Handle message
            assert_client_info_sent_fail(&mut ctx, bbox,
                SignalingError::InvalidMessage(
                    "We're a responder, but the `initiator_connected` field in the server-auth message is not set".into()));
        }

        /// In case the client is the responder, it SHALL check that the
        /// initiator_connected field contains a boolean value. In case the
        /// field's value is true, the responder MUST proceed with sending a
        /// `token` or `key` client-to-client message described in the
        /// Client-to-Client Messages section.
        fn _server_auth_respond_initiator(mut ctx: TestContext) -> Vec<HandleAction> {
            // Prepare a ServerAuth message
            let msg = ServerAuth {
                your_cookie: ctx.our_cookie.clone(),
                signed_keys: None,
                responders: None,
                initiator_connected: Some(true),
            }.into_message();
            let bbox = make_test_msg(msg, &ctx, Address(7));

            // Signaling ref
            let mut s = ctx.signaling;

            // Handle message
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::ClientInfoSent);
            assert_eq!(s.as_responder().initiator.handshake_state(), InitiatorHandshakeState::New);
            let actions = s.handle_message(bbox).unwrap();
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::Done);
            assert_eq!(s.as_responder().initiator.handshake_state(), InitiatorHandshakeState::KeySent);

            actions
        }

        #[test]
        fn server_auth_respond_initiator_with_token() { // TODO: Add similar test without token
            let mut ctx = make_test_signaling(Role::Responder, ClientIdentity::Responder(7),
                                              ServerHandshakeState::ClientInfoSent, Some(AuthToken::new()));
            let actions = _server_auth_respond_initiator(ctx);
            assert_eq!(actions.len(), 2);
        }

        #[test]
        fn server_auth_respond_initiator_without_token() { // TODO: Add similar test without token
            let mut ctx = make_test_signaling(Role::Responder, ClientIdentity::Responder(7),
                                              ServerHandshakeState::ClientInfoSent, None);
            let actions = _server_auth_respond_initiator(ctx);
            assert_eq!(actions.len(), 1);
        }

        /// If processing the server auth message succeeds, the signaling state
        /// should change to `PeerHandshake`.
        #[test]
        fn server_auth_signaling_state_transition() {
            let mut ctx = make_test_signaling(Role::Responder, ClientIdentity::Responder(7),
                                              ServerHandshakeState::ClientInfoSent, None);

            // Prepare a ServerAuth message
            let msg = ServerAuth {
                your_cookie: ctx.our_cookie.clone(),
                signed_keys: None,
                responders: None,
                initiator_connected: Some(false),
            }.into_message();
            let bbox = make_test_msg(msg, &ctx, Address(7));

            // Signaling ref
            let mut s = ctx.signaling;

            // Handle message
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::ClientInfoSent);
            assert_eq!(s.signaling_state(), SignalingState::ServerHandshake);
            let actions = s.handle_message(bbox).unwrap();
            assert_eq!(s.server().handshake_state(), ServerHandshakeState::Done);
            assert_eq!(s.signaling_state(), SignalingState::PeerHandshake);
        }
    }

    #[test]
    fn server_context_new() {
        let ctx = ServerContext::new();
        assert_eq!(ctx.identity(), Identity::Server);
        assert_eq!(ctx.permanent_key(), None);
        assert_eq!(ctx.session_key(), None);
    }
}
