pub mod gas;
pub mod storage;
mod tendermint;

use std::path::PathBuf;
use std::sync::mpsc;

use anoma::bytes::ByteBuf;
use anoma::config::Config;
use anoma::protobuf::types::Tx;
use prost::Message;
use thiserror::Error;
use vm::host_env::write_log::StorageKey;

use self::gas::BlockGasMeter;
use self::storage::{
    Address, BasicAddress, BlockHash, BlockHeight, Storage, ValidatorAddress,
};
use self::tendermint::{AbciMsg, AbciReceiver};
use crate::vm::host_env::write_log::WriteLog;
use crate::vm::{self, TxRunner, VpRunner};

#[derive(Error, Debug)]
pub enum Error {
    #[error("TEMPORARY error: {error}")]
    Temporary { error: String },
    #[error("Error removing the DB data: {0}")]
    RemoveDB(std::io::Error),
    #[error("Storage error: {0}")]
    StorageError(storage::Error),
    #[error("Shell ABCI channel receiver error: {0}")]
    AbciChannelRecvError(mpsc::RecvError),
    #[error("Shell ABCI channel sender error: {0}")]
    AbciChannelSendError(String),
    #[error("Error decoding a transaction from bytes: {0}")]
    TxDecodingError(prost::DecodeError),
    #[error("Transaction runner error: {0}")]
    TxRunnerError(vm::Error),
    #[error("Validity predicate for {addr} runner error: {error}")]
    VpRunnerError { addr: Address, error: vm::Error },
    #[error("Gas error: {0}")]
    GasError(gas::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn run(config: Config) -> Result<()> {
    // open a channel between ABCI (the sender) and the shell (the receiver)
    let (sender, receiver) = mpsc::channel();
    let shell = Shell::new(receiver, &config.db_home_dir());
    let addr = format!("{}:{}", config.tendermint.host, config.tendermint.port)
        .parse()
        .map_err(|e| Error::Temporary {
            error: format!("cannot parse tendermint address {}", e),
        })?;
    // Run Tendermint ABCI server in another thread
    std::thread::spawn(move || tendermint::run(sender, config, addr));
    shell.run()
}

pub fn reset(config: Config) -> Result<()> {
    // simply nuke the DB files
    let db_path = config.db_home_dir();
    match std::fs::remove_dir_all(&db_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        res => res.map_err(Error::RemoveDB)?,
    };
    // reset Tendermint state
    tendermint::reset(config);
    Ok(())
}

#[derive(Debug)]
pub struct Shell {
    abci: AbciReceiver,
    storage: storage::Storage,
    gas_meter: BlockGasMeter,
    write_log: WriteLog,
}

#[derive(Clone, Debug)]
pub enum MempoolTxType {
    /// A transaction that has not been validated by this node before
    NewTransaction,
    /// A transaction that has been validated at some previous level that may
    /// need to be validated again
    RecheckTransaction,
}

pub struct MerkleRoot(pub Vec<u8>);

impl Shell {
    pub fn new(abci: AbciReceiver, db_path: &PathBuf) -> Self {
        let mut storage = Storage::new(db_path);
        // TODO load initial accounts from genesis
        let va = ValidatorAddress::new_address("va".to_owned());
        storage
            .write(
                &va,
                "balance/eth",
                vec![0x10_u8, 0x27_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8],
            )
            .expect("Unable to set the initial balance for validator account");
        let ba = BasicAddress::new_address("ba".to_owned());
        storage
            .write(
                &ba,
                "balance/eth",
                vec![0x64_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8],
            )
            .expect("Unable to set the initial balance for basic account");
        Self {
            abci,
            storage,
            gas_meter: BlockGasMeter::default(),
            write_log: WriteLog::new(),
        }
    }

    /// Run the shell in the current thread (blocking).
    pub fn run(mut self) -> Result<()> {
        loop {
            let msg = self.abci.recv().map_err(Error::AbciChannelRecvError)?;
            match msg {
                AbciMsg::GetInfo { reply } => {
                    let result = self.last_state();
                    reply.send(result).map_err(|e| {
                        Error::AbciChannelSendError(format!("GetInfo {}", e))
                    })?
                }
                AbciMsg::InitChain { reply, chain_id } => {
                    self.init_chain(chain_id)?;
                    reply.send(()).map_err(|e| {
                        Error::AbciChannelSendError(format!("InitChain {}", e))
                    })?
                }
                AbciMsg::MempoolValidate { reply, tx, r#type } => {
                    let result = self
                        .mempool_validate(&tx, r#type)
                        .map_err(|e| format!("{}", e));
                    reply.send(result).map_err(|e| {
                        Error::AbciChannelSendError(format!(
                            "MempoolValidate {}",
                            e
                        ))
                    })?
                }
                AbciMsg::BeginBlock {
                    reply,
                    hash,
                    height,
                } => {
                    self.begin_block(hash, height);
                    reply.send(()).map_err(|e| {
                        Error::AbciChannelSendError(format!("BeginBlock {}", e))
                    })?
                }
                AbciMsg::ApplyTx { reply, tx } => {
                    let result =
                        self.apply_tx(&tx).map_err(|e| format!("{}", e));
                    reply.send(result).map_err(|e| {
                        Error::AbciChannelSendError(format!("ApplyTx {}", e))
                    })?
                }
                AbciMsg::EndBlock { reply, height } => {
                    self.end_block(height);
                    reply.send(()).map_err(|e| {
                        Error::AbciChannelSendError(format!("EndBlock {}", e))
                    })?
                }
                AbciMsg::CommitBlock { reply } => {
                    let result = self.commit();
                    reply.send(result).map_err(|e| {
                        Error::AbciChannelSendError(format!(
                            "CommitBlock {}",
                            e
                        ))
                    })?
                }
            }
        }
    }
}

impl Shell {
    pub fn init_chain(&mut self, chain_id: String) -> Result<()> {
        self.storage
            .set_chain_id(&chain_id)
            .map_err(Error::StorageError)
    }

    /// Validate a transaction request. On success, the transaction will
    /// included in the mempool and propagated to peers, otherwise it will be
    /// rejected.
    pub fn mempool_validate(
        &self,
        tx_bytes: &[u8],
        r#_type: MempoolTxType,
    ) -> Result<()> {
        let _tx = Tx::decode(&tx_bytes[..]).map_err(Error::TxDecodingError)?;
        Ok(())
    }

    /// Validate and apply a transaction.
    pub fn apply_tx(&mut self, tx_bytes: &[u8]) -> Result<u64> {
        self.gas_meter
            .add_base_transaction_fee(tx_bytes.len())
            .map_err(Error::GasError)?;

        let tx = Tx::decode(&tx_bytes[..]).map_err(Error::TxDecodingError)?;

        let tx_data = tx.data.unwrap_or(vec![]);

        // Execute the transaction code
        let tx_runner = TxRunner::new();
        tx_runner
            .run(&mut self.storage, &mut self.write_log, tx.code, &tx_data)
            .map_err(Error::TxRunnerError)?;

        let keys_changed: Vec<String> = self
            .write_log
            .get_changed_key()
            .iter()
            .map(|StorageKey { addr, key }| format!("{}/{}", addr, key))
            .collect();
        // TODO determine these from the changed keys
        let src = "va";
        let dest = "ba";
        let src_addr = Address::new_address(src.into());
        let dest_addr = Address::new_address(dest.into());

        // Run a VP for every account with modified storage sub-space
        // TODO run in parallel for all accounts
        //   - all must return `true` to accept the tx
        //   - cancel all remaining workers and fail if any returns `false`
        let src_vp = self
            .storage
            .validity_predicate(&src_addr)
            .map_err(Error::StorageError)?;
        let dest_vp = self
            .storage
            .validity_predicate(&dest_addr)
            .map_err(Error::StorageError)?;

        let vp_runner = VpRunner::new();
        let (vp_sender, vp_receiver) = mpsc::channel();
        vp_runner
            .run(
                src_vp,
                &tx_data,
                src.to_string(),
                &self.storage,
                &self.write_log,
                &keys_changed,
                vp_sender.clone(),
            )
            .map_err(|error| Error::VpRunnerError {
                addr: src_addr.clone(),
                error,
            })?;
        let src_accept = vp_receiver
            .recv()
            .expect("Expected a message from source's VP runner");
        vp_runner
            .run(
                dest_vp,
                &tx_data,
                dest.to_string(),
                &self.storage,
                &self.write_log,
                &keys_changed,
                vp_sender,
            )
            .map_err(|error| Error::VpRunnerError {
                addr: dest_addr.clone(),
                error,
            })?;
        let dest_accept = vp_receiver
            .recv()
            .expect("Expected a message from destination's VP runner");

        // Apply the transaction if accepted by all the VPs
        if src_accept && dest_accept {
            log::debug!(
                "all accepted apply_tx storage modification {:#?}",
                self.storage
            );
            self.write_log.commit_tx();
        } else {
            log::debug!(
                "tx declined by {}",
                if src_accept {
                    "dest"
                } else {
                    if dest_accept { "src" } else { "src and dest" }
                }
            );
            self.write_log.drop_tx();
        }

        self.gas_meter
            .finalize_transaction()
            .map_err(Error::GasError)
    }

    /// Begin a new block.
    pub fn begin_block(&mut self, hash: BlockHash, height: BlockHeight) {
        self.gas_meter.reset();
        self.storage.begin_block(hash, height).unwrap();
    }

    /// End a block.
    pub fn end_block(&mut self, _height: BlockHeight) {}

    /// Commit a block. Persist the application state and return the Merkle root
    /// hash.
    pub fn commit(&mut self) -> MerkleRoot {
        // commit changes from the write-log to storage
        self.write_log
            .commit_block(&mut self.storage)
            .expect("Expected committing block write log success");
        log::debug!("storage to commit {:#?}", self.storage);
        // store the block's data in DB
        // TODO commit async?
        self.storage.commit().unwrap_or_else(|e| {
            log::error!(
                "Encountered a storage error while committing a block {:?}",
                e
            )
        });
        let root = self.storage.merkle_root();
        MerkleRoot(root.as_slice().to_vec())
    }

    /// Load the Merkle root hash and the height of the last committed block, if
    /// any.
    pub fn last_state(&mut self) -> Option<(MerkleRoot, u64)> {
        let result = self.storage.load_last_state().unwrap_or_else(|e| {
            log::error!(
                "Encountered an error while reading last state from
        storage {}",
                e
            );
            None
        });
        match &result {
            Some((root, height)) => {
                log::info!(
                    "Last state root hash: {}, height: {}",
                    ByteBuf(&root.0),
                    height
                )
            }
            None => {
                log::info!("No state could be found")
            }
        }
        result
    }
}