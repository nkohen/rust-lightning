//! Various utilities for building scripts and deriving keys related to channels. These are
//! largely of interest for those implementing chain::keysinterface::ChannelKeys message signing
//! by hand.

use bitcoin::blockdata::script::{Script,Builder};
use bitcoin::blockdata::opcodes;
use bitcoin::blockdata::transaction::{TxIn,TxOut,OutPoint,Transaction, SigHashType};
use bitcoin::consensus::encode::{self, Decodable, Encodable};
use bitcoin::util::bip143;

use bitcoin::hashes::{Hash, HashEngine};
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::hashes::ripemd160::Hash as Ripemd160;
use bitcoin::hash_types::{Txid, PubkeyHash};

use ln::channelmanager::{PaymentHash, PaymentPreimage};
use ln::msgs::DecodeError;
use util::ser::{Readable, Writeable, Writer, WriterWriteAdaptor};
use util::byte_utils;

use bitcoin::secp256k1::key::{SecretKey, PublicKey};
use bitcoin::secp256k1::{Secp256k1, Signature};
use bitcoin::secp256k1;

use std::{cmp, mem};

const MAX_ALLOC_SIZE: usize = 64*1024;

pub(super) const HTLC_SUCCESS_TX_WEIGHT: u64 = 703;
pub(super) const HTLC_TIMEOUT_TX_WEIGHT: u64 = 663;

#[derive(PartialEq)]
pub(crate) enum HTLCType {
	AcceptedHTLC,
	OfferedHTLC
}

impl HTLCType {
	/// Check if a given tx witnessScript len matchs one of a pre-signed HTLC
	pub(crate) fn scriptlen_to_htlctype(witness_script_len: usize) ->  Option<HTLCType> {
		if witness_script_len == 133 {
			Some(HTLCType::OfferedHTLC)
		} else if witness_script_len >= 136 && witness_script_len <= 139 {
			Some(HTLCType::AcceptedHTLC)
		} else {
			None
		}
	}
}

// Various functions for key derivation and transaction creation for use within channels. Primarily
// used in Channel and ChannelMonitor.

pub(super) fn build_commitment_secret(commitment_seed: &[u8; 32], idx: u64) -> [u8; 32] {
	let mut res: [u8; 32] = commitment_seed.clone();
	for i in 0..48 {
		let bitpos = 47 - i;
		if idx & (1 << bitpos) == (1 << bitpos) {
			res[bitpos / 8] ^= 1 << (bitpos & 7);
			res = Sha256::hash(&res).into_inner();
		}
	}
	res
}

/// Implements the per-commitment secret storage scheme from
/// [BOLT 3](https://github.com/lightningnetwork/lightning-rfc/blob/dcbf8583976df087c79c3ce0b535311212e6812d/03-transactions.md#efficient-per-commitment-secret-storage).
///
/// Allows us to keep track of all of the revocation secrets of counterarties in just 50*32 bytes
/// or so.
#[derive(Clone)]
pub(super) struct CounterpartyCommitmentSecrets {
	old_secrets: [([u8; 32], u64); 49],
}

impl PartialEq for CounterpartyCommitmentSecrets {
	fn eq(&self, other: &Self) -> bool {
		for (&(ref secret, ref idx), &(ref o_secret, ref o_idx)) in self.old_secrets.iter().zip(other.old_secrets.iter()) {
			if secret != o_secret || idx != o_idx {
				return false
			}
		}
		true
	}
}

impl CounterpartyCommitmentSecrets {
	pub(super) fn new() -> Self {
		Self { old_secrets: [([0; 32], 1 << 48); 49], }
	}

	#[inline]
	fn place_secret(idx: u64) -> u8 {
		for i in 0..48 {
			if idx & (1 << i) == (1 << i) {
				return i
			}
		}
		48
	}

	pub(super) fn get_min_seen_secret(&self) -> u64 {
		//TODO This can be optimized?
		let mut min = 1 << 48;
		for &(_, idx) in self.old_secrets.iter() {
			if idx < min {
				min = idx;
			}
		}
		min
	}

	#[inline]
	pub(super) fn derive_secret(secret: [u8; 32], bits: u8, idx: u64) -> [u8; 32] {
		let mut res: [u8; 32] = secret;
		for i in 0..bits {
			let bitpos = bits - 1 - i;
			if idx & (1 << bitpos) == (1 << bitpos) {
				res[(bitpos / 8) as usize] ^= 1 << (bitpos & 7);
				res = Sha256::hash(&res).into_inner();
			}
		}
		res
	}

	pub(super) fn provide_secret(&mut self, idx: u64, secret: [u8; 32]) -> Result<(), ()> {
		let pos = Self::place_secret(idx);
		for i in 0..pos {
			let (old_secret, old_idx) = self.old_secrets[i as usize];
			if Self::derive_secret(secret, pos, old_idx) != old_secret {
				return Err(());
			}
		}
		if self.get_min_seen_secret() <= idx {
			return Ok(());
		}
		self.old_secrets[pos as usize] = (secret, idx);
		Ok(())
	}

	/// Can only fail if idx is < get_min_seen_secret
	pub(super) fn get_secret(&self, idx: u64) -> Option<[u8; 32]> {
		for i in 0..self.old_secrets.len() {
			if (idx & (!((1 << i) - 1))) == self.old_secrets[i].1 {
				return Some(Self::derive_secret(self.old_secrets[i].0, i as u8, idx))
			}
		}
		assert!(idx < self.get_min_seen_secret());
		None
	}
}

impl Writeable for CounterpartyCommitmentSecrets {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		for &(ref secret, ref idx) in self.old_secrets.iter() {
			writer.write_all(secret)?;
			writer.write_all(&byte_utils::be64_to_array(*idx))?;
		}
		Ok(())
	}
}
impl Readable for CounterpartyCommitmentSecrets {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let mut old_secrets = [([0; 32], 1 << 48); 49];
		for &mut (ref mut secret, ref mut idx) in old_secrets.iter_mut() {
			*secret = Readable::read(reader)?;
			*idx = Readable::read(reader)?;
		}

