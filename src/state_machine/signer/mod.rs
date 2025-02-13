use hashbrown::{HashMap, HashSet};
use p256k1::{
    point::{Compressed, Point},
    scalar::Scalar,
};
use rand_core::{CryptoRng, OsRng, RngCore};
use std::collections::BTreeMap;
use tracing::{debug, info, warn};

use crate::{
    common::{PolyCommitment, PublicNonce},
    net::{
        DkgBegin, DkgEnd, DkgPrivateShares, DkgPublicShares, DkgStatus, Message, NonceRequest,
        NonceResponse, Packet, Signable, SignatureShareRequest, SignatureShareResponse,
    },
    state_machine::{PublicKeys, StateMachine},
    traits::Signer as SignerTrait,
    util::{decrypt, encrypt, make_shared_secret},
};

#[derive(Debug, PartialEq)]
/// Signer states
pub enum State {
    /// The signer is idle
    Idle,
    /// The signer is distributing DKG public shares
    DkgPublicDistribute,
    /// The signer is gathering DKG public shares
    DkgPublicGather,
    /// The signer is distributing DKG private shares
    DkgPrivateDistribute,
    /// The signer is gathering DKG private shares
    DkgPrivateGather,
    /// The signer is distributing signature shares
    SignGather,
    /// The signer is finished signing
    Signed,
}

#[derive(thiserror::Error, Debug)]
/// The error type for a signer
pub enum Error {
    /// The party ID was invalid
    #[error("InvalidPartyID")]
    InvalidPartyID,
    /// A DKG public share was invalid
    #[error("InvalidDkgPublicShares")]
    InvalidDkgPublicShares,
    /// A DKG private share was invalid
    #[error("InvalidDkgPrivateShares")]
    InvalidDkgPrivateShares(Vec<u32>),
    /// A nonce response was invalid
    #[error("InvalidNonceResponse")]
    InvalidNonceResponse,
    /// A signature share was invalid
    #[error("InvalidSignatureShare")]
    InvalidSignatureShare,
    /// A bad state change was made
    #[error("Bad State Change: {0}")]
    BadStateChange(String),
}

/// A state machine for a signing round
pub struct SigningRound<Signer: SignerTrait> {
    /// current DKG round ID
    pub dkg_id: u64,
    /// current signing round ID
    pub sign_id: u64,
    /// current signing iteration ID
    pub sign_iter_id: u64,
    /// the threshold of the keys needed for a valid signature
    pub threshold: u32,
    /// the total number of signers
    pub total_signers: u32,
    /// the total number of keys
    pub total_keys: u32,
    /// the Signer object
    pub signer: Signer,
    /// the Signer ID
    pub signer_id: u32,
    /// the current state
    pub state: State,
    /// map of party_id to the polynomial commitment for that party
    pub commitments: BTreeMap<u32, PolyCommitment>,
    /// map of decrypted DKG private shares
    pub decrypted_shares: HashMap<u32, HashMap<u32, Scalar>>,
    /// invalid private shares
    pub invalid_private_shares: Vec<u32>,
    /// public nonces for this signing round
    pub public_nonces: Vec<PublicNonce>,
    /// the private key used to sign messages sent over the network
    pub network_private_key: Scalar,
    /// the public keys for all signers and coordinator
    pub public_keys: PublicKeys,
}

impl<Signer: SignerTrait> SigningRound<Signer> {
    /// create a SigningRound
    pub fn new(
        threshold: u32,
        total_signers: u32,
        total_keys: u32,
        signer_id: u32,
        key_ids: Vec<u32>,
        network_private_key: Scalar,
        public_keys: PublicKeys,
    ) -> Self {
        assert!(threshold <= total_keys);
        let mut rng = OsRng;
        let signer = Signer::new(
            signer_id,
            &key_ids,
            total_signers,
            total_keys,
            threshold,
            &mut rng,
        );
        debug!(
            "new SigningRound for signer_id {} with key_ids {:?}",
            signer_id, &key_ids
        );
        SigningRound {
            dkg_id: 0,
            sign_id: 1,
            sign_iter_id: 1,
            threshold,
            total_signers,
            total_keys,
            signer,
            signer_id,
            state: State::Idle,
            commitments: BTreeMap::new(),
            decrypted_shares: HashMap::new(),
            invalid_private_shares: Vec::new(),
            public_nonces: vec![],
            network_private_key,
            public_keys,
        }
    }

