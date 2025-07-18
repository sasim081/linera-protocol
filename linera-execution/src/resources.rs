// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! This module tracks the resources used during the execution of a transaction.

use std::{sync::Arc, time::Duration};

use custom_debug_derive::Debug;
use linera_base::{
    data_types::{Amount, ArithmeticError, Blob},
    ensure,
    identifiers::AccountOwner,
    ownership::ChainOwnership,
    vm::VmRuntime,
};
use linera_views::{context::Context, ViewError};
use serde::Serialize;

use crate::{ExecutionError, Message, Operation, ResourceControlPolicy, SystemExecutionStateView};

#[derive(Clone, Debug, Default)]
pub struct ResourceController<Account = Amount, Tracker = ResourceTracker> {
    /// The (fixed) policy used to charge fees and control resource usage.
    policy: Arc<ResourceControlPolicy>,
    /// How the resources were used so far.
    pub tracker: Tracker,
    /// The account paying for the resource usage.
    pub account: Account,
}

impl<Account, Tracker> ResourceController<Account, Tracker> {
    /// Creates a new resource controller with the given policy and account.
    pub fn new(policy: Arc<ResourceControlPolicy>, tracker: Tracker, account: Account) -> Self {
        Self {
            policy,
            tracker,
            account,
        }
    }

    /// Returns a reference to the policy.
    pub fn policy(&self) -> &Arc<ResourceControlPolicy> {
        &self.policy
    }

    /// Returns a reference to the tracker.
    pub fn tracker(&self) -> &Tracker {
        &self.tracker
    }
}

/// The runtime size of an `Amount`.
pub const RUNTIME_AMOUNT_SIZE: u32 = 16;

/// The runtime size of a `ApplicationId`.
pub const RUNTIME_APPLICATION_ID_SIZE: u32 = 32;

/// The runtime size of a `BlockHeight`.
pub const RUNTIME_BLOCK_HEIGHT_SIZE: u32 = 8;

/// The runtime size of a `ChainId`.
pub const RUNTIME_CHAIN_ID_SIZE: u32 = 32;

/// The runtime size of a `Timestamp`.
pub const RUNTIME_TIMESTAMP_SIZE: u32 = 8;

/// The runtime size of the weight of an owner.
pub const RUNTIME_OWNER_WEIGHT_SIZE: u32 = 8;

/// The runtime constant part size of the `ChainOwnership`.
/// It consists of one `u32` and four `TimeDelta` which are the constant part of
/// the `ChainOwnership`. The way we do it is not optimal:
/// TODO(#4164): Implement a procedure for computing naive sizes.
pub const RUNTIME_CONSTANT_CHAIN_OWNERSHIP_SIZE: u32 = 4 + 4 * 8;

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use linera_base::{
        data_types::{Amount, BlockHeight, Timestamp},
        identifiers::{ApplicationId, ChainId},
    };

    use crate::resources::{
        RUNTIME_AMOUNT_SIZE, RUNTIME_APPLICATION_ID_SIZE, RUNTIME_BLOCK_HEIGHT_SIZE,
        RUNTIME_CHAIN_ID_SIZE, RUNTIME_OWNER_WEIGHT_SIZE, RUNTIME_TIMESTAMP_SIZE,
    };

    #[test]
    fn test_size_of_runtime_operations() {
        assert_eq!(RUNTIME_AMOUNT_SIZE as usize, size_of::<Amount>());
        assert_eq!(
            RUNTIME_APPLICATION_ID_SIZE as usize,
            size_of::<ApplicationId>()
        );
        assert_eq!(RUNTIME_BLOCK_HEIGHT_SIZE as usize, size_of::<BlockHeight>());
        assert_eq!(RUNTIME_CHAIN_ID_SIZE as usize, size_of::<ChainId>());
        assert_eq!(RUNTIME_TIMESTAMP_SIZE as usize, size_of::<Timestamp>());
        assert_eq!(RUNTIME_OWNER_WEIGHT_SIZE as usize, size_of::<u64>());
    }
}