		Ok(Self { old_secrets })
	}
}

/// Derives a per-commitment-transaction private key (eg an htlc key or payment key) from the base
/// private key for that type of key and the per_commitment_point (available in TxCreationKeys)
pub fn derive_private_key<T: secp256k1::Signing>(secp_ctx: &Secp256k1<T>, per_commitment_point: &PublicKey, base_secret: &SecretKey) -> Result<SecretKey, secp256k1::Error> {
	let mut sha = Sha256::engine();
	sha.input(&per_commitment_point.serialize());
	sha.input(&PublicKey::from_secret_key(&secp_ctx, &base_secret).serialize());
	let res = Sha256::from_engine(sha).into_inner();

	let mut key = base_secret.clone();
	key.add_assign(&res)?;
	Ok(key)
}

pub(super) fn derive_public_key<T: secp256k1::Signing>(secp_ctx: &Secp256k1<T>, per_commitment_point: &PublicKey, base_point: &PublicKey) -> Result<PublicKey, secp256k1::Error> {
	let mut sha = Sha256::engine();
	sha.input(&per_commitment_point.serialize());
	sha.input(&base_point.serialize());
	let res = Sha256::from_engine(sha).into_inner();

	let hashkey = PublicKey::from_secret_key(&secp_ctx, &SecretKey::from_slice(&res)?);
	base_point.combine(&hashkey)
}

/// Derives a revocation key from its constituent parts.
/// Note that this is infallible iff we trust that at least one of the two input keys are randomly
/// generated (ie our own).
pub(super) fn derive_private_revocation_key<T: secp256k1::Signing>(secp_ctx: &Secp256k1<T>, per_commitment_secret: &SecretKey, revocation_base_secret: &SecretKey) -> Result<SecretKey, secp256k1::Error> {
	let revocation_base_point = PublicKey::from_secret_key(&secp_ctx, &revocation_base_secret);
	let per_commitment_point = PublicKey::from_secret_key(&secp_ctx, &per_commitment_secret);

	let rev_append_commit_hash_key = {
		let mut sha = Sha256::engine();
		sha.input(&revocation_base_point.serialize());
		sha.input(&per_commitment_point.serialize());

		Sha256::from_engine(sha).into_inner()
	};
	let commit_append_rev_hash_key = {
		let mut sha = Sha256::engine();
		sha.input(&per_commitment_point.serialize());
		sha.input(&revocation_base_point.serialize());

		Sha256::from_engine(sha).into_inner()
	};

	let mut part_a = revocation_base_secret.clone();
	part_a.mul_assign(&rev_append_commit_hash_key)?;
	let mut part_b = per_commitment_secret.clone();
	part_b.mul_assign(&commit_append_rev_hash_key)?;
	part_a.add_assign(&part_b[..])?;
	Ok(part_a)
}

pub(super) fn derive_public_revocation_key<T: secp256k1::Verification>(secp_ctx: &Secp256k1<T>, per_commitment_point: &PublicKey, revocation_base_point: &PublicKey) -> Result<PublicKey, secp256k1::Error> {
	let rev_append_commit_hash_key = {
		let mut sha = Sha256::engine();
		sha.input(&revocation_base_point.serialize());
		sha.input(&per_commitment_point.serialize());

		Sha256::from_engine(sha).into_inner()
	};
	let commit_append_rev_hash_key = {
		let mut sha = Sha256::engine();
		sha.input(&per_commitment_point.serialize());
		sha.input(&revocation_base_point.serialize());

		Sha256::from_engine(sha).into_inner()
	};

	let mut part_a = revocation_base_point.clone();
	part_a.mul_assign(&secp_ctx, &rev_append_commit_hash_key)?;
	let mut part_b = per_commitment_point.clone();
	part_b.mul_assign(&secp_ctx, &commit_append_rev_hash_key)?;
	part_a.combine(&part_b)
}

/// The set of public keys which are used in the creation of one commitment transaction.
/// These are derived from the channel base keys and per-commitment data.
#[derive(PartialEq, Clone)]
pub struct TxCreationKeys {
	/// The per-commitment public key which was used to derive the other keys.
	pub per_commitment_point: PublicKey,
	/// The revocation key which is used to allow the owner of the commitment transaction to
	/// provide their counterparty the ability to punish them if they broadcast an old state.
	pub(crate) revocation_key: PublicKey,
	/// A's HTLC Key
	pub(crate) a_htlc_key: PublicKey,
	/// B's HTLC Key
	pub(crate) b_htlc_key: PublicKey,
	/// A's Payment Key (which isn't allowed to be spent from for some delay)
	pub(crate) a_delayed_payment_key: PublicKey,
}
impl_writeable!(TxCreationKeys, 33*6,
	{ per_commitment_point, revocation_key, a_htlc_key, b_htlc_key, a_delayed_payment_key });

/// One counterparty's public keys which do not change over the life of a channel.
#[derive(Clone, PartialEq)]
pub struct ChannelPublicKeys {
	/// The public key which is used to sign all commitment transactions, as it appears in the
	/// on-chain channel lock-in 2-of-2 multisig output.
	pub funding_pubkey: PublicKey,
	/// The base point which is used (with derive_public_revocation_key) to derive per-commitment
	/// revocation keys. The per-commitment revocation private key is then revealed by the owner of
	/// a commitment transaction so that their counterparty can claim all available funds if they
	/// broadcast an old state.
	pub revocation_basepoint: PublicKey,
	/// The public key which receives our immediately spendable primary channel balance in
	/// remote-broadcasted commitment transactions. This key is static across every commitment
	/// transaction.
	pub payment_point: PublicKey,
	/// The base point which is used (with derive_public_key) to derive a per-commitment payment
	/// public key which receives non-HTLC-encumbered funds which are only available for spending
	/// after some delay (or can be claimed via the revocation path).
	pub delayed_payment_basepoint: PublicKey,
	/// The base point which is used (with derive_public_key) to derive a per-commitment public key
	/// which is used to encumber HTLC-in-flight outputs.
	pub htlc_basepoint: PublicKey,
}