    fn reset<T: RngCore + CryptoRng>(&mut self, dkg_id: u64, rng: &mut T) {
        self.dkg_id = dkg_id;
        self.commitments.clear();
        self.decrypted_shares.clear();
        self.invalid_private_shares.clear();
        self.public_nonces.clear();
        self.signer.reset_polys(rng);
    }

    ///
    pub fn process_inbound_messages(&mut self, messages: &[Packet]) -> Result<Vec<Packet>, Error> {
        let mut responses = vec![];
        for message in messages {
            // TODO: this code was swiped from frost-signer. Expose it there so we don't have duplicate code
            // See: https://github.com/stacks-network/stacks-blockchain/issues/3913
            let outbounds = self.process(&message.msg)?;
            for out in outbounds {
                let msg = Packet {
                    sig: match &out {
                        Message::DkgBegin(msg) | Message::DkgPrivateBegin(msg) => msg
                            .sign(&self.network_private_key)
                            .expect("failed to sign DkgBegin")
                            .to_vec(),
                        Message::DkgEnd(msg) => msg
                            .sign(&self.network_private_key)
                            .expect("failed to sign DkgEnd")
                            .to_vec(),
                        Message::DkgPublicShares(msg) => msg
                            .sign(&self.network_private_key)
                            .expect("failed to sign DkgPublicShares")
                            .to_vec(),
                        Message::DkgPrivateShares(msg) => msg
                            .sign(&self.network_private_key)
                            .expect("failed to sign DkgPrivateShare")
                            .to_vec(),
                        Message::NonceRequest(msg) => msg
                            .sign(&self.network_private_key)
                            .expect("failed to sign NonceRequest")
                            .to_vec(),
                        Message::NonceResponse(msg) => msg
                            .sign(&self.network_private_key)
                            .expect("failed to sign NonceResponse")
                            .to_vec(),
                        Message::SignatureShareRequest(msg) => msg
                            .sign(&self.network_private_key)
                            .expect("failed to sign SignShareRequest")
                            .to_vec(),
                        Message::SignatureShareResponse(msg) => msg
                            .sign(&self.network_private_key)
                            .expect("failed to sign SignShareResponse")
                            .to_vec(),
                    },
                    msg: out,
                };
                responses.push(msg);
            }
        }
        Ok(responses)
    }

    /// process the passed incoming message, and return any outgoing messages needed in response
    pub fn process(&mut self, message: &Message) -> Result<Vec<Message>, Error> {
        let out_msgs = match message {
            Message::DkgBegin(dkg_begin) => self.dkg_begin(dkg_begin),
            Message::DkgPrivateBegin(_) => self.dkg_private_begin(),
            Message::DkgPublicShares(dkg_public_shares) => self.dkg_public_share(dkg_public_shares),
            Message::DkgPrivateShares(dkg_private_shares) => {
                self.dkg_private_shares(dkg_private_shares)
            }
            Message::SignatureShareRequest(sign_share_request) => {
                self.sign_share_request(sign_share_request)
            }
            Message::NonceRequest(nonce_request) => self.nonce_request(nonce_request),
            _ => Ok(vec![]), // TODO
        };

        match out_msgs {
            Ok(mut out) => {
                if self.public_shares_done() {
                    debug!(
                        "public_shares_done==true. commitments {}",
                        self.commitments.len()
                    );
                    self.move_to(State::DkgPrivateDistribute)?;
                } else if self.can_dkg_end() {
                    debug!(
                        "can_dkg_end==true. shares {} commitments {}",
                        self.decrypted_shares.len(),
                        self.commitments.len()
                    );
                    let dkg_end_msgs = self.dkg_ended()?;
                    out.push(dkg_end_msgs);
                    self.move_to(State::Idle)?;
                }
                Ok(out)
            }
            Err(e) => Err(e),
        }
    }

