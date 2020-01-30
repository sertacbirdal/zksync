//! Transaction sending policy.
//!
//! Goal is to handle stuck transactions.
//! When we try to commit operation to ETH we select nonce, gas price, sign transaction and
//! watch for its confirmations.
//!
//! If transaction is not confirmed for a while we increase gas price and do the same, but we
//! keep list of all sent transactions for one particular operations, since we can't be sure which
//! one will be commited so we track all of them.
//!
//! Note: make sure to save signed tx to db before sending it to ETH, this way we can be sure
//! that state is always recoverable.

// Built-in deps
use std::collections::{HashSet, VecDeque};
use std::str::FromStr;
use std::time::Duration;
// External uses
use bigdecimal::BigDecimal;
use failure::ensure;
use futures::{channel::mpsc, compat::Future01CompatExt, executor::block_on};
use tokio::runtime::Runtime;
use tokio::time;
use web3::contract::Options;
use web3::transports::Http;
use web3::types::{TransactionReceipt, H256, U256};
use web3::Transport;
// Workspace uses
use eth_client::{ETHClient, SignedCallResult};
use models::abi::zksync_contract;
use models::config_options::{ConfigurationOptions, ThreadPanicNotify};
use models::node::config;
use models::{Action, ActionType, Operation};
use storage::{ConnectionPool, StorageETHOperation, StorageProcessor};

const EXPECTED_WAIT_TIME_BLOCKS: u64 = 30;
const TX_POLL_PERIOD: Duration = Duration::from_secs(5);
const WAIT_CONFIRMATIONS: u64 = 1;

struct OperationETHState {
    operation: Operation,
    txs: Vec<TransactionETHState>,
}

#[derive(Debug, Clone)]
pub struct TransactionETHState {
    pub op_id: i64,
    pub deadline_block: u64,
    pub signed_tx: SignedCallResult,
}

impl From<StorageETHOperation> for TransactionETHState {
    fn from(stored: StorageETHOperation) -> Self {
        TransactionETHState {
            op_id: stored.op_id,
            deadline_block: stored.deadline_block as u64,
            signed_tx: SignedCallResult {
                raw_tx: stored.raw_tx,
                gas_price: U256::from_str(&stored.gas_price.to_string()).unwrap(),
                nonce: U256::from(stored.nonce as u128),
                hash: H256::from_slice(&stored.tx_hash),
            },
        }
    }
}

impl TransactionETHState {
    fn is_stuck(&self, current_block: u64) -> bool {
        current_block >= self.deadline_block
    }
}

struct ExecutedTxStatus {
    confirmations: u64,
    success: bool,
}

struct ETHSender<T: Transport> {
    // unconfirmed operations queue
    unconfirmed_ops: VecDeque<OperationETHState>,
    db_pool: ConnectionPool,
    eth_client: ETHClient<T>,
    rx_for_eth: mpsc::Receiver<Operation>,
    op_notify: mpsc::Sender<Operation>,
}

impl<T: Transport> ETHSender<T> {
    fn new(
        db_pool: ConnectionPool,
        eth_client: ETHClient<T>,
        rx_for_eth: mpsc::Receiver<Operation>,
        op_notify: mpsc::Sender<Operation>,
    ) -> Self {
        let mut sender = Self {
            eth_client,
            unconfirmed_ops: VecDeque::new(),
            db_pool: db_pool.clone(),
            rx_for_eth,
            op_notify,
        };
        let storage = db_pool.access_storage().expect("Failed to access storage");
        if sender.restore_state(storage).is_err() {
            info!("No unconfirmed operations");
        }
        sender
    }

    fn restore_state(&mut self, storage: StorageProcessor) -> Result<(), failure::Error> {
        self.unconfirmed_ops = storage
            .load_unconfirmed_operations()?
            .into_iter()
            .map(|(operation, txs)| OperationETHState {
                operation,
                txs: txs.into_iter().map(|tx| tx.into()).collect(),
            })
            .collect();
        Ok(())
    }