/// The resources used so far by an execution process.
/// Acts as an accumulator for all resources consumed during
/// a specific execution flow. This could be the execution of a block,
/// the processing of a single message, or a specific phase within these
/// broader operations.
#[derive(Copy, Debug, Clone, Default)]
pub struct ResourceTracker {
    /// The total size of the block so far.
    pub block_size: u64,
    /// The EVM fuel used so far.
    pub evm_fuel: u64,
    /// The Wasm fuel used so far.
    pub wasm_fuel: u64,
    /// The number of read operations.
    pub read_operations: u32,
    /// The number of write operations.
    pub write_operations: u32,
    /// The size of bytes read from runtime.
    pub bytes_runtime: u32,
    /// The number of bytes read.
    pub bytes_read: u64,
    /// The number of bytes written.
    pub bytes_written: u64,
    /// The number of blobs read.
    pub blobs_read: u32,
    /// The number of blobs published.
    pub blobs_published: u32,
    /// The number of blob bytes read.
    pub blob_bytes_read: u64,
    /// The number of blob bytes published.
    pub blob_bytes_published: u64,
    /// The change in the number of bytes being stored by user applications.
    pub bytes_stored: i32,
    /// The number of operations executed.
    pub operations: u32,
    /// The total size of the arguments of user operations.
    pub operation_bytes: u64,
    /// The number of outgoing messages created (system and user).
    pub messages: u32,
    /// The total size of the arguments of outgoing user messages.
    pub message_bytes: u64,
    /// The number of HTTP requests performed.
    pub http_requests: u32,
    /// The number of calls to services as oracles.
    pub service_oracle_queries: u32,
    /// The time spent executing services as oracles.
    pub service_oracle_execution: Duration,
    /// The amount allocated to message grants.
    pub grants: Amount,
}

impl ResourceTracker {
    fn fuel(&self, vm_runtime: VmRuntime) -> u64 {
        match vm_runtime {
            VmRuntime::Wasm => self.wasm_fuel,
            VmRuntime::Evm => self.evm_fuel,
        }
    }
}

/// How to access the balance of an account.
pub trait BalanceHolder {
    fn balance(&self) -> Result<Amount, ArithmeticError>;

    fn try_add_assign(&mut self, other: Amount) -> Result<(), ArithmeticError>;

    fn try_sub_assign(&mut self, other: Amount) -> Result<(), ArithmeticError>;
}