    /// DKG is done so compute secrets
    pub fn dkg_ended(&mut self) -> Result<Message, Error> {
        let polys: Vec<PolyCommitment> = self.commitments.clone().into_values().collect();

        let dkg_end = if self.invalid_private_shares.is_empty() {
            match self.signer.compute_secrets(&self.decrypted_shares, &polys) {
                Ok(()) => DkgEnd {
                    dkg_id: self.dkg_id,
                    signer_id: self.signer_id,
                    status: DkgStatus::Success,
                },
                Err(dkg_error_map) => DkgEnd {
                    dkg_id: self.dkg_id,
                    signer_id: self.signer_id,
                    status: DkgStatus::Failure(format!("{:?}", dkg_error_map)),
                },
            }
        } else {
            DkgEnd {
                dkg_id: self.dkg_id,
                signer_id: self.signer_id,
                status: DkgStatus::Failure(format!("{:?}", self.invalid_private_shares)),
            }
        };

        info!(
            "Signer {} sending DkgEnd round {} status {:?}",
            self.signer_id, self.dkg_id, dkg_end.status,
        );

        let dkg_end = Message::DkgEnd(dkg_end);
        Ok(dkg_end)
    }

    /// do we have all DkgPublicShares?
    pub fn public_shares_done(&self) -> bool {
        debug!(
            "public_shares_done state {:?} commitments {}",
            self.state,
            self.commitments.len(),
        );
        self.state == State::DkgPublicGather
            && self.commitments.len() == usize::try_from(self.signer.get_num_parties()).unwrap()
    }

    /// do we have all DkgPublicShares and DkgPrivateShares?
    pub fn can_dkg_end(&self) -> bool {
        debug!(
            "can_dkg_end state {:?} commitments {} shares {}",
            self.state,
            self.commitments.len(),
            self.decrypted_shares.len()
        );
        self.state == State::DkgPrivateGather
            && self.commitments.len() == usize::try_from(self.signer.get_num_parties()).unwrap()
            && self.decrypted_shares.len()
                == usize::try_from(self.signer.get_num_parties()).unwrap()
    }

    fn nonce_request(&mut self, nonce_request: &NonceRequest) -> Result<Vec<Message>, Error> {
        let mut rng = OsRng;
        let mut msgs = vec![];
        let signer_id = self.signer_id;
        let key_ids = self.signer.get_key_ids();
        let nonces = self.signer.gen_nonces(&mut rng);

        let response = NonceResponse {
            dkg_id: nonce_request.dkg_id,
            sign_id: nonce_request.sign_id,
            sign_iter_id: nonce_request.sign_iter_id,
            signer_id,
            key_ids,
            nonces,
        };

        let response = Message::NonceResponse(response);

        info!(
            "Signer {} sending NonceResponse for DKG round {} sign round {} sign iteration {}",
            signer_id, nonce_request.dkg_id, nonce_request.sign_id, nonce_request.sign_iter_id,
        );
        msgs.push(response);

        Ok(msgs)
    }