impl_writeable!(ChannelPublicKeys, 33*5, {
	funding_pubkey,
	revocation_basepoint,
	payment_point,
	delayed_payment_basepoint,
	htlc_basepoint
});


impl TxCreationKeys {
	pub(crate) fn new<T: secp256k1::Signing + secp256k1::Verification>(secp_ctx: &Secp256k1<T>, per_commitment_point: &PublicKey, a_delayed_payment_base: &PublicKey, a_htlc_base: &PublicKey, b_revocation_base: &PublicKey, b_htlc_base: &PublicKey) -> Result<TxCreationKeys, secp256k1::Error> {
		Ok(TxCreationKeys {
			per_commitment_point: per_commitment_point.clone(),
			revocation_key: derive_public_revocation_key(&secp_ctx, &per_commitment_point, &b_revocation_base)?,
			a_htlc_key: derive_public_key(&secp_ctx, &per_commitment_point, &a_htlc_base)?,
			b_htlc_key: derive_public_key(&secp_ctx, &per_commitment_point, &b_htlc_base)?,
			a_delayed_payment_key: derive_public_key(&secp_ctx, &per_commitment_point, &a_delayed_payment_base)?,
		})
	}
}

/// Gets the "to_local" output redeemscript, ie the script which is time-locked or spendable by
/// the revocation key
pub(super) fn get_revokeable_redeemscript(revocation_key: &PublicKey, to_self_delay: u16, delayed_payment_key: &PublicKey) -> Script {
	Builder::new().push_opcode(opcodes::all::OP_IF)
	              .push_slice(&revocation_key.serialize())
	              .push_opcode(opcodes::all::OP_ELSE)
	              .push_int(to_self_delay as i64)
	              .push_opcode(opcodes::all::OP_CSV)
	              .push_opcode(opcodes::all::OP_DROP)
	              .push_slice(&delayed_payment_key.serialize())
	              .push_opcode(opcodes::all::OP_ENDIF)
	              .push_opcode(opcodes::all::OP_CHECKSIG)
	              .into_script()
}

#[derive(Clone, PartialEq)]
/// Information about an HTLC as it appears in a commitment transaction
pub struct HTLCOutputInCommitment {
	/// Whether the HTLC was "offered" (ie outbound in relation to this commitment transaction).
	/// Note that this is not the same as whether it is ountbound *from us*. To determine that you
	/// need to compare this value to whether the commitment transaction in question is that of
	/// the remote party or our own.
	pub offered: bool,
	/// The value, in msat, of the HTLC. The value as it appears in the commitment transaction is
	/// this divided by 1000.
	pub amount_msat: u64,
	/// The CLTV lock-time at which this HTLC expires.
	pub cltv_expiry: u32,
	/// The hash of the preimage which unlocks this HTLC.
	pub payment_hash: PaymentHash,
	/// The position within the commitment transactions' outputs. This may be None if the value is
	/// below the dust limit (in which case no output appears in the commitment transaction and the
	/// value is spent to additional transaction fees).
	pub transaction_output_index: Option<u32>,
}

impl_writeable!(HTLCOutputInCommitment, 1 + 8 + 4 + 32 + 5, {
	offered,
	amount_msat,
	cltv_expiry,
	payment_hash,
	transaction_output_index
});

#[inline]
pub(crate) fn get_htlc_redeemscript_with_explicit_keys(htlc: &HTLCOutputInCommitment, a_htlc_key: &PublicKey, b_htlc_key: &PublicKey, revocation_key: &PublicKey) -> Script {
	let payment_hash160 = Ripemd160::hash(&htlc.payment_hash.0[..]).into_inner();
	if htlc.offered {
		Builder::new().push_opcode(opcodes::all::OP_DUP)
		              .push_opcode(opcodes::all::OP_HASH160)
		              .push_slice(&PubkeyHash::hash(&revocation_key.serialize())[..])
		              .push_opcode(opcodes::all::OP_EQUAL)
		              .push_opcode(opcodes::all::OP_IF)
		              .push_opcode(opcodes::all::OP_CHECKSIG)
		              .push_opcode(opcodes::all::OP_ELSE)
		              .push_slice(&b_htlc_key.serialize()[..])
		              .push_opcode(opcodes::all::OP_SWAP)
		              .push_opcode(opcodes::all::OP_SIZE)
		              .push_int(32)
		              .push_opcode(opcodes::all::OP_EQUAL)
		              .push_opcode(opcodes::all::OP_NOTIF)
		              .push_opcode(opcodes::all::OP_DROP)
		              .push_int(2)
		              .push_opcode(opcodes::all::OP_SWAP)
		              .push_slice(&a_htlc_key.serialize()[..])
		              .push_int(2)
		              .push_opcode(opcodes::all::OP_CHECKMULTISIG)
		              .push_opcode(opcodes::all::OP_ELSE)
		              .push_opcode(opcodes::all::OP_HASH160)
		              .push_slice(&payment_hash160)
		              .push_opcode(opcodes::all::OP_EQUALVERIFY)
		              .push_opcode(opcodes::all::OP_CHECKSIG)
		              .push_opcode(opcodes::all::OP_ENDIF)
		              .push_opcode(opcodes::all::OP_ENDIF)
		              .into_script()
	} else {
		Builder::new().push_opcode(opcodes::all::OP_DUP)
		              .push_opcode(opcodes::all::OP_HASH160)
		              .push_slice(&PubkeyHash::hash(&revocation_key.serialize())[..])
		              .push_opcode(opcodes::all::OP_EQUAL)
		              .push_opcode(opcodes::all::OP_IF)
		              .push_opcode(opcodes::all::OP_CHECKSIG)
		              .push_opcode(opcodes::all::OP_ELSE)
		              .push_slice(&b_htlc_key.serialize()[..])
		              .push_opcode(opcodes::all::OP_SWAP)
		              .push_opcode(opcodes::all::OP_SIZE)
		              .push_int(32)
		              .push_opcode(opcodes::all::OP_EQUAL)
		              .push_opcode(opcodes::all::OP_IF)
		              .push_opcode(opcodes::all::OP_HASH160)
		              .push_slice(&payment_hash160)
		              .push_opcode(opcodes::all::OP_EQUALVERIFY)
		              .push_int(2)
		              .push_opcode(opcodes::all::OP_SWAP)
		              .push_slice(&a_htlc_key.serialize()[..])
		              .push_int(2)
		              .push_opcode(opcodes::all::OP_CHECKMULTISIG)
		              .push_opcode(opcodes::all::OP_ELSE)
		              .push_opcode(opcodes::all::OP_DROP)
		              .push_int(htlc.cltv_expiry as i64)
		              .push_opcode(opcodes::all::OP_CLTV)
		              .push_opcode(opcodes::all::OP_DROP)
		              .push_opcode(opcodes::all::OP_CHECKSIG)
		              .push_opcode(opcodes::all::OP_ENDIF)
		              .push_opcode(opcodes::all::OP_ENDIF)
		              .into_script()
	}
}