// The main accounting functions for a ResourceController.
impl<Account, Tracker> ResourceController<Account, Tracker>
where
    Account: BalanceHolder,
    Tracker: AsRef<ResourceTracker> + AsMut<ResourceTracker>,
{
    /// Obtains the balance of the account. The only possible error is an arithmetic
    /// overflow, which should not happen in practice due to final token supply.
    pub fn balance(&self) -> Result<Amount, ArithmeticError> {
        self.account.balance()
    }

    /// Operates a 3-way merge by transferring the difference between `initial`
    /// and `other` to `self`.
    pub fn merge_balance(&mut self, initial: Amount, other: Amount) -> Result<(), ExecutionError> {
        if other <= initial {
            let sub_amount = initial.try_sub(other).expect("other <= initial");
            self.account.try_sub_assign(sub_amount).map_err(|_| {
                ExecutionError::FeesExceedFunding {
                    fees: sub_amount,
                    balance: self.balance().unwrap_or(Amount::MAX),
                }
            })?;
        } else {
            self.account
                .try_add_assign(other.try_sub(initial).expect("other > initial"))?;
        }
        Ok(())
    }

    /// Subtracts an amount from a balance and reports an error if that is impossible.
    fn update_balance(&mut self, fees: Amount) -> Result<(), ExecutionError> {
        self.account
            .try_sub_assign(fees)
            .map_err(|_| ExecutionError::FeesExceedFunding {
                fees,
                balance: self.balance().unwrap_or(Amount::MAX),
            })?;
        Ok(())
    }

    /// Obtains the amount of fuel that could be spent by consuming the entire balance.
    pub(crate) fn remaining_fuel(&self, vm_runtime: VmRuntime) -> u64 {
        let balance = self.balance().unwrap_or(Amount::MAX);
        let fuel = self.tracker.as_ref().fuel(vm_runtime);
        let maximum_fuel_per_block = self.policy.maximum_fuel_per_block(vm_runtime);
        self.policy
            .remaining_fuel(balance, vm_runtime)
            .min(maximum_fuel_per_block.saturating_sub(fuel))
    }

    /// Tracks the allocation of a grant.
    pub fn track_grant(&mut self, grant: Amount) -> Result<(), ExecutionError> {
        self.tracker.as_mut().grants.try_add_assign(grant)?;
        self.update_balance(grant)
    }

    /// Tracks the execution of an operation in block.
    pub fn track_operation(&mut self, operation: &Operation) -> Result<(), ExecutionError> {
        self.tracker.as_mut().operations = self
            .tracker
            .as_mut()
            .operations
            .checked_add(1)
            .ok_or(ArithmeticError::Overflow)?;
        self.update_balance(self.policy.operation)?;
        match operation {
            Operation::System(_) => Ok(()),
            Operation::User { bytes, .. } => {
                let size = bytes.len();
                self.tracker.as_mut().operation_bytes = self
                    .tracker
                    .as_mut()
                    .operation_bytes
                    .checked_add(size as u64)
                    .ok_or(ArithmeticError::Overflow)?;
                self.update_balance(self.policy.operation_bytes_price(size as u64)?)?;
                Ok(())
            }
        }
    }

    /// Tracks the creation of an outgoing message.
    pub fn track_message(&mut self, message: &Message) -> Result<(), ExecutionError> {
        self.tracker.as_mut().messages = self
            .tracker
            .as_mut()
            .messages
            .checked_add(1)
            .ok_or(ArithmeticError::Overflow)?;
        self.update_balance(self.policy.message)?;
        match message {
            Message::System(_) => Ok(()),
            Message::User { bytes, .. } => {
                let size = bytes.len();
                self.tracker.as_mut().message_bytes = self
                    .tracker
                    .as_mut()
                    .message_bytes
                    .checked_add(size as u64)
                    .ok_or(ArithmeticError::Overflow)?;
                self.update_balance(self.policy.message_bytes_price(size as u64)?)?;
                Ok(())
            }
        }
    }

    /// Tracks the execution of an HTTP request.
    pub fn track_http_request(&mut self) -> Result<(), ExecutionError> {
        self.tracker.as_mut().http_requests = self
            .tracker
            .as_ref()
            .http_requests
            .checked_add(1)
            .ok_or(ArithmeticError::Overflow)?;
        self.update_balance(self.policy.http_request)
    }

    /// Tracks a number of fuel units used.
    pub(crate) fn track_fuel(
        &mut self,
        fuel: u64,
        vm_runtime: VmRuntime,
    ) -> Result<(), ExecutionError> {
        match vm_runtime {
            VmRuntime::Wasm => {
                self.tracker.as_mut().wasm_fuel = self
                    .tracker
                    .as_ref()
                    .wasm_fuel
                    .checked_add(fuel)
                    .ok_or(ArithmeticError::Overflow)?;
                ensure!(
                    self.tracker.as_ref().wasm_fuel <= self.policy.maximum_wasm_fuel_per_block,
                    ExecutionError::MaximumFuelExceeded(vm_runtime)
                );
            }
            VmRuntime::Evm => {
                self.tracker.as_mut().evm_fuel = self
                    .tracker
                    .as_ref()
                    .evm_fuel
                    .checked_add(fuel)
                    .ok_or(ArithmeticError::Overflow)?;
                ensure!(
                    self.tracker.as_ref().evm_fuel <= self.policy.maximum_evm_fuel_per_block,
                    ExecutionError::MaximumFuelExceeded(vm_runtime)
                );
            }
        }
        self.update_balance(self.policy.fuel_price(fuel, vm_runtime)?)
    }

    /// Tracks runtime reading of `ChainId`
    pub(crate) fn track_runtime_chain_id(&mut self) -> Result<(), ExecutionError> {
        self.track_size_runtime_operations(RUNTIME_CHAIN_ID_SIZE)
    }

    /// Tracks runtime reading of `BlockHeight`
    pub(crate) fn track_runtime_block_height(&mut self) -> Result<(), ExecutionError> {
        self.track_size_runtime_operations(RUNTIME_BLOCK_HEIGHT_SIZE)
    }

    /// Tracks runtime reading of `ApplicationId`
    pub(crate) fn track_runtime_application_id(&mut self) -> Result<(), ExecutionError> {
        self.track_size_runtime_operations(RUNTIME_APPLICATION_ID_SIZE)
    }

    /// Tracks runtime reading of application parameters.
    pub(crate) fn track_runtime_application_parameters(
        &mut self,
        parameters: &[u8],
    ) -> Result<(), ExecutionError> {
        let parameters_len = parameters.len() as u32;
        self.track_size_runtime_operations(parameters_len)
    }

    /// Tracks runtime reading of `Timestamp`
    pub(crate) fn track_runtime_timestamp(&mut self) -> Result<(), ExecutionError> {
        self.track_size_runtime_operations(RUNTIME_TIMESTAMP_SIZE)
    }

    /// Tracks runtime reading of balance
    pub(crate) fn track_runtime_balance(&mut self) -> Result<(), ExecutionError> {
        self.track_size_runtime_operations(RUNTIME_AMOUNT_SIZE)
    }

    /// Tracks runtime reading of owner balances
    pub(crate) fn track_runtime_owner_balances(
        &mut self,
        owner_balances: &[(AccountOwner, Amount)],
    ) -> Result<(), ExecutionError> {
        let mut size = 0;
        for (account_owner, _) in owner_balances {
            size += account_owner.size() + RUNTIME_AMOUNT_SIZE;
        }
        self.track_size_runtime_operations(size)
    }

    /// Tracks runtime reading of owners
    pub(crate) fn track_runtime_owners(
        &mut self,
        owners: &[AccountOwner],
    ) -> Result<(), ExecutionError> {
        let mut size = 0;
        for owner in owners {
            size += owner.size();
        }
        self.track_size_runtime_operations(size)
    }

    /// Tracks runtime reading of owners
    pub(crate) fn track_runtime_chain_ownership(
        &mut self,
        chain_ownership: &ChainOwnership,
    ) -> Result<(), ExecutionError> {
        let mut size = 0;
        for account_owner in &chain_ownership.super_owners {
            size += account_owner.size();
        }
        for account_owner in chain_ownership.owners.keys() {
            size += account_owner.size() + RUNTIME_OWNER_WEIGHT_SIZE;
        }
        size += RUNTIME_CONSTANT_CHAIN_OWNERSHIP_SIZE;
        self.track_size_runtime_operations(size)
    }

    /// Tracks runtime operations.
    fn track_size_runtime_operations(&mut self, size: u32) -> Result<(), ExecutionError> {
        self.tracker.as_mut().bytes_runtime = self
            .tracker
            .as_mut()
            .bytes_runtime
            .checked_add(size)
            .ok_or(ArithmeticError::Overflow)?;
        self.update_balance(self.policy.bytes_runtime_price(size)?)
    }

    /// Tracks a read operation.
    pub(crate) fn track_read_operation(&mut self) -> Result<(), ExecutionError> {
        self.tracker.as_mut().read_operations = self
            .tracker
            .as_mut()
            .read_operations
            .checked_add(1)
            .ok_or(ArithmeticError::Overflow)?;
        self.update_balance(self.policy.read_operations_price(1)?)
    }

    /// Tracks a write operation.
    pub(crate) fn track_write_operations(&mut self, count: u32) -> Result<(), ExecutionError> {
        self.tracker.as_mut().write_operations = self
            .tracker
            .as_mut()
            .write_operations
            .checked_add(count)
            .ok_or(ArithmeticError::Overflow)?;
        self.update_balance(self.policy.write_operations_price(count)?)
    }

    /// Tracks a number of bytes read.
    pub(crate) fn track_bytes_read(&mut self, count: u64) -> Result<(), ExecutionError> {
        self.tracker.as_mut().bytes_read = self
            .tracker
            .as_mut()
            .bytes_read
            .checked_add(count)
            .ok_or(ArithmeticError::Overflow)?;
        if self.tracker.as_mut().bytes_read >= self.policy.maximum_bytes_read_per_block {
            return Err(ExecutionError::ExcessiveRead);
        }
        self.update_balance(self.policy.bytes_read_price(count)?)?;
        Ok(())
    }

    /// Tracks a number of bytes written.
    pub(crate) fn track_bytes_written(&mut self, count: u64) -> Result<(), ExecutionError> {
        self.tracker.as_mut().bytes_written = self
            .tracker
            .as_mut()
            .bytes_written
            .checked_add(count)
            .ok_or(ArithmeticError::Overflow)?;
        if self.tracker.as_mut().bytes_written >= self.policy.maximum_bytes_written_per_block {
            return Err(ExecutionError::ExcessiveWrite);
        }
        self.update_balance(self.policy.bytes_written_price(count)?)?;
        Ok(())
    }

    /// Tracks a number of blob bytes written.
    pub(crate) fn track_blob_read(&mut self, count: u64) -> Result<(), ExecutionError> {
        {
            let tracker = self.tracker.as_mut();
            tracker.blob_bytes_read = tracker
                .blob_bytes_read
                .checked_add(count)
                .ok_or(ArithmeticError::Overflow)?;
            tracker.blobs_read = tracker
                .blobs_read
                .checked_add(1)
                .ok_or(ArithmeticError::Overflow)?;
        }
        self.update_balance(self.policy.blob_read_price(count)?)?;
        Ok(())
    }

    /// Tracks a number of blob bytes published.
    pub fn track_blob_published(&mut self, blob: &Blob) -> Result<(), ExecutionError> {
        self.policy.check_blob_size(blob.content())?;
        let size = blob.content().bytes().len() as u64;
        if blob.is_committee_blob() {
            return Ok(());
        }
        {
            let tracker = self.tracker.as_mut();
            tracker.blob_bytes_published = tracker
                .blob_bytes_published
                .checked_add(size)
                .ok_or(ArithmeticError::Overflow)?;
            tracker.blobs_published = tracker
                .blobs_published
                .checked_add(1)
                .ok_or(ArithmeticError::Overflow)?;
        }
        self.update_balance(self.policy.blob_published_price(size)?)?;
        Ok(())
    }

    /// Tracks a change in the number of bytes stored.
    // TODO(#1536): This is not fully implemented.
    #[allow(dead_code)]
    pub(crate) fn track_stored_bytes(&mut self, delta: i32) -> Result<(), ExecutionError> {
        self.tracker.as_mut().bytes_stored = self
            .tracker
            .as_mut()
            .bytes_stored
            .checked_add(delta)
            .ok_or(ArithmeticError::Overflow)?;
        Ok(())
    }

    /// Returns the remaining time services can spend executing as oracles.
    pub(crate) fn remaining_service_oracle_execution_time(
        &self,
    ) -> Result<Duration, ExecutionError> {
        let tracker = self.tracker.as_ref();
        let spent_execution_time = tracker.service_oracle_execution;
        let limit = Duration::from_millis(self.policy.maximum_service_oracle_execution_ms);

        limit
            .checked_sub(spent_execution_time)
            .ok_or(ExecutionError::MaximumServiceOracleExecutionTimeExceeded)
    }

    /// Tracks a call to a service to run as an oracle.
    pub(crate) fn track_service_oracle_call(&mut self) -> Result<(), ExecutionError> {
        self.tracker.as_mut().service_oracle_queries = self
            .tracker
            .as_mut()
            .service_oracle_queries
            .checked_add(1)
            .ok_or(ArithmeticError::Overflow)?;
        self.update_balance(self.policy.service_as_oracle_query)
    }

    /// Tracks the time spent executing the service as an oracle.
    pub(crate) fn track_service_oracle_execution(
        &mut self,
        execution_time: Duration,
    ) -> Result<(), ExecutionError> {
        let tracker = self.tracker.as_mut();
        let spent_execution_time = &mut tracker.service_oracle_execution;
        let limit = Duration::from_millis(self.policy.maximum_service_oracle_execution_ms);

        *spent_execution_time = spent_execution_time.saturating_add(execution_time);

        ensure!(
            *spent_execution_time < limit,
            ExecutionError::MaximumServiceOracleExecutionTimeExceeded
        );

        Ok(())
    }

    /// Tracks the size of a response produced by an oracle.
    pub(crate) fn track_service_oracle_response(
        &mut self,
        response_bytes: usize,
    ) -> Result<(), ExecutionError> {
        ensure!(
            response_bytes as u64 <= self.policy.maximum_oracle_response_bytes,
            ExecutionError::ServiceOracleResponseTooLarge
        );

        Ok(())
    }
}