    fn sign_share_request(
        &mut self,
        sign_request: &SignatureShareRequest,
    ) -> Result<Vec<Message>, Error> {
        let mut msgs = vec![];

        let signer_ids = sign_request
            .nonce_responses
            .iter()
            .map(|nr| nr.signer_id)
            .collect::<Vec<u32>>();

        debug!("Got SignatureShareRequest for signer_ids {:?}", signer_ids);

        for signer_id in &signer_ids {
            if *signer_id == self.signer_id {
                let key_ids: Vec<u32> = sign_request
                    .nonce_responses
                    .iter()
                    .flat_map(|nr| nr.key_ids.iter().copied())
                    .collect::<Vec<u32>>();
                let nonces = sign_request
                    .nonce_responses
                    .iter()
                    .flat_map(|nr| nr.nonces.clone())
                    .collect::<Vec<PublicNonce>>();
                let signature_shares = if sign_request.is_taproot {
                    self.signer.sign_taproot(
                        &sign_request.message,
                        &signer_ids,
                        &key_ids,
                        &nonces,
                        sign_request.merkle_root,
                    )
                } else {
                    self.signer
                        .sign(&sign_request.message, &signer_ids, &key_ids, &nonces)
                };

                let response = SignatureShareResponse {
                    dkg_id: sign_request.dkg_id,
                    sign_id: sign_request.sign_id,
                    sign_iter_id: sign_request.sign_iter_id,
                    signer_id: *signer_id,
                    signature_shares,
                };

                info!(
                    "Signer {} sending SignatureShareResponse for DKG round {} sign round {} sign iteration {}",
                    signer_id, self.dkg_id, self.sign_id, self.sign_iter_id,
                );

                let response = Message::SignatureShareResponse(response);

                msgs.push(response);
            } else {
                debug!("SignatureShareRequest for {} dropped.", signer_id);
            }
        }
        Ok(msgs)
    }

    fn dkg_begin(&mut self, dkg_begin: &DkgBegin) -> Result<Vec<Message>, Error> {
        let mut rng = OsRng;

        self.reset(dkg_begin.dkg_id, &mut rng);
        self.move_to(State::DkgPublicDistribute)?;

        //let _party_state = self.signer.save();

        self.dkg_public_begin()
    }

    fn dkg_public_begin(&mut self) -> Result<Vec<Message>, Error> {
        let mut rng = OsRng;
        let mut msgs = vec![];
        let comms = self.signer.get_poly_commitments(&mut rng);

        info!(
            "Signer {} sending DkgPublicShares for round {}",
            self.signer.get_id(),
            self.dkg_id,
        );

        let mut public_share = DkgPublicShares {
            dkg_id: self.dkg_id,
            signer_id: self.signer_id,
            comms: Vec::new(),
        };

        for poly in &comms {
            public_share
                .comms
                .push((poly.id.id.get_u32(), poly.clone()));
        }

        let public_share = Message::DkgPublicShares(public_share);
        msgs.push(public_share);

        self.move_to(State::DkgPublicGather)?;
        Ok(msgs)
    }

    fn dkg_private_begin(&mut self) -> Result<Vec<Message>, Error> {
        let mut rng = OsRng;
        let mut msgs = vec![];
        let mut private_shares = DkgPrivateShares {
            dkg_id: self.dkg_id,
            signer_id: self.signer_id,
            shares: Vec::new(),
        };
        info!(
            "Signer {} sending DkgPrivateShares for round {}",
            self.signer.get_id(),
            self.dkg_id,
        );

        debug!(
            "Signer {} shares {:?}",
            self.signer_id,
            &self.signer.get_shares()
        );
        for (key_id, shares) in &self.signer.get_shares() {
            debug!(
                "Signer {} addding dkg private share for key_id {}",
                self.signer_id, key_id
            );
            // encrypt each share for the recipient
            let mut encrypted_shares = HashMap::new();

            for (dst_key_id, private_share) in shares {
                debug!("encrypting dkg private share for key_id {}", dst_key_id + 1);
                let compressed =
                    Compressed::from(self.public_keys.key_ids[&(dst_key_id + 1)].to_bytes());
                let dst_public_key = Point::try_from(&compressed).unwrap();
                let shared_secret = make_shared_secret(&self.network_private_key, &dst_public_key);
                let encrypted_share =
                    encrypt(&shared_secret, &private_share.to_bytes(), &mut rng).unwrap();

                encrypted_shares.insert(*dst_key_id, encrypted_share);
            }

            private_shares.shares.push((*key_id, encrypted_shares));
        }

        let private_shares = Message::DkgPrivateShares(private_shares);
        msgs.push(private_shares);

        self.move_to(State::DkgPrivateGather)?;
        Ok(msgs)
    }

