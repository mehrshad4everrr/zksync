use std::error::Error;
use ff::{Field, PrimeField, PrimeFieldRepr, BitIterator};
use super::plasma_state::{State, Block};
use super::prover::{Prover, LifetimedProver};
use std::fmt;
use rand::{OsRng, Rng};

use sapling_crypto::circuit::float_point::parse_float_to_u128;
use super::super::circuit::plasma_constants;
use super::super::balance_tree;
use super::super::circuit::utils::be_bit_vector_into_bytes;
use super::super::circuit::baby_plasma::{Update, Transaction, TransactionWitness};

use sapling_crypto::alt_babyjubjub::{AltJubjubBn256};

use pairing::bn256::Bn256;
use pairing::bn256::Fr;
use bellman::groth16::{Proof, Parameters, create_random_proof, verify_proof, prepare_verifying_key};

use crypto::sha2::Sha256;
use crypto::digest::Digest;

#[derive(Debug)]
pub enum BabyProverErr {
    Unknown,
    InvalidAmountEncoding,
    InvalidFeeEncoding,
    InvalidSender,
    InvalidRecipient,
    IoError(std::io::Error)
}

impl Error for BabyProverErr {
    fn description(&self) -> &str {
        match *self {
            BabyProverErr::Unknown => "Unknown error",
            BabyProverErr::InvalidAmountEncoding => "transfer amount is malformed or too large",
            BabyProverErr::InvalidFeeEncoding => "transfer fee is malformed or too large",
            BabyProverErr::InvalidSender => "sender account is unknown",
            BabyProverErr::InvalidRecipient => "recipient account is unknown",
            BabyProverErr::IoError(_) => "encountered an I/O error",
        }
    }
}

impl fmt::Display for BabyProverErr {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        if let &BabyProverErr::IoError(ref e) = self {
            write!(f, "I/O error: ")?;
            e.fmt(f)
        } else {
            write!(f, "{}", self.description())
        }
    }
}

pub struct BabyProver {
    batch_size: usize,
    accounts_tree: balance_tree::BabyBalanceTree,
    parameters: BabyParameters,
}

type BabyProof = Proof<Bn256>;
type BabyParameters = Parameters<Bn256>;

fn field_element_to_u32<P: PrimeField>(fr: P) -> u32 {
    let mut iterator: Vec<bool> = BitIterator::new(fr.into_repr()).collect();
    iterator.reverse();
    iterator.truncate(32);
    let mut res = 0u32;
    let mut base = 1u32;
    for bit in iterator {
        if bit {
            res += base;
        }
        base = base << 1;
    }

    res
}

// impl<'a> LifetimedProver<'a, Bn256> for BabyProver {
//     fn create(initial_state: &'a State<E>) -> Option<Self> {
        
//     }
// }