    fn try_commit_current_op(&mut self) {
        if let Some(mut current_op) = self.unconfirmed_ops.pop_front() {
            let success = self
                .drive_to_completion(&mut current_op)
                .map_err(|e| {
                    warn!("Error while trying to complete uncommitted op: {}", e);
                })
                .unwrap_or_default();

            if success {
                info!(
                    "Operation {}, {}  block: {}, confirmed on ETH",
                    current_op.operation.id.unwrap(),
                    current_op.operation.action.to_string(),
                    current_op.operation.block.block_number,
                );

                if current_op.operation.action.get_type() == ActionType::VERIFY {
                    // we notify about verify only when commit is confirmed on the ethereum
                    self.op_notify
                        .try_send(current_op.operation)
                        .map_err(|e| warn!("Failed notify about verify op confirmation: {}", e))
                        .unwrap_or_default();

                    // complete pending withdrawals after each verify.
                    self.call_complete_withdrawals()
                        .map_err(|e| {
                            warn!("Error: {}", e);
                        })
                        .unwrap_or_default();
                }
            } else {
                self.unconfirmed_ops.push_front(current_op);
            }
        }
    }

    async fn run(mut self) {
        let mut timer = time::interval(TX_POLL_PERIOD);

        loop {
            while let Ok(Some(operation)) = self.rx_for_eth.try_next() {
                self.unconfirmed_ops.push_back(OperationETHState {
                    operation,
                    txs: Vec::new(),
                });
            }

            timer.tick().await;

            self.try_commit_current_op();
        }
    }

    fn block_number(&self) -> Result<u64, failure::Error> {
        Ok(block_on(self.eth_client.web3.eth().block_number().compat()).map(|n| n.as_u64())?)
    }

    fn get_tx_status(&self, hash: &H256) -> Result<Option<ExecutedTxStatus>, failure::Error> {
        match block_on(
            self.eth_client
                .web3
                .eth()
                .transaction_receipt(*hash)
                .compat(),
        )? {
            Some(TransactionReceipt {
                block_number: Some(tx_block_number),
                status: Some(status),
                ..
            }) => {
                let confirmations = self
                    .block_number()?
                    .saturating_sub(tx_block_number.as_u64());
                let success = status.as_u64() == 1;
                Ok(Some(ExecutedTxStatus {
                    confirmations,
                    success,
                }))
            }
            _ => Ok(None),
        }
    }

    fn save_signed_tx_to_db(&self, tx: &TransactionETHState) -> Result<(), failure::Error> {
        let storage = self.db_pool.access_storage()?;
        Ok(storage.save_operation_eth_tx(
            tx.op_id,
            tx.signed_tx.hash,
            tx.deadline_block,
            tx.signed_tx.nonce.as_u32(),
            BigDecimal::from_str(&tx.signed_tx.gas_price.to_string()).unwrap(),
            tx.signed_tx.raw_tx.clone(),
        )?)
    }

    fn save_completed_tx_to_db(&self, hash: &H256) -> Result<(), failure::Error> {
        let storage = self.db_pool.access_storage()?;
        Ok(storage.confirm_eth_tx(hash)?)
    }