    /// handle incoming DkgPublicShares
    pub fn dkg_public_share(
        &mut self,
        dkg_public_shares: &DkgPublicShares,
    ) -> Result<Vec<Message>, Error> {
        for (party_id, comm) in &dkg_public_shares.comms {
            self.commitments.insert(*party_id, comm.clone());
        }
        debug!(
            "received DkgPublicShares from signer {} {}/{}",
            dkg_public_shares.signer_id,
            self.commitments.len(),
            self.signer.get_num_parties(),
        );
        Ok(vec![])
    }

    /// handle incoming DkgPrivateShares
    pub fn dkg_private_shares(
        &mut self,
        dkg_private_shares: &DkgPrivateShares,
    ) -> Result<Vec<Message>, Error> {
        // go ahead and decrypt here, since we know the signer_id and hence the pubkey of the sender

        // make a HashSet of our key_ids so we can quickly query them
        let key_ids: HashSet<u32> = self.signer.get_key_ids().into_iter().collect();
        let compressed =
            Compressed::from(self.public_keys.signers[&dkg_private_shares.signer_id].to_bytes());
        let public_key = Point::try_from(&compressed).unwrap();
        let shared_secret = make_shared_secret(&self.network_private_key, &public_key);

        for (src_id, shares) in &dkg_private_shares.shares {
            let mut decrypted_shares = HashMap::new();
            for (dst_key_id, bytes) in shares {
                if key_ids.contains(dst_key_id) {
                    match decrypt(&shared_secret, bytes) {
                        Ok(plain) => match Scalar::try_from(&plain[..]) {
                            Ok(s) => {
                                decrypted_shares.insert(*dst_key_id, s);
                            }
                            Err(e) => {
                                warn!("Failed to parse Scalar for dkg private share from src_id {} to dst_id {}: {:?}", src_id, dst_key_id, e);
                                self.invalid_private_shares.push(*src_id);
                            }
                        },
                        Err(e) => {
                            warn!("Failed to decrypt dkg private share from src_id {} to dst_id {}: {:?}", src_id, dst_key_id, e);
                            self.invalid_private_shares.push(*src_id);
                        }
                    }
                }
            }
            self.decrypted_shares.insert(*src_id, decrypted_shares);
        }
        debug!(
            "received DkgPrivateShares from signer {} {}/{}",
            dkg_private_shares.signer_id,
            self.decrypted_shares.len(),
            self.signer.get_num_parties(),
        );
        Ok(vec![])
    }
}

impl<Signer: SignerTrait> StateMachine<State, Error> for SigningRound<Signer> {
    fn move_to(&mut self, state: State) -> Result<(), Error> {
        self.can_move_to(&state)?;
        self.state = state;
        Ok(())
    }

    fn can_move_to(&self, state: &State) -> Result<(), Error> {
        let prev_state = &self.state;
        let accepted = match state {
            State::Idle => true,
            State::DkgPublicDistribute => {
                prev_state == &State::Idle
                    || prev_state == &State::DkgPublicGather
                    || prev_state == &State::DkgPrivateDistribute
            }
            State::DkgPublicGather => prev_state == &State::DkgPublicDistribute,
            State::DkgPrivateDistribute => prev_state == &State::DkgPublicGather,
            State::DkgPrivateGather => prev_state == &State::DkgPrivateDistribute,
            State::SignGather => prev_state == &State::Idle,
            State::Signed => prev_state == &State::SignGather,
        };
        if accepted {
            debug!("state change from {:?} to {:?}", prev_state, state);
            Ok(())
        } else {
            Err(Error::BadStateChange(format!(
                "{:?} to {:?}",
                prev_state, state
            )))
        }
    }
}