impl<'b> Prover<Bn256> for BabyProver {

    type Err = BabyProverErr;
    type Proof = BabyProof;

    fn new(initial_state: &State<Bn256>) 
        -> Result<Self, Self::Err> 
    {
        use std::fs::File;
        use std::io::{BufReader};

        println!("Reading proving key, may take a while");

        let f_r = File::open("pk.key");
        if f_r.is_err() {
            return Err(BabyProverErr::IoError(f_r.err().unwrap()));
        }
        let mut r = BufReader::new(f_r.unwrap());
        let circuit_params = BabyParameters::read(& mut r, true);

        if circuit_params.is_err() {
            return Err(BabyProverErr::IoError(circuit_params.err().unwrap()));
        }

        println!("Copying states to balance tree");

        let mut tree = balance_tree::BabyBalanceTree::new(*plasma_constants::BALANCE_TREE_DEPTH as u32);

        let iter = initial_state.accounts_iter();

        for e in iter {
            let acc_number = *e.0 as u32;
            let leaf_copy = e.1.clone();
            tree.insert(acc_number, leaf_copy);
        }

        let root = tree.root_hash();

        let supplied_root = initial_state.root_hash().clone();

        if root != supplied_root {
            return Err(BabyProverErr::Unknown);
        }

        Ok(Self{
            batch_size: 128,
            accounts_tree: tree,
            parameters: circuit_params.unwrap()
        })
    }

    fn encode_proof(block: &Self::Proof) -> Result<Vec<u8>, Self::Err> {

        // uint256[8] memory in_proof
        // see contracts/Verifier.sol:44

        // TODO: implement
        unimplemented!()        
    }


    // Takes public data from transactions for further commitment to Ethereum
    fn encode_transactions(block: &Block<Bn256>) -> Result<Vec<u8>, Self::Err> {
        let mut encoding : Vec<u8> = vec![];
        let transactions = &block.transactions;

        for tx in transactions {
            let tx_bits = tx.public_data_into_bits();
            let tx_encoding = be_bit_vector_into_bytes(&tx_bits);
            encoding.extend(tx_encoding.into_iter());
        }
        Ok(encoding)
    }

    // Apply transactions to the state while also making a witness for proof, then calculate proof
    fn apply_and_prove(&mut self, block: &Block<Bn256>) -> Result<Self::Proof, Self::Err> {
        let block_number = block.block_number;
        let public_data: Vec<u8> = BabyProver::encode_transactions(block).unwrap();

        let transactions = &block.transactions;
        let num_txes = transactions.len();

        if num_txes != self.batch_size {
            return Err(BabyProverErr::Unknown);
        }

        let mut witnesses: Vec<Option<(Transaction<Bn256>, TransactionWitness<Bn256>)>> = vec![];

        let mut total_fees = Fr::zero();

        let initial_root = self.accounts_tree.root_hash();

        for tx in transactions {
            let sender_leaf_number = field_element_to_u32(tx.from);
            let recipient_leaf_number = field_element_to_u32(tx.to);

            // let mut items = tree.items.clone();

            let sender_leaf = self.accounts_tree.items.get(&sender_leaf_number);
            let recipient_leaf = self.accounts_tree.items.get(&recipient_leaf_number);
            if sender_leaf.is_none() || recipient_leaf.is_none() {
                return Err(BabyProverErr::InvalidSender);
            }
            
            let parsed_transfer_amount = parse_float_to_u128(BitIterator::new(tx.amount.into_repr()).collect(), 
                *plasma_constants::AMOUNT_EXPONENT_BIT_WIDTH,
                *plasma_constants::AMOUNT_MANTISSA_BIT_WIDTH,
                10
            );

            let parsed_fee = parse_float_to_u128(BitIterator::new(tx.fee.into_repr()).collect(), 
                *plasma_constants::FEE_EXPONENT_BIT_WIDTH,
                *plasma_constants::FEE_MANTISSA_BIT_WIDTH,
                10
            );

            if parsed_transfer_amount.is_err() || parsed_fee.is_err() {
                return Err(BabyProverErr::InvalidAmountEncoding);
            }

            let transfer_amount_as_field_element = Fr::from_str(&parsed_transfer_amount.unwrap().to_string()).unwrap();
            let fee_as_field_element = Fr::from_str(&parsed_fee.unwrap().to_string()).unwrap();

            let path_from : Vec<Option<(Fr, bool)>> = self.accounts_tree.merkle_path(sender_leaf_number).into_iter().map(|e| Some(e)).collect();
            let path_to: Vec<Option<(Fr, bool)>> = self.accounts_tree.merkle_path(recipient_leaf_number).into_iter().map(|e| Some(e)).collect();

            let mut transaction : Transaction<Bn256> = Transaction {
                from: Some(tx.from.clone()),
                to: Some(tx.to.clone()),
                amount: Some(tx.amount.clone()),
                fee: Some(tx.fee.clone()),
                nonce: Some(tx.nonce.clone()),
                good_until_block: Some(tx.good_until_block.clone()),
                signature: Some(tx.signature.clone())
            };

            let mut updated_sender_leaf = sender_leaf.unwrap().clone();
            let mut updated_recipient_leaf = recipient_leaf.unwrap().clone();

            updated_sender_leaf.balance.sub_assign(&transfer_amount_as_field_element);
            updated_sender_leaf.balance.sub_assign(&fee_as_field_element);

            updated_sender_leaf.nonce.add_assign(&Fr::one());

            updated_recipient_leaf.balance.add_assign(&transfer_amount_as_field_element);

            total_fees.add_assign(&fee_as_field_element);

            // println!("Updated sender: balance: {}, nonce: {}, pub_x: {}, pub_y: {}", updated_sender_leaf.balance, updated_sender_leaf.nonce, updated_sender_leaf.pub_x, updated_sender_leaf.pub_y);
            // println!("Updated recipient: balance: {}, nonce: {}, pub_x: {}, pub_y: {}", updated_recipient_leaf.balance, updated_recipient_leaf.nonce, updated_recipient_leaf.pub_x, updated_recipient_leaf.pub_y);

            self.accounts_tree.insert(sender_leaf_number, updated_sender_leaf.clone());
            self.accounts_tree.insert(recipient_leaf_number, updated_recipient_leaf.clone());

            {
                let sender_leaf = sender_leaf.unwrap();

                let recipient_leaf = recipient_leaf.unwrap();

                let transaction_witness = TransactionWitness {
                    auth_path_from: path_from,
                    balance_from: Some(sender_leaf.balance),
                    nonce_from: Some(sender_leaf.nonce),
                    pub_x_from: Some(sender_leaf.pub_x),
                    pub_y_from: Some(sender_leaf.pub_y),
                    auth_path_to: path_to,
                    balance_to: Some(recipient_leaf.balance),
                    nonce_to: Some(recipient_leaf.nonce),
                    pub_x_to: Some(recipient_leaf.pub_x),
                    pub_y_to: Some(recipient_leaf.pub_y)
                };

                let witness = (transaction.clone(), transaction_witness);

                witnesses.push(Some(witness));
            }
        }

        let block_number = Fr::from_str(&block_number.to_string()).unwrap();

        let final_root = self.accounts_tree.root_hash();

        let mut public_data_initial_bits = vec![];

        // these two are BE encodings because an iterator is BE. This is also an Ethereum standard behavior

        let block_number_bits: Vec<bool> = BitIterator::new(block_number.into_repr()).collect();
        for _ in 0..256-block_number_bits.len() {
            public_data_initial_bits.push(false);
        }
        public_data_initial_bits.extend(block_number_bits.into_iter());

        let total_fee_bits: Vec<bool> = BitIterator::new(total_fees.into_repr()).collect();
        for _ in 0..256-total_fee_bits.len() {
            public_data_initial_bits.push(false);
        }
        public_data_initial_bits.extend(total_fee_bits.into_iter());

        assert_eq!(public_data_initial_bits.len(), 512);

        let mut h = Sha256::new();

        let bytes_to_hash = be_bit_vector_into_bytes(&public_data_initial_bits);

        h.input(&bytes_to_hash);

        let mut hash_result = [0u8; 32];
        h.result(&mut hash_result[..]);

        {    
            let packed_transaction_data_bytes = public_data;

            let mut next_round_hash_bytes = vec![];
            next_round_hash_bytes.extend(hash_result.iter());
            next_round_hash_bytes.extend(packed_transaction_data_bytes);

            let mut h = Sha256::new();

            h.input(&next_round_hash_bytes);

            h.result(&mut hash_result[..]);
        }

        // clip to fit into field element

        hash_result[0] &= 0x1f; // temporary solution

        let mut repr = Fr::zero().into_repr();
        repr.read_be(&hash_result[..]).expect("pack hash as field element");

        let public_data_commitment = Fr::from_repr(repr).unwrap();

        let params = &AltJubjubBn256::new();

        let instance = Update {
            params: params,
            number_of_transactions: num_txes,
            old_root: Some(initial_root),
            new_root: Some(final_root),
            public_data_commitment: Some(public_data_commitment),
            block_number: Some(block_number),
            total_fee: Some(total_fees),
            transactions: witnesses.clone(),
        };

        let mut rng = OsRng::new().unwrap();

        let proof = create_random_proof(instance, &self.parameters, & mut rng);
        if proof.is_err() {
            return Err(BabyProverErr::Unknown);
        }

        let pvk = prepare_verifying_key(&self.parameters.vk);

        let success = verify_proof(&pvk, &proof.unwrap(), &[initial_root, final_root, public_data_commitment]).unwrap();
        if !success {
            return Err(BabyProverErr::Unknown);
        }

        Ok(proof.unwrap())
    }
    
}