impl<Account, Tracker> ResourceController<Account, Tracker>
where
    Tracker: AsMut<ResourceTracker>,
{
    /// Tracks the serialized size of a block, or parts of it.
    pub fn track_block_size_of(&mut self, data: &impl Serialize) -> Result<(), ExecutionError> {
        self.track_block_size(bcs::serialized_size(data)?)
    }

    /// Tracks the serialized size of a block, or parts of it.
    pub fn track_block_size(&mut self, size: usize) -> Result<(), ExecutionError> {
        let tracker = self.tracker.as_mut();
        tracker.block_size = u64::try_from(size)
            .ok()
            .and_then(|size| tracker.block_size.checked_add(size))
            .ok_or(ExecutionError::BlockTooLarge)?;
        ensure!(
            tracker.block_size <= self.policy.maximum_block_size,
            ExecutionError::BlockTooLarge
        );
        Ok(())
    }
}

impl ResourceController<Option<AccountOwner>, ResourceTracker> {
    /// Provides a reference to the current execution state and obtains a temporary object
    /// where the accounting functions of [`ResourceController`] are available.
    pub async fn with_state<'a, C>(
        &mut self,
        view: &'a mut SystemExecutionStateView<C>,
    ) -> Result<ResourceController<Sources<'a>, &mut ResourceTracker>, ViewError>
    where
        C: Context + Clone + Send + Sync + 'static,
    {
        self.with_state_and_grant(view, None).await
    }

    /// Provides a reference to the current execution state as well as an optional grant,
    /// and obtains a temporary object where the accounting functions of
    /// [`ResourceController`] are available.
    pub async fn with_state_and_grant<'a, C>(
        &mut self,
        view: &'a mut SystemExecutionStateView<C>,
        grant: Option<&'a mut Amount>,
    ) -> Result<ResourceController<Sources<'a>, &mut ResourceTracker>, ViewError>
    where
        C: Context + Clone + Send + Sync + 'static,
    {
        let mut sources = Vec::new();
        // First, use the grant (e.g. for messages) and otherwise use the chain account
        // (e.g. for blocks and operations).
        if let Some(grant) = grant {
            sources.push(grant);
        } else {
            sources.push(view.balance.get_mut());
        }
        // Then the local account, if any. Currently, any negative fee (e.g. storage
        // refund) goes preferably to this account.
        if let Some(owner) = &self.account {
            if let Some(balance) = view.balances.get_mut(owner).await? {
                sources.push(balance);
            }
        }

        Ok(ResourceController {
            policy: self.policy.clone(),
            tracker: &mut self.tracker,
            account: Sources { sources },
        })
    }
}

