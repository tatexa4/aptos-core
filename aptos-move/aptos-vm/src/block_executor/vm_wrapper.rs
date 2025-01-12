// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    adapter_common::{PreprocessedTransaction, VMAdapter},
    aptos_vm::AptosVM,
    block_executor::AptosTransactionOutput,
    data_cache::StorageAdapter,
    move_vm_ext::write_op_converter::WriteOpConverter,
};
use aptos_block_executor::task::{ExecutionStatus, ExecutorTask};
use aptos_logger::{enabled, Level};
use aptos_mvhashmap::types::TxnIndex;
use aptos_state_view::StateView;
use aptos_types::{state_store::state_key::StateKey, write_set::WriteOp};
use aptos_vm_logging::{log_schema::AdapterLogSchema, prelude::*};
use bytes::Bytes;
use move_core_types::{
    effects::Op as MoveStorageOp,
    ident_str,
    language_storage::{ModuleId, CORE_CODE_ADDRESS},
    vm_status::VMStatus,
};

pub(crate) struct AptosExecutorTask<'a, S> {
    vm: AptosVM,
    base_view: &'a S,
}

impl<'a, S: 'a + StateView + Sync> ExecutorTask for AptosExecutorTask<'a, S> {
    type Argument = &'a S;
    type Error = VMStatus;
    type Output = AptosTransactionOutput;
    type Txn = PreprocessedTransaction;

    fn init(argument: &'a S) -> Self {
        // AptosVM has to be initialized using configs from storage.
        // Using adapter allows us to fetch those.
        // TODO: with new adapter we can relax trait bounds on S and avoid
        // creating `StorageAdapter` here.
        let config_storage = StorageAdapter::new(argument);
        let vm = AptosVM::new(&config_storage);

        // Loading `0x1::account` and its transitive dependency into the code cache.
        //
        // This should give us a warm VM to avoid the overhead of VM cold start.
        // Result of this load could be omitted as this is a best effort approach and won't hurt if that fails.
        //
        // Loading up `0x1::account` should be sufficient as this is the most common module
        // used for prologue, epilogue and transfer functionality.

        let _ = vm.load_module(
            &ModuleId::new(CORE_CODE_ADDRESS, ident_str!("account").to_owned()),
            &vm.as_move_resolver(argument),
        );

        Self {
            vm,
            base_view: argument,
        }
    }

    // This function is called by the BlockExecutor for each transaction is intends
    // to execute (via the ExecutorTask trait). It can be as a part of sequential
    // execution, or speculatively as a part of a parallel execution.
    fn execute_transaction(
        &self,
        view: &impl StateView,
        txn: &PreprocessedTransaction,
        txn_idx: TxnIndex,
        materialize_deltas: bool,
    ) -> ExecutionStatus<AptosTransactionOutput, VMStatus> {
        let log_context = AdapterLogSchema::new(self.base_view.id(), txn_idx as usize);

        match self
            .vm
            .execute_single_transaction(txn, &self.vm.as_move_resolver(view), &log_context)
        {
            Ok((vm_status, mut vm_output, sender)) => {
                if materialize_deltas {
                    // TODO: Integrate aggregator v2.
                    vm_output = vm_output
                        .try_materialize(view)
                        .expect("Delta materialization failed");
                }

                if vm_output.status().is_discarded() {
                    match sender {
                        Some(s) => speculative_trace!(
                            &log_context,
                            format!(
                                "Transaction discarded, sender: {}, error: {:?}",
                                s, vm_status
                            ),
                        ),
                        None => {
                            speculative_trace!(
                                &log_context,
                                format!("Transaction malformed, error: {:?}", vm_status),
                            )
                        },
                    };
                }
                if AptosVM::should_restart_execution(&vm_output) {
                    speculative_info!(
                        &log_context,
                        "Reconfiguration occurred: restart required".into()
                    );
                    ExecutionStatus::SkipRest(AptosTransactionOutput::new(vm_output))
                } else {
                    ExecutionStatus::Success(AptosTransactionOutput::new(vm_output))
                }
            },
            Err(err) => ExecutionStatus::Abort(err),
        }
    }

    fn convert_to_value(
        &self,
        view: &impl StateView,
        key: &StateKey,
        maybe_blob: Option<Bytes>,
        creation: bool,
    ) -> anyhow::Result<WriteOp> {
        let storage_adapter = self.vm.as_move_resolver(view);
        let wop_converter =
            WriteOpConverter::new(&storage_adapter, self.vm.is_storage_slot_metadata_enabled());

        let move_op = match maybe_blob {
            Some(blob) => {
                if creation {
                    MoveStorageOp::New(blob)
                } else {
                    MoveStorageOp::Modify(blob)
                }
            },
            None => MoveStorageOp::Delete,
        };

        wop_converter
            .convert(key, move_op, false)
            .map_err(|_| anyhow::Error::msg("Error on converting to WriteOp"))
    }
}