/// note here that 'a_revocation_key' is generated using b_revocation_basepoint and a's
/// commitment secret. 'htlc' does *not* need to have its previous_output_index filled.
#[inline]
pub fn get_htlc_redeemscript(htlc: &HTLCOutputInCommitment, keys: &TxCreationKeys) -> Script {
	get_htlc_redeemscript_with_explicit_keys(htlc, &keys.a_htlc_key, &keys.b_htlc_key, &keys.revocation_key)
}

/// Gets the redeemscript for a funding output from the two funding public keys.
/// Note that the order of funding public keys does not matter.
pub fn make_funding_redeemscript(a: &PublicKey, b: &PublicKey) -> Script {
	let our_funding_key = a.serialize();
	let their_funding_key = b.serialize();

	let builder = Builder::new().push_opcode(opcodes::all::OP_PUSHNUM_2);
	if our_funding_key[..] < their_funding_key[..] {
		builder.push_slice(&our_funding_key)
			.push_slice(&their_funding_key)
	} else {
		builder.push_slice(&their_funding_key)
			.push_slice(&our_funding_key)
	}.push_opcode(opcodes::all::OP_PUSHNUM_2).push_opcode(opcodes::all::OP_CHECKMULTISIG).into_script()
}

/// panics if htlc.transaction_output_index.is_none()!
pub fn build_htlc_transaction(prev_hash: &Txid, feerate_per_kw: u64, to_self_delay: u16, htlc: &HTLCOutputInCommitment, a_delayed_payment_key: &PublicKey, revocation_key: &PublicKey) -> Transaction {
	let mut txins: Vec<TxIn> = Vec::new();
	txins.push(TxIn {
		previous_output: OutPoint {
			txid: prev_hash.clone(),
			vout: htlc.transaction_output_index.expect("Can't build an HTLC transaction for a dust output"),
		},
		script_sig: Script::new(),
		sequence: 0,
		witness: Vec::new(),
	});

	let total_fee = if htlc.offered {
			feerate_per_kw * HTLC_TIMEOUT_TX_WEIGHT / 1000
		} else {
			feerate_per_kw * HTLC_SUCCESS_TX_WEIGHT / 1000
		};

	let mut txouts: Vec<TxOut> = Vec::new();
	txouts.push(TxOut {
		script_pubkey: get_revokeable_redeemscript(revocation_key, to_self_delay, a_delayed_payment_key).to_v0_p2wsh(),
		value: htlc.amount_msat / 1000 - total_fee //TODO: BOLT 3 does not specify if we should add amount_msat before dividing or if we should divide by 1000 before subtracting (as we do here)
	});

	Transaction {
		version: 2,
		lock_time: if htlc.offered { htlc.cltv_expiry } else { 0 },
		input: txins,
		output: txouts,
	}
}