    fn drive_to_completion(&self, op: &mut OperationETHState) -> Result<bool, failure::Error> {
        let current_block = self.block_number()?;

        // check status
        let mut last_pending_tx: Option<(H256, bool)> = None;
        let mut failed_txs: HashSet<H256> = HashSet::new();

        for tx in &op.txs {
            if let Some(ExecutedTxStatus {
                confirmations,
                success,
            }) = self.get_tx_status(&tx.signed_tx.hash)?
            {
                if success {
                    if confirmations >= WAIT_CONFIRMATIONS {
                        info!(
                            "Operation {}, {}  block: {}, committed, tx: {:#x}",
                            op.operation.id.unwrap(),
                            op.operation.action.to_string(),
                            op.operation.block.block_number,
                            tx.signed_tx.hash,
                        );
                        self.save_completed_tx_to_db(&tx.signed_tx.hash)?;
                        return Ok(true);
                    } else {
                        info!("Transaction committed, waiting for confirmations: hash {:#x}, confirmations: {}", tx.signed_tx.hash, confirmations);
                        return Ok(false);
                    }
                } else {
                    // TODO check confirmations for fail
                    warn!(
                        "ETH transaction failed: tx: {:#x}, operation_id: {} ",
                        tx.signed_tx.hash,
                        op.operation.id.unwrap()
                    );
                    failed_txs.insert(tx.signed_tx.hash);
                }
            } else {
                let stuck = tx.is_stuck(current_block);
                last_pending_tx = Some((tx.signed_tx.hash, stuck));
            }
        }
        // forget about failed txs
        //        op.txs.retain(|tx| !failed_txs.contains(&tx.signed_tx.hash));

        // if stuck/not sent yet -> send new tx
        let deadline_block = current_block + EXPECTED_WAIT_TIME_BLOCKS;
        let new_tx = if let Some((pending_tx_hash, stuck)) = last_pending_tx {
            // resend
            if stuck {
                warn!("Transaction stuck: {:#x}", pending_tx_hash);
                let stuck_tx = op
                    .txs
                    .iter()
                    .find(|tx| tx.signed_tx.hash == pending_tx_hash);
                let new_tx =
                    self.create_and_save_new_tx(&op.operation, deadline_block, stuck_tx)?;
                Some(new_tx)
            } else {
                None
            }
        } else {
            let new_tx = self.create_and_save_new_tx(&op.operation, deadline_block, None)?;
            Some(new_tx)
        };
        if let Some(new_tx) = new_tx {
            op.txs.push(new_tx.clone());
            info!(
                "Sending tx for op, op_id: {} tx_hash: {:#x}",
                new_tx.op_id, new_tx.signed_tx.hash
            );
            self.send_tx(&new_tx.signed_tx)?;
        }

        Ok(false)
    }

    fn send_tx(&self, signed_tx: &SignedCallResult) -> Result<(), failure::Error> {
        let hash = block_on(self.eth_client.send_raw_tx(signed_tx.raw_tx.clone()))?;
        ensure!(
            hash == signed_tx.hash,
            "Hash from signer and Ethereum node mismatch"
        );
        Ok(())
    }

    fn create_and_save_new_tx(
        &self,
        op: &Operation,
        deadline_block: u64,
        stuck_tx: Option<&TransactionETHState>,
    ) -> Result<TransactionETHState, failure::Error> {
        // if transaction was stuck we better to up gas price.
        let tx_options = if let Some(stuck_tx) = stuck_tx {
            let old_tx_gas_price =
                U256::from_dec_str(&stuck_tx.signed_tx.gas_price.to_string()).unwrap();
            let new_gas_price = {
                let network_price = block_on(self.eth_client.get_gas_price())?;
                // replacement price should be at least 10% higher, we make it 15% higher.
                let replacement_price = (old_tx_gas_price * U256::from(115)) / U256::from(100);
                std::cmp::max(network_price, replacement_price)
            };

            let new_nonce = block_on(self.eth_client.current_nonce())?;

            info!(
                "Replacing tx: hash: {:#x}, old_gas: {}, new_gas: {}, old_nonce: {}, new_nonce: {}",
                stuck_tx.signed_tx.hash,
                old_tx_gas_price,
                new_gas_price,
                stuck_tx.signed_tx.nonce,
                new_nonce
            );

            Options::with(move |opt| {
                opt.gas_price = Some(new_gas_price);
                opt.nonce = Some(new_nonce);
            })
        } else {
            Options::default()
        };

        //        // FAIL TEST
        //        let rnd: u64 = rand::thread_rng().gen_range(0,10);
        //        if rnd < 3 {
        //            error!("Messing with nonce");
        //            let mut committed_nonce = self.eth_client.current_nonce().wait()?;
        //            committed_nonce += (rnd + 1).into();
        //            tx_options.nonce = Some(committed_nonce);
        //        }
        //        // TEST

        let signed_tx = self.sign_operation_tx(op, tx_options)?;
        let new_transaction = TransactionETHState {
            op_id: op.id.unwrap(),
            deadline_block,
            signed_tx,
        };
        self.save_signed_tx_to_db(&new_transaction)?;
        trace!(
            "Signed new ETH: tx_hash {:#?}",
            new_transaction.signed_tx.hash
        );
        Ok(new_transaction)
    }