// The simplest `BalanceHolder` is an `Amount`.
impl BalanceHolder for Amount {
    fn balance(&self) -> Result<Amount, ArithmeticError> {
        Ok(*self)
    }

    fn try_add_assign(&mut self, other: Amount) -> Result<(), ArithmeticError> {
        self.try_add_assign(other)
    }

    fn try_sub_assign(&mut self, other: Amount) -> Result<(), ArithmeticError> {
        self.try_sub_assign(other)
    }
}

// This is also needed for the default instantiation `ResourceController<Amount, ResourceTracker>`.
// See https://doc.rust-lang.org/std/convert/trait.AsMut.html#reflexivity for general context.
impl AsMut<ResourceTracker> for ResourceTracker {
    fn as_mut(&mut self) -> &mut Self {
        self
    }
}

impl AsRef<ResourceTracker> for ResourceTracker {
    fn as_ref(&self) -> &Self {
        self
    }
}

/// A temporary object holding a number of references to funding sources.
pub struct Sources<'a> {
    sources: Vec<&'a mut Amount>,
}

impl BalanceHolder for Sources<'_> {
    fn balance(&self) -> Result<Amount, ArithmeticError> {
        let mut amount = Amount::ZERO;
        for source in self.sources.iter() {
            amount.try_add_assign(**source)?;
        }
        Ok(amount)
    }

    fn try_add_assign(&mut self, other: Amount) -> Result<(), ArithmeticError> {
        // Try to credit the owner account first.
        // TODO(#1648): This may need some additional design work.
        let source = self.sources.last_mut().expect("at least one source");
        source.try_add_assign(other)
    }

    fn try_sub_assign(&mut self, mut other: Amount) -> Result<(), ArithmeticError> {
        for source in self.sources.iter_mut() {
            if source.try_sub_assign(other).is_ok() {
                return Ok(());
            }
            other.try_sub_assign(**source).expect("*source < other");
            **source = Amount::ZERO;
        }
        if other > Amount::ZERO {
            Err(ArithmeticError::Underflow)
        } else {
            Ok(())
        }
    }
}