#[derive(Clone)]
/// We use this to track local commitment transactions and put off signing them until we are ready
/// to broadcast. Eventually this will require a signer which is possibly external, but for now we
/// just pass in the SecretKeys required.
pub struct LocalCommitmentTransaction {
	// TODO: We should migrate away from providing the transaction, instead providing enough to
	// allow the ChannelKeys to construct it from scratch. Luckily we already have HTLC data here,
	// so we're probably most of the way there.
	/// The commitment transaction itself, in unsigned form.
	pub unsigned_tx: Transaction,
	/// Our counterparty's signature for the transaction, above.
	pub their_sig: Signature,
	// Which order the signatures should go in when constructing the final commitment tx witness.
	// The user should be able to reconstruc this themselves, so we don't bother to expose it.
	our_sig_first: bool,
	/// The key derivation parameters for this commitment transaction
	pub local_keys: TxCreationKeys,
	/// The feerate paid per 1000-weight-unit in this commitment transaction. This value is
	/// controlled by the channel initiator.
	pub feerate_per_kw: u64,
	/// The HTLCs and remote htlc signatures which were included in this commitment transaction.
	///
	/// Note that this includes all HTLCs, including ones which were considered dust and not
	/// actually included in the transaction as it appears on-chain, but who's value is burned as
	/// fees and not included in the to_local or to_remote outputs.
	///
	/// The remote HTLC signatures in the second element will always be set for non-dust HTLCs, ie
	/// those for which transaction_output_index.is_some().
	pub per_htlc: Vec<(HTLCOutputInCommitment, Option<Signature>)>,
}
impl LocalCommitmentTransaction {
	#[cfg(test)]
	pub fn dummy() -> Self {
		let dummy_input = TxIn {
			previous_output: OutPoint {
				txid: Default::default(),
				vout: 0,
			},
			script_sig: Default::default(),
			sequence: 0,
			witness: vec![]
		};
		let dummy_key = PublicKey::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[42; 32]).unwrap());
		let dummy_sig = Secp256k1::new().sign(&secp256k1::Message::from_slice(&[42; 32]).unwrap(), &SecretKey::from_slice(&[42; 32]).unwrap());
		Self {
			unsigned_tx: Transaction {
				version: 2,
				input: vec![dummy_input],
				output: Vec::new(),
				lock_time: 0,
			},
			their_sig: dummy_sig,
			our_sig_first: false,
			local_keys: TxCreationKeys {
					per_commitment_point: dummy_key.clone(),
					revocation_key: dummy_key.clone(),
					a_htlc_key: dummy_key.clone(),
					b_htlc_key: dummy_key.clone(),
					a_delayed_payment_key: dummy_key.clone(),
				},
			feerate_per_kw: 0,
			per_htlc: Vec::new()
		}
	}

	/// Generate a new LocalCommitmentTransaction based on a raw commitment transaction,
	/// remote signature and both parties keys
	pub(crate) fn new_missing_local_sig(unsigned_tx: Transaction, their_sig: Signature, our_funding_key: &PublicKey, their_funding_key: &PublicKey, local_keys: TxCreationKeys, feerate_per_kw: u64, htlc_data: Vec<(HTLCOutputInCommitment, Option<Signature>)>) -> LocalCommitmentTransaction {
		if unsigned_tx.input.len() != 1 { panic!("Tried to store a commitment transaction that had input count != 1!"); }
		if unsigned_tx.input[0].witness.len() != 0 { panic!("Tried to store a signed commitment transaction?"); }

		Self {
			unsigned_tx,
			their_sig,
			our_sig_first: our_funding_key.serialize()[..] < their_funding_key.serialize()[..],
			local_keys,
			feerate_per_kw,
			per_htlc: htlc_data,
		}
	}

	/// Get the txid of the local commitment transaction contained in this
	/// LocalCommitmentTransaction
	pub fn txid(&self) -> Txid {
		self.unsigned_tx.txid()
	}

	/// Gets our signature for the contained commitment transaction given our funding private key.
	///
	/// Funding key is your key included in the 2-2 funding_outpoint lock. Should be provided
	/// by your ChannelKeys.
	/// Funding redeemscript is script locking funding_outpoint. This is the mutlsig script
	/// between your own funding key and your counterparty's. Currently, this is provided in
	/// ChannelKeys::sign_local_commitment() calls directly.
	/// Channel value is amount locked in funding_outpoint.
	pub fn get_local_sig<T: secp256k1::Signing>(&self, funding_key: &SecretKey, funding_redeemscript: &Script, channel_value_satoshis: u64, secp_ctx: &Secp256k1<T>) -> Signature {
		let sighash = hash_to_message!(&bip143::SighashComponents::new(&self.unsigned_tx)
			.sighash_all(&self.unsigned_tx.input[0], funding_redeemscript, channel_value_satoshis)[..]);
		secp_ctx.sign(&sighash, funding_key)
	}

	pub(crate) fn add_local_sig(&self, funding_redeemscript: &Script, our_sig: Signature) -> Transaction {
		let mut tx = self.unsigned_tx.clone();
		// First push the multisig dummy, note that due to BIP147 (NULLDUMMY) it must be a zero-length element.
		tx.input[0].witness.push(Vec::new());

		if self.our_sig_first {
			tx.input[0].witness.push(our_sig.serialize_der().to_vec());
			tx.input[0].witness.push(self.their_sig.serialize_der().to_vec());
		} else {
			tx.input[0].witness.push(self.their_sig.serialize_der().to_vec());
			tx.input[0].witness.push(our_sig.serialize_der().to_vec());
		}
		tx.input[0].witness[1].push(SigHashType::All as u8);
		tx.input[0].witness[2].push(SigHashType::All as u8);

		tx.input[0].witness.push(funding_redeemscript.as_bytes().to_vec());
		tx
	}

	/// Get a signature for each HTLC which was included in the commitment transaction (ie for
	/// which HTLCOutputInCommitment::transaction_output_index.is_some()).
	///
	/// The returned Vec has one entry for each HTLC, and in the same order. For HTLCs which were
	/// considered dust and not included, a None entry exists, for all others a signature is
	/// included.
	pub fn get_htlc_sigs<T: secp256k1::Signing + secp256k1::Verification>(&self, htlc_base_key: &SecretKey, local_csv: u16, secp_ctx: &Secp256k1<T>) -> Result<Vec<Option<Signature>>, ()> {
		let txid = self.txid();
		let mut ret = Vec::with_capacity(self.per_htlc.len());
		let our_htlc_key = derive_private_key(secp_ctx, &self.local_keys.per_commitment_point, htlc_base_key).map_err(|_| ())?;

		for this_htlc in self.per_htlc.iter() {
			if this_htlc.0.transaction_output_index.is_some() {
				let htlc_tx = build_htlc_transaction(&txid, self.feerate_per_kw, local_csv, &this_htlc.0, &self.local_keys.a_delayed_payment_key, &self.local_keys.revocation_key);

				let htlc_redeemscript = get_htlc_redeemscript_with_explicit_keys(&this_htlc.0, &self.local_keys.a_htlc_key, &self.local_keys.b_htlc_key, &self.local_keys.revocation_key);

				let sighash = hash_to_message!(&bip143::SighashComponents::new(&htlc_tx).sighash_all(&htlc_tx.input[0], &htlc_redeemscript, this_htlc.0.amount_msat / 1000)[..]);
				ret.push(Some(secp_ctx.sign(&sighash, &our_htlc_key)));
			} else {
				ret.push(None);
			}
		}
		Ok(ret)
	}

	/// Gets a signed HTLC transaction given a preimage (for !htlc.offered) and the local HTLC transaction signature.
	pub(crate) fn get_signed_htlc_tx(&self, htlc_index: usize, signature: &Signature, preimage: &Option<PaymentPreimage>, local_csv: u16) -> Transaction {
		let txid = self.txid();
		let this_htlc = &self.per_htlc[htlc_index];
		assert!(this_htlc.0.transaction_output_index.is_some());
		// if we don't have preimage for an HTLC-Success, we can't generate an HTLC transaction.
		if !this_htlc.0.offered && preimage.is_none() { unreachable!(); }
		// Further, we should never be provided the preimage for an HTLC-Timeout transaction.
		if  this_htlc.0.offered && preimage.is_some() { unreachable!(); }

		let mut htlc_tx = build_htlc_transaction(&txid, self.feerate_per_kw, local_csv, &this_htlc.0, &self.local_keys.a_delayed_payment_key, &self.local_keys.revocation_key);
		// Channel should have checked that we have a remote signature for this HTLC at
		// creation, and we should have a sensible htlc transaction:
		assert!(this_htlc.1.is_some());

		let htlc_redeemscript = get_htlc_redeemscript_with_explicit_keys(&this_htlc.0, &self.local_keys.a_htlc_key, &self.local_keys.b_htlc_key, &self.local_keys.revocation_key);

		// First push the multisig dummy, note that due to BIP147 (NULLDUMMY) it must be a zero-length element.
		htlc_tx.input[0].witness.push(Vec::new());

		htlc_tx.input[0].witness.push(this_htlc.1.unwrap().serialize_der().to_vec());
		htlc_tx.input[0].witness.push(signature.serialize_der().to_vec());
		htlc_tx.input[0].witness[1].push(SigHashType::All as u8);
		htlc_tx.input[0].witness[2].push(SigHashType::All as u8);

		if this_htlc.0.offered {
			// Due to BIP146 (MINIMALIF) this must be a zero-length element to relay.
			htlc_tx.input[0].witness.push(Vec::new());
		} else {
			htlc_tx.input[0].witness.push(preimage.unwrap().0.to_vec());
		}

		htlc_tx.input[0].witness.push(htlc_redeemscript.as_bytes().to_vec());
		htlc_tx
	}
}
impl PartialEq for LocalCommitmentTransaction {
	// We dont care whether we are signed in equality comparison
	fn eq(&self, o: &Self) -> bool {
		self.txid() == o.txid()
	}
}
impl Writeable for LocalCommitmentTransaction {
	fn write<W: Writer>(&self, writer: &mut W) -> Result<(), ::std::io::Error> {
		if let Err(e) = self.unsigned_tx.consensus_encode(&mut WriterWriteAdaptor(writer)) {
			match e {
				encode::Error::Io(e) => return Err(e),
				_ => panic!("local tx must have been well-formed!"),
			}
		}
		self.their_sig.write(writer)?;
		self.our_sig_first.write(writer)?;
		self.local_keys.write(writer)?;
		self.feerate_per_kw.write(writer)?;
		writer.write_all(&byte_utils::be64_to_array(self.per_htlc.len() as u64))?;
		for &(ref htlc, ref sig) in self.per_htlc.iter() {
			htlc.write(writer)?;
			sig.write(writer)?;
		}
		Ok(())
	}
}
impl Readable for LocalCommitmentTransaction {
	fn read<R: ::std::io::Read>(reader: &mut R) -> Result<Self, DecodeError> {
		let unsigned_tx = match Transaction::consensus_decode(reader.by_ref()) {
			Ok(tx) => tx,
			Err(e) => match e {
				encode::Error::Io(ioe) => return Err(DecodeError::Io(ioe)),
				_ => return Err(DecodeError::InvalidValue),
			},
		};
		let their_sig = Readable::read(reader)?;
		let our_sig_first = Readable::read(reader)?;
		let local_keys = Readable::read(reader)?;
		let feerate_per_kw = Readable::read(reader)?;
		let htlcs_count: u64 = Readable::read(reader)?;
		let mut per_htlc = Vec::with_capacity(cmp::min(htlcs_count as usize, MAX_ALLOC_SIZE / mem::size_of::<(HTLCOutputInCommitment, Option<Signature>)>()));
		for _ in 0..htlcs_count {
			let htlc: HTLCOutputInCommitment = Readable::read(reader)?;
			let sigs = Readable::read(reader)?;
			per_htlc.push((htlc, sigs));
		}

		if unsigned_tx.input.len() != 1 {
			// Ensure tx didn't hit the 0-input ambiguity case.
			return Err(DecodeError::InvalidValue);
		}
		Ok(Self {
			unsigned_tx,
			their_sig,
			our_sig_first,
			local_keys,
			feerate_per_kw,
			per_htlc,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::CounterpartyCommitmentSecrets;
	use hex;

	#[test]
	fn test_per_commitment_storage() {
		// Test vectors from BOLT 3:
		let mut secrets: Vec<[u8; 32]> = Vec::new();
		let mut monitor;

		macro_rules! test_secrets {
			() => {
				let mut idx = 281474976710655;
				for secret in secrets.iter() {
					assert_eq!(monitor.get_secret(idx).unwrap(), *secret);
					idx -= 1;
				}
				assert_eq!(monitor.get_min_seen_secret(), idx + 1);
				assert!(monitor.get_secret(idx).is_none());
			};
		}

		{
			// insert_secret correct sequence
			monitor = CounterpartyCommitmentSecrets::new();
			secrets.clear();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("7cc854b54e3e0dcdb010d7a3fee464a9687be6e8db3be6854c475621e007a5dc").unwrap());
			monitor.provide_secret(281474976710655, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("c7518c8ae4660ed02894df8976fa1a3659c1a8b4b5bec0c4b872abeba4cb8964").unwrap());
			monitor.provide_secret(281474976710654, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("2273e227a5b7449b6e70f1fb4652864038b1cbf9cd7c043a7d6456b7fc275ad8").unwrap());
			monitor.provide_secret(281474976710653, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("27cddaa5624534cb6cb9d7da077cf2b22ab21e9b506fd4998a51d54502e99116").unwrap());
			monitor.provide_secret(281474976710652, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("c65716add7aa98ba7acb236352d665cab17345fe45b55fb879ff80e6bd0c41dd").unwrap());
			monitor.provide_secret(281474976710651, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("969660042a28f32d9be17344e09374b379962d03db1574df5a8a5a47e19ce3f2").unwrap());
			monitor.provide_secret(281474976710650, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("a5a64476122ca0925fb344bdc1854c1c0a59fc614298e50a33e331980a220f32").unwrap());
			monitor.provide_secret(281474976710649, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("05cde6323d949933f7f7b78776bcc1ea6d9b31447732e3802e1f7ac44b650e17").unwrap());
			monitor.provide_secret(281474976710648, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();
		}

		{
			// insert_secret #1 incorrect
			monitor = CounterpartyCommitmentSecrets::new();
			secrets.clear();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("02a40c85b6f28da08dfdbe0926c53fab2de6d28c10301f8f7c4073d5e42e3148").unwrap());
			monitor.provide_secret(281474976710655, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("c7518c8ae4660ed02894df8976fa1a3659c1a8b4b5bec0c4b872abeba4cb8964").unwrap());
			assert!(monitor.provide_secret(281474976710654, secrets.last().unwrap().clone()).is_err());
		}

		{
			// insert_secret #2 incorrect (#1 derived from incorrect)
			monitor = CounterpartyCommitmentSecrets::new();
			secrets.clear();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("02a40c85b6f28da08dfdbe0926c53fab2de6d28c10301f8f7c4073d5e42e3148").unwrap());
			monitor.provide_secret(281474976710655, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("dddc3a8d14fddf2b68fa8c7fbad2748274937479dd0f8930d5ebb4ab6bd866a3").unwrap());
			monitor.provide_secret(281474976710654, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("2273e227a5b7449b6e70f1fb4652864038b1cbf9cd7c043a7d6456b7fc275ad8").unwrap());
			monitor.provide_secret(281474976710653, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("27cddaa5624534cb6cb9d7da077cf2b22ab21e9b506fd4998a51d54502e99116").unwrap());
			assert!(monitor.provide_secret(281474976710652, secrets.last().unwrap().clone()).is_err());
		}

		{
			// insert_secret #3 incorrect
			monitor = CounterpartyCommitmentSecrets::new();
			secrets.clear();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("7cc854b54e3e0dcdb010d7a3fee464a9687be6e8db3be6854c475621e007a5dc").unwrap());
			monitor.provide_secret(281474976710655, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("c7518c8ae4660ed02894df8976fa1a3659c1a8b4b5bec0c4b872abeba4cb8964").unwrap());
			monitor.provide_secret(281474976710654, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("c51a18b13e8527e579ec56365482c62f180b7d5760b46e9477dae59e87ed423a").unwrap());
			monitor.provide_secret(281474976710653, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("27cddaa5624534cb6cb9d7da077cf2b22ab21e9b506fd4998a51d54502e99116").unwrap());
			assert!(monitor.provide_secret(281474976710652, secrets.last().unwrap().clone()).is_err());
		}

		{
			// insert_secret #4 incorrect (1,2,3 derived from incorrect)
			monitor = CounterpartyCommitmentSecrets::new();
			secrets.clear();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("02a40c85b6f28da08dfdbe0926c53fab2de6d28c10301f8f7c4073d5e42e3148").unwrap());
			monitor.provide_secret(281474976710655, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("dddc3a8d14fddf2b68fa8c7fbad2748274937479dd0f8930d5ebb4ab6bd866a3").unwrap());
			monitor.provide_secret(281474976710654, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("c51a18b13e8527e579ec56365482c62f180b7d5760b46e9477dae59e87ed423a").unwrap());
			monitor.provide_secret(281474976710653, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("ba65d7b0ef55a3ba300d4e87af29868f394f8f138d78a7011669c79b37b936f4").unwrap());
			monitor.provide_secret(281474976710652, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("c65716add7aa98ba7acb236352d665cab17345fe45b55fb879ff80e6bd0c41dd").unwrap());
			monitor.provide_secret(281474976710651, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("969660042a28f32d9be17344e09374b379962d03db1574df5a8a5a47e19ce3f2").unwrap());
			monitor.provide_secret(281474976710650, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("a5a64476122ca0925fb344bdc1854c1c0a59fc614298e50a33e331980a220f32").unwrap());
			monitor.provide_secret(281474976710649, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("05cde6323d949933f7f7b78776bcc1ea6d9b31447732e3802e1f7ac44b650e17").unwrap());
			assert!(monitor.provide_secret(281474976710648, secrets.last().unwrap().clone()).is_err());
		}