    fn sign_operation_tx(
        &self,
        op: &Operation,
        tx_options: Options,
    ) -> Result<SignedCallResult, failure::Error> {
        match &op.action {
            Action::Commit => {
                //                let mut block_number = op.block.block_number;
                //                // FAIL TEST
                //                let rnd = rand::thread_rng().gen_range(0,10);
                //                if rnd < 5 {
                //                    error!("Messing with tx pubdata");
                //                    block_number += rnd + 1;
                //                }
                //                // FAIL TEST
                let root = op.block.get_eth_encoded_root();

                let public_data = op.block.get_eth_public_data();
                debug!(
                    "public_data for block_number {}: {}",
                    op.block.block_number,
                    hex::encode(&public_data)
                );

                // function commitBlock(uint32 _blockNumber, uint24 _feeAccount, bytes32 _newRoot, bytes calldata _publicData)
                block_on(self.eth_client.sign_call_tx(
                    "commitBlock",
                    (
                        u64::from(op.block.block_number),
                        u64::from(op.block.fee_account),
                        root,
                        public_data,
                    ),
                    tx_options,
                ))
            }
            Action::Verify { proof } => {
                // function verifyBlock(uint32 _blockNumber, uint256[8] calldata proof) external {
                block_on(self.eth_client.sign_call_tx(
                    "verifyBlock",
                    (u64::from(op.block.block_number), *proof.clone()),
                    tx_options,
                ))
            }
        }
    }

    fn call_complete_withdrawals(&self) -> Result<(), failure::Error> {
        // function completeWithdrawals(uint32 _n) external {
        let tx = block_on(self.eth_client.sign_call_tx(
            "completeWithdrawals",
            config::MAX_WITHDRAWALS_TO_COMPLETE_IN_A_CALL,
            Options::default(),
        ))
        .map_err(|e| failure::format_err!("completeWithdrawals: {}", e))?;
        info!("Sending completeWithdrawals tx with hash: {:#?}", tx.hash);
        self.send_tx(&tx)
    }
}

pub fn start_eth_sender(
    pool: ConnectionPool,
    panic_notify: mpsc::Sender<bool>,
    op_notify_sender: mpsc::Sender<Operation>,
    send_requst_receiver: mpsc::Receiver<Operation>,
    config_options: ConfigurationOptions,
) {
    std::thread::Builder::new()
        .name("eth_sender".to_string())
        .spawn(move || {
            let _panic_sentinel = ThreadPanicNotify(panic_notify);
            let (_event_loop, transport) =
                Http::new(&config_options.web3_url).expect("failed to start web3 transport");

            let eth_client = ETHClient::new(
                transport,
                zksync_contract(),
                config_options.operator_eth_addr,
                config_options.operator_private_key,
                config_options.contract_eth_addr,
                config_options.chain_id,
                config_options.gas_price_factor,
            );

            let mut runtime = Runtime::new().expect("eth-sender-runtime");
            let eth_sender =
                ETHSender::new(pool, eth_client, send_requst_receiver, op_notify_sender);
            runtime.block_on(eth_sender.run());
        })
        .expect("Eth sender thread");
}