		{
			// insert_secret #5 incorrect
			monitor = CounterpartyCommitmentSecrets::new();
			secrets.clear();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("7cc854b54e3e0dcdb010d7a3fee464a9687be6e8db3be6854c475621e007a5dc").unwrap());
			monitor.provide_secret(281474976710655, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("c7518c8ae4660ed02894df8976fa1a3659c1a8b4b5bec0c4b872abeba4cb8964").unwrap());
			monitor.provide_secret(281474976710654, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("2273e227a5b7449b6e70f1fb4652864038b1cbf9cd7c043a7d6456b7fc275ad8").unwrap());
			monitor.provide_secret(281474976710653, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("27cddaa5624534cb6cb9d7da077cf2b22ab21e9b506fd4998a51d54502e99116").unwrap());
			monitor.provide_secret(281474976710652, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("631373ad5f9ef654bb3dade742d09504c567edd24320d2fcd68e3cc47e2ff6a6").unwrap());
			monitor.provide_secret(281474976710651, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("969660042a28f32d9be17344e09374b379962d03db1574df5a8a5a47e19ce3f2").unwrap());
			assert!(monitor.provide_secret(281474976710650, secrets.last().unwrap().clone()).is_err());
		}

		{
			// insert_secret #6 incorrect (5 derived from incorrect)
			monitor = CounterpartyCommitmentSecrets::new();
			secrets.clear();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("7cc854b54e3e0dcdb010d7a3fee464a9687be6e8db3be6854c475621e007a5dc").unwrap());
			monitor.provide_secret(281474976710655, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("c7518c8ae4660ed02894df8976fa1a3659c1a8b4b5bec0c4b872abeba4cb8964").unwrap());
			monitor.provide_secret(281474976710654, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("2273e227a5b7449b6e70f1fb4652864038b1cbf9cd7c043a7d6456b7fc275ad8").unwrap());
			monitor.provide_secret(281474976710653, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("27cddaa5624534cb6cb9d7da077cf2b22ab21e9b506fd4998a51d54502e99116").unwrap());
			monitor.provide_secret(281474976710652, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("631373ad5f9ef654bb3dade742d09504c567edd24320d2fcd68e3cc47e2ff6a6").unwrap());
			monitor.provide_secret(281474976710651, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("b7e76a83668bde38b373970155c868a653304308f9896692f904a23731224bb1").unwrap());
			monitor.provide_secret(281474976710650, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("a5a64476122ca0925fb344bdc1854c1c0a59fc614298e50a33e331980a220f32").unwrap());
			monitor.provide_secret(281474976710649, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("05cde6323d949933f7f7b78776bcc1ea6d9b31447732e3802e1f7ac44b650e17").unwrap());
			assert!(monitor.provide_secret(281474976710648, secrets.last().unwrap().clone()).is_err());
		}

		{
			// insert_secret #7 incorrect
			monitor = CounterpartyCommitmentSecrets::new();
			secrets.clear();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("7cc854b54e3e0dcdb010d7a3fee464a9687be6e8db3be6854c475621e007a5dc").unwrap());
			monitor.provide_secret(281474976710655, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("c7518c8ae4660ed02894df8976fa1a3659c1a8b4b5bec0c4b872abeba4cb8964").unwrap());
			monitor.provide_secret(281474976710654, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("2273e227a5b7449b6e70f1fb4652864038b1cbf9cd7c043a7d6456b7fc275ad8").unwrap());
			monitor.provide_secret(281474976710653, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("27cddaa5624534cb6cb9d7da077cf2b22ab21e9b506fd4998a51d54502e99116").unwrap());
			monitor.provide_secret(281474976710652, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("c65716add7aa98ba7acb236352d665cab17345fe45b55fb879ff80e6bd0c41dd").unwrap());
			monitor.provide_secret(281474976710651, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("969660042a28f32d9be17344e09374b379962d03db1574df5a8a5a47e19ce3f2").unwrap());
			monitor.provide_secret(281474976710650, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("e7971de736e01da8ed58b94c2fc216cb1dca9e326f3a96e7194fe8ea8af6c0a3").unwrap());
			monitor.provide_secret(281474976710649, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("05cde6323d949933f7f7b78776bcc1ea6d9b31447732e3802e1f7ac44b650e17").unwrap());
			assert!(monitor.provide_secret(281474976710648, secrets.last().unwrap().clone()).is_err());
		}

		{
			// insert_secret #8 incorrect
			monitor = CounterpartyCommitmentSecrets::new();
			secrets.clear();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("7cc854b54e3e0dcdb010d7a3fee464a9687be6e8db3be6854c475621e007a5dc").unwrap());
			monitor.provide_secret(281474976710655, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("c7518c8ae4660ed02894df8976fa1a3659c1a8b4b5bec0c4b872abeba4cb8964").unwrap());
			monitor.provide_secret(281474976710654, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("2273e227a5b7449b6e70f1fb4652864038b1cbf9cd7c043a7d6456b7fc275ad8").unwrap());
			monitor.provide_secret(281474976710653, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("27cddaa5624534cb6cb9d7da077cf2b22ab21e9b506fd4998a51d54502e99116").unwrap());
			monitor.provide_secret(281474976710652, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("c65716add7aa98ba7acb236352d665cab17345fe45b55fb879ff80e6bd0c41dd").unwrap());
			monitor.provide_secret(281474976710651, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("969660042a28f32d9be17344e09374b379962d03db1574df5a8a5a47e19ce3f2").unwrap());
			monitor.provide_secret(281474976710650, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("a5a64476122ca0925fb344bdc1854c1c0a59fc614298e50a33e331980a220f32").unwrap());
			monitor.provide_secret(281474976710649, secrets.last().unwrap().clone()).unwrap();
			test_secrets!();

			secrets.push([0; 32]);
			secrets.last_mut().unwrap()[0..32].clone_from_slice(&hex::decode("a7efbc61aac46d34f77778bac22c8a20c6a46ca460addc49009bda875ec88fa4").unwrap());
			assert!(monitor.provide_secret(281474976710648, secrets.last().unwrap().clone()).is_err());
		}
	}
}
