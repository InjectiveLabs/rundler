use std::{
    collections::{HashMap, HashSet},
    mem,
    ops::Deref,
    sync::Arc,
};

use ethers::{
    abi::AbiDecode,
    types::{Address, BlockId, Opcode, H256, U256},
};
use indexmap::IndexSet;
#[cfg(test)]
use mockall::automock;
use rundler_provider::{AggregatorOut, AggregatorSimOut, EntryPoint, Provider};
use rundler_types::{contracts::i_entry_point::FailedOp, Entity, EntityType, UserOperation};
use tonic::async_trait;

use super::{
    mempool::{match_mempools, MempoolMatchResult},
    tracer::parse_combined_tracer_str,
};
use crate::common::{
    eth,
    mempool::MempoolConfig,
    tracer::{
        AssociatedSlotsByAddress, SimulateValidationTracer, SimulateValidationTracerImpl,
        SimulationTracerOutput, StorageAccess,
    },
    types::{
        ExpectedStorage, StakeInfo, ValidTimeRange, ValidationOutput, ValidationReturnInfo,
        ViolationError,
    },
};

#[derive(Clone, Debug, Default)]
pub struct SimulationSuccess {
    pub mempools: Vec<H256>,
    pub block_hash: H256,
    pub pre_op_gas: U256,
    pub valid_time_range: ValidTimeRange,
    pub aggregator: Option<AggregatorSimOut>,
    pub code_hash: H256,
    pub entities_needing_stake: Vec<EntityType>,
    pub account_is_staked: bool,
    pub accessed_addresses: HashSet<Address>,
    pub expected_storage: ExpectedStorage,
}

impl SimulationSuccess {
    pub fn aggregator_address(&self) -> Option<Address> {
        self.aggregator.as_ref().map(|agg| agg.address)
    }
}

pub type SimulationError = ViolationError<SimulationViolation>;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct StorageSlot {
    pub address: Address,
    pub slot: U256,
}

#[cfg_attr(test, automock)]
#[async_trait]
pub trait Simulator: Send + Sync + 'static {
    async fn simulate_validation(
        &self,
        op: UserOperation,
        block_hash: Option<H256>,
        expected_code_hash: Option<H256>,
    ) -> Result<SimulationSuccess, SimulationError>;
}

#[derive(Debug)]
pub struct SimulatorImpl<P: Provider, E: EntryPoint> {
    provider: Arc<P>,
    entry_point: Arc<E>,
    simulate_validation_tracer: SimulateValidationTracerImpl<P, E>,
    sim_settings: Settings,
    mempool_configs: HashMap<H256, MempoolConfig>,
}

impl<P, E> SimulatorImpl<P, E>
where
    P: Provider,
    E: EntryPoint,
{
    pub fn new(
        provider: Arc<P>,
        entry_point: E,
        sim_settings: Settings,
        mempool_configs: HashMap<H256, MempoolConfig>,
    ) -> Self {
        let entry_point = Arc::new(entry_point);
        let simulate_validation_tracer =
            SimulateValidationTracerImpl::new(Arc::clone(&provider), Arc::clone(&entry_point));
        Self {
            provider,
            entry_point,
            simulate_validation_tracer: simulate_validation_tracer,
            sim_settings,
            mempool_configs,
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.sim_settings
    }

    // Run the tracer and transform the output.
    // Any violations during this stage are errors.
    async fn create_context(
        &self,
        op: UserOperation,
        block_id: BlockId,
    ) -> Result<ValidationContext, SimulationError> {
        let factory_address = op.factory();
        let sender_address = op.sender;
        let paymaster_address = op.paymaster();
        let tracer_out = self
            .simulate_validation_tracer
            .trace_simulate_validation(op.clone(), block_id, self.sim_settings.max_verification_gas)
            .await?;
        let num_phases = tracer_out.phases.len() as u32;
        // Check if there are too many phases here, then check too few at the
        // end. We are detecting cases where the entry point is broken. Too many
        // phases definitely means it's broken, but too few phases could still
        // mean the entry point is fine if one of the phases fails and it
        // doesn't reach the end of execution.
        if num_phases > 3 {
            Err(vec![SimulationViolation::WrongNumberOfPhases(num_phases)])?
        }
        let Some(ref revert_data) = tracer_out.revert_data else {
            Err(vec![SimulationViolation::DidNotRevert])?
        };
        let last_entity = entity_type_from_simulation_phase(tracer_out.phases.len() - 1).unwrap();

        if let Ok(failed_op) = FailedOp::decode_hex(revert_data) {
            let entity_addr = match last_entity {
                EntityType::Factory => factory_address,
                EntityType::Paymaster => paymaster_address,
                EntityType::Account => Some(sender_address),
                _ => None,
            };
            Err(vec![SimulationViolation::UnintendedRevertWithMessage(
                last_entity,
                failed_op.reason,
                entity_addr,
            )])?
        }
        let Ok(entry_point_out) = ValidationOutput::decode_hex(revert_data) else {
            Err(vec![SimulationViolation::UnintendedRevert(last_entity)])?
        };
        let entity_infos = EntityInfos::new(
            factory_address,
            sender_address,
            paymaster_address,
            &entry_point_out,
            self.sim_settings,
        );
        let is_unstaked_wallet_creation = entity_infos
            .get(EntityType::Factory)
            .filter(|factory| !factory.is_staked)
            .is_some();
        if num_phases < 3 {
            Err(vec![SimulationViolation::WrongNumberOfPhases(num_phases)])?
        };
        Ok(ValidationContext {
            block_id,
            entity_infos,
            tracer_out,
            entry_point_out,
            is_unstaked_wallet_creation,
            entities_needing_stake: vec![],
            accessed_addresses: HashSet::new(),
        })
    }

    async fn validate_aggregator_signature(
        &self,
        op: UserOperation,
        aggregator_address: Option<Address>,
        gas_cap: u64,
    ) -> anyhow::Result<AggregatorOut> {
        let Some(aggregator_address) = aggregator_address else {
            return Ok(AggregatorOut::NotNeeded);
        };

        self.provider
            .clone()
            .validate_user_op_signature(aggregator_address, op, gas_cap)
            .await
    }

    // Parse the output from tracing and return a list of violations.
    // Most violations found during this stage are allowlistable and can be added
    // to the list of allowlisted violations on a given mempool.
    fn gather_context_violations(
        &self,
        context: &mut ValidationContext,
    ) -> anyhow::Result<Vec<SimulationViolation>> {
        let &mut ValidationContext {
            ref entity_infos,
            ref tracer_out,
            ref entry_point_out,
            is_unstaked_wallet_creation,
            ref mut entities_needing_stake,
            ref mut accessed_addresses,
            ..
        } = context;

        let mut violations = vec![];

        if entry_point_out.return_info.sig_failed {
            violations.push(SimulationViolation::InvalidSignature);
        }

        let sender_address = entity_infos.sender_address();

        for (index, phase) in tracer_out.phases.iter().enumerate().take(3) {
            let kind = entity_type_from_simulation_phase(index).unwrap();
            let Some(entity_info) = entity_infos.get(kind) else {
                continue;
            };
            let entity = Entity {
                kind,
                address: entity_info.address,
            };
            for opcode in &phase.forbidden_opcodes_used {
                let (contract, opcode) = parse_combined_tracer_str(opcode)?;
                violations.push(SimulationViolation::UsedForbiddenOpcode(
                    entity,
                    contract,
                    ViolationOpCode(opcode),
                ));
            }
            for precompile in &phase.forbidden_precompiles_used {
                let (contract, precompile) = parse_combined_tracer_str(precompile)?;
                violations.push(SimulationViolation::UsedForbiddenPrecompile(
                    entity, contract, precompile,
                ));
            }
            let mut needs_stake = entity.kind == EntityType::Paymaster
                && !entry_point_out.return_info.paymaster_context.is_empty();
            let mut banned_slots_accessed = IndexSet::<StorageSlot>::new();
            for StorageAccess { address, slots } in &phase.storage_accesses {
                let address = *address;
                accessed_addresses.insert(address);
                for &slot in slots {
                    let restriction = get_storage_restriction(GetStorageRestrictionArgs {
                        slots_by_address: &tracer_out.associated_slots_by_address,
                        is_unstaked_wallet_creation,
                        entry_point_address: self.entry_point.address(),
                        entity_address: entity_info.address,
                        sender_address,
                        accessed_address: address,
                        slot,
                    });
                    match restriction {
                        StorageRestriction::Allowed => {}
                        StorageRestriction::NeedsStake => needs_stake = true,
                        StorageRestriction::Banned => {
                            banned_slots_accessed.insert(StorageSlot { address, slot });
                        }
                    }
                }
            }
            if needs_stake {
                entities_needing_stake.push(entity.kind);
                if !entity_info.is_staked {
                    violations.push(SimulationViolation::NotStaked(
                        entity,
                        self.sim_settings.min_stake_value.into(),
                        self.sim_settings.min_unstake_delay.into(),
                    ));
                }
            }
            for slot in banned_slots_accessed {
                violations.push(SimulationViolation::InvalidStorageAccess(entity, slot));
            }
            let non_sender_called_with_value = phase
                .addresses_calling_with_value
                .iter()
                .any(|address| address != &sender_address);
            if non_sender_called_with_value || phase.called_non_entry_point_with_value {
                violations.push(SimulationViolation::CallHadValue(entity));
            }
            if phase.called_banned_entry_point_method {
                violations.push(SimulationViolation::CalledBannedEntryPointMethod(entity));
            }

            // These violations are not allowlistable but we need to collect them here
            if phase.ran_out_of_gas {
                violations.push(SimulationViolation::OutOfGas(entity));
            }
            for &address in &phase.undeployed_contract_accesses {
                violations.push(SimulationViolation::AccessedUndeployedContract(
                    entity, address,
                ))
            }
        }

        if let Some(aggregator_info) = entry_point_out.aggregator_info {
            entities_needing_stake.push(EntityType::Aggregator);
            if !is_staked(aggregator_info.stake_info, self.sim_settings) {
                violations.push(SimulationViolation::NotStaked(
                    Entity::aggregator(aggregator_info.address),
                    self.sim_settings.min_stake_value.into(),
                    self.sim_settings.min_unstake_delay.into(),
                ));
            }
        }
        if tracer_out.factory_called_create2_twice {
            let factory = entity_infos.get(EntityType::Factory);
            match factory {
                Some(factory) => {
                    violations.push(SimulationViolation::FactoryCalledCreate2Twice(
                        factory.address,
                    ));
                }
                None => {
                    // weird case where CREATE2 is called > 1, but there isn't a factory
                    // defined. This should never happen, blame the violation on the entry point.
                    violations.push(SimulationViolation::FactoryCalledCreate2Twice(
                        self.entry_point.address(),
                    ));
                }
            }
        }

        Ok(violations)
    }

    // Check the code hash of the entities associated with the user operation
    // if needed, validate that the signature is valid for the aggregator.
    // Violations during this stage are always errors.
    async fn check_contracts(
        &self,
        op: UserOperation,
        context: &mut ValidationContext,
        expected_code_hash: Option<H256>,
    ) -> Result<(H256, Option<AggregatorSimOut>), SimulationError> {
        let &mut ValidationContext {
            block_id,
            ref mut tracer_out,
            ref entry_point_out,
            ..
        } = context;

        // collect a vector of violations to ensure a deterministic error message
        let mut violations = vec![];

        let aggregator_address = entry_point_out.aggregator_info.map(|info| info.address);
        let code_hash_future = eth::get_code_hash(
            self.provider.deref(),
            mem::take(&mut tracer_out.accessed_contract_addresses),
            Some(block_id),
        );
        let aggregator_signature_future = self.validate_aggregator_signature(
            op,
            aggregator_address,
            self.sim_settings.max_verification_gas,
        );

        let (code_hash, aggregator_out) =
            tokio::try_join!(code_hash_future, aggregator_signature_future)?;

        if let Some(expected_code_hash) = expected_code_hash {
            if expected_code_hash != code_hash {
                violations.push(SimulationViolation::CodeHashChanged)
            }
        }
        let aggregator = match aggregator_out {
            AggregatorOut::NotNeeded => None,
            AggregatorOut::SuccessWithInfo(info) => Some(info),
            AggregatorOut::ValidationReverted => {
                violations.push(SimulationViolation::AggregatorValidationFailed);
                None
            }
        };

        if !violations.is_empty() {
            return Err(violations.into());
        }

        Ok((code_hash, aggregator))
    }
}

#[async_trait]
impl<P, E> Simulator for SimulatorImpl<P, E>
where
    P: Provider,
    E: EntryPoint,
{
    async fn simulate_validation(
        &self,
        op: UserOperation,
        block_hash: Option<H256>,
        expected_code_hash: Option<H256>,
    ) -> Result<SimulationSuccess, SimulationError> {
        let block_hash = match block_hash {
            Some(block_hash) => block_hash,
            None => self.provider.get_latest_block_hash().await?,
        };
        let block_id = block_hash.into();
        let mut context = match self.create_context(op.clone(), block_id).await {
            Ok(context) => context,
            error @ Err(_) => error?,
        };

        // Gather all violations from the tracer
        let mut violations = self.gather_context_violations(&mut context)?;
        // Sort violations so that the final error message is deterministic
        violations.sort();
        // Check violations against mempool rules, find supporting mempools, error if none found
        let mempools = match match_mempools(&self.mempool_configs, &violations) {
            MempoolMatchResult::Matches(pools) => pools,
            MempoolMatchResult::NoMatch(i) => return Err(vec![violations[i].clone()].into()),
        };

        // Check code hash and aggregator signature, these can't fail
        let (code_hash, aggregator) = self
            .check_contracts(op, &mut context, expected_code_hash)
            .await?;

        // Transform outputs into success struct
        let ValidationContext {
            tracer_out,
            entry_point_out,
            is_unstaked_wallet_creation: _,
            entities_needing_stake,
            accessed_addresses,
            ..
        } = context;
        let ValidationOutput {
            return_info,
            sender_info,
            ..
        } = entry_point_out;
        let account_is_staked = is_staked(sender_info, self.sim_settings);
        let ValidationReturnInfo {
            pre_op_gas,
            valid_after,
            valid_until,
            ..
        } = return_info;
        Ok(SimulationSuccess {
            mempools,
            block_hash,
            pre_op_gas,
            valid_time_range: ValidTimeRange::new(valid_after, valid_until),
            aggregator,
            code_hash,
            entities_needing_stake,
            account_is_staked,
            accessed_addresses,
            expected_storage: tracer_out.expected_storage,
        })
    }
}

#[derive(Clone, Debug, parse_display::Display, Ord, Eq, PartialOrd, PartialEq)]
pub enum SimulationViolation {
    // Make sure to maintain the order here based on the importance
    // of the violation for converting to an JRPC error
    #[display("invalid signature")]
    InvalidSignature,
    #[display("reverted while simulating {0} validation: {1}")]
    UnintendedRevertWithMessage(EntityType, String, Option<Address>),
    #[display("{0.kind} uses banned opcode: {2} in contract {1:?}")]
    UsedForbiddenOpcode(Entity, Address, ViolationOpCode),
    #[display("{0.kind} uses banned precompile: {2:?} in contract {1:?}")]
    UsedForbiddenPrecompile(Entity, Address, Address),
    #[display(
        "{0.kind} tried to access code at {1} during validation, but that address is not a contract"
    )]
    AccessedUndeployedContract(Entity, Address),
    #[display("factory may only call CREATE2 once during initialization")]
    FactoryCalledCreate2Twice(Address),
    #[display("{0.kind} accessed forbidden storage at address {1:?} during validation")]
    InvalidStorageAccess(Entity, StorageSlot),
    #[display("{0.kind} called entry point method other than depositTo")]
    CalledBannedEntryPointMethod(Entity),
    #[display("{0.kind} must not send ETH during validation (except from account to entry point)")]
    CallHadValue(Entity),
    #[display("code accessed by validation has changed since the last time validation was run")]
    CodeHashChanged,
    #[display("{0.kind} must be staked")]
    NotStaked(Entity, U256, U256),
    #[display("reverted while simulating {0} validation")]
    UnintendedRevert(EntityType),
    #[display("simulateValidation did not revert. Make sure your EntryPoint is valid")]
    DidNotRevert,
    #[display("simulateValidation should have 3 parts but had {0} instead. Make sure your EntryPoint is valid")]
    WrongNumberOfPhases(u32),
    #[display("ran out of gas during {0.kind} validation")]
    OutOfGas(Entity),
    #[display("aggregator signature validation failed")]
    AggregatorValidationFailed,
}

#[derive(Debug, PartialEq, Clone, parse_display::Display, Eq)]
#[display("{0:?}")]
pub struct ViolationOpCode(pub Opcode);

impl PartialOrd for ViolationOpCode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ViolationOpCode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let left = self.0 as i32;
        let right = other.0 as i32;

        left.cmp(&right)
    }
}

fn entity_type_from_simulation_phase(i: usize) -> Option<EntityType> {
    match i {
        0 => Some(EntityType::Factory),
        1 => Some(EntityType::Account),
        2 => Some(EntityType::Paymaster),
        _ => None,
    }
}

#[derive(Debug)]
struct ValidationContext {
    block_id: BlockId,
    entity_infos: EntityInfos,
    tracer_out: SimulationTracerOutput,
    entry_point_out: ValidationOutput,
    is_unstaked_wallet_creation: bool,
    entities_needing_stake: Vec<EntityType>,
    accessed_addresses: HashSet<Address>,
}

#[derive(Clone, Copy, Debug)]
struct EntityInfo {
    pub address: Address,
    pub is_staked: bool,
}

#[derive(Clone, Copy, Debug)]
struct EntityInfos {
    factory: Option<EntityInfo>,
    sender: EntityInfo,
    paymaster: Option<EntityInfo>,
}

impl EntityInfos {
    pub fn new(
        factory_address: Option<Address>,
        sender_address: Address,
        paymaster_address: Option<Address>,
        entry_point_out: &ValidationOutput,
        sim_settings: Settings,
    ) -> Self {
        let factory = factory_address.map(|address| EntityInfo {
            address,
            is_staked: is_staked(entry_point_out.factory_info, sim_settings),
        });
        let sender = EntityInfo {
            address: sender_address,
            is_staked: is_staked(entry_point_out.sender_info, sim_settings),
        };
        let paymaster = paymaster_address.map(|address| EntityInfo {
            address,
            is_staked: is_staked(entry_point_out.paymaster_info, sim_settings),
        });
        Self {
            factory,
            sender,
            paymaster,
        }
    }

    pub fn get(self, entity: EntityType) -> Option<EntityInfo> {
        match entity {
            EntityType::Factory => self.factory,
            EntityType::Account => Some(self.sender),
            EntityType::Paymaster => self.paymaster,
            _ => None,
        }
    }

    pub fn sender_address(self) -> Address {
        self.sender.address
    }
}

fn is_staked(info: StakeInfo, sim_settings: Settings) -> bool {
    info.stake >= sim_settings.min_stake_value.into()
        && info.unstake_delay_sec >= sim_settings.min_unstake_delay.into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StorageRestriction {
    Allowed,
    NeedsStake,
    Banned,
}

#[derive(Clone, Copy, Debug)]
struct GetStorageRestrictionArgs<'a> {
    slots_by_address: &'a AssociatedSlotsByAddress,
    is_unstaked_wallet_creation: bool,
    entry_point_address: Address,
    entity_address: Address,
    sender_address: Address,
    accessed_address: Address,
    slot: U256,
}

fn get_storage_restriction(args: GetStorageRestrictionArgs<'_>) -> StorageRestriction {
    let GetStorageRestrictionArgs {
        slots_by_address,
        is_unstaked_wallet_creation,
        entry_point_address,
        entity_address,
        sender_address,
        accessed_address,
        slot,
        ..
    } = args;
    if accessed_address == sender_address {
        StorageRestriction::Allowed
    } else if slots_by_address.is_associated_slot(sender_address, slot) {
        // Allow entities to access the sender's associated storage unless its during an unstaked wallet creation
        // Can always access the entry point's associated storage (note only depositTo is allowed to be called)
        if accessed_address == entry_point_address || !is_unstaked_wallet_creation {
            StorageRestriction::Allowed
        } else {
            StorageRestriction::NeedsStake
        }
    } else if accessed_address == entity_address
        || slots_by_address.is_associated_slot(entity_address, slot)
    {
        StorageRestriction::NeedsStake
    } else {
        StorageRestriction::Banned
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Settings {
    pub min_unstake_delay: u32,
    pub min_stake_value: u128,
    pub max_simulate_handle_ops_gas: u64,
    pub max_verification_gas: u64,
}

impl Settings {
    pub fn new(
        min_unstake_delay: u32,
        min_stake_value: u128,
        max_simulate_handle_ops_gas: u64,
        max_verification_gas: u64,
    ) -> Self {
        Self {
            min_unstake_delay,
            min_stake_value,
            max_simulate_handle_ops_gas,
            max_verification_gas,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // one day in seconds: defined in the ERC-4337 spec
            min_unstake_delay: 84600,
            // 10^18 wei = 1 eth
            min_stake_value: 1_000_000_000_000_000_000,
            // 550 million gas: currently the defaults for Alchemy eth_call
            max_simulate_handle_ops_gas: 550_000_000,
            max_verification_gas: 5_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use ethers::{
        providers::{JsonRpcError, MockError, ProviderError},
        types::{
            transaction::{eip2718::TypedTransaction, eip2930::AccessList},
            Address, Eip1559TransactionRequest, GethTrace, NameOrAddress,
        },
    };
    use serde_json::Value;

    use super::*;
    use crate::common::types::{MockEntryPointLike, MockProviderLike};

    fn create_base_config() -> (MockProviderLike, MockEntryPointLike) {
        return (MockProviderLike::new(), MockEntryPointLike::new());
    }

    fn create_simulator(
        provider: MockProviderLike,
        entry_point: MockEntryPointLike,
    ) -> SimulatorImpl<MockProviderLike, MockEntryPointLike> {
        let settings = Settings::default();

        let mut mempool_configs = HashMap::new();
        mempool_configs.insert(H256::zero(), MempoolConfig::default());

        let provider = Arc::new(provider);
        let simulator: SimulatorImpl<MockProviderLike, MockEntryPointLike> = SimulatorImpl::new(
            Arc::clone(&provider),
            entry_point,
            settings,
            mempool_configs,
        );

        simulator
    }

    #[tokio::test]
    async fn test_simulate_validation() {
        let (mut provider, mut entry_point) = create_base_config();

        provider.expect_get_latest_block_hash().returning(|| {
            Ok(
                H256::from_str(
                    "0x38138f1cb4653ab6ab1c89ae3a6acc8705b54bd16a997d880c4421014ed66c3d",
                )
                .unwrap(),
            )
        });

        entry_point.expect_simulate_validation().returning(|_, _| {
            Ok(TypedTransaction::Eip1559(Eip1559TransactionRequest {
                from: None,
                to: Some(NameOrAddress::Address(
                    Address::from_str("0x5ff137d4b0fdcd49dca30c7cf57e578a026d2789").unwrap(),
                )),
                nonce: None,
                gas: None,
                value: None,
                data: None,
                chain_id: None,
                access_list: AccessList(vec![]),
                max_priority_fee_per_gas: None,
                max_fee_per_gas: None,
            }))
        });

        let simulation_tracer_result_json = r#"{
            "accessedContractAddresses": [
                "0x5ff137d4b0fdcd49dca30c7cf57e578a026d2789",
                "0xb856dbd4fa1a79a46d426f537455e7d3e79ab7c4",
                "0x8abb13360b87be5eeb1b98647a016add927a136c"
            ],
            "associatedSlotsByAddress": {
                "0x0000000000000000000000000000000000000000": [
                "0xd5c1ebdd81c5c7bebcd52bc11c8d37f7038b3c64f849c2ca58a022abeab1adae",
                "0xad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5"
                ],
                "0xb856dbd4fa1a79a46d426f537455e7d3e79ab7c4": [
                "0x3072884cc37d411af7360b34f105e1e860b1631783232a4f2d5c094d365cdaab",
                "0xf5357e1da3acf909ceaed3492183cbad85a3c9e1f0076495f66d3eed05219bd5",
                "0xf264fff4db20d04721712f34a6b5a8bca69a212345e40a92101082e79bdd1f0a"
                ]
            },
            "expectedStorage": {
                "0x5ff137d4b0fdcd49dca30c7cf57e578a026d2789": {
                "0xad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "0xad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb6": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "0xd5c1ebdd81c5c7bebcd52bc11c8d37f7038b3c64f849c2ca58a022abeab1adae": "0x0000000000000000000000000000000000000000000000000000000000000104",
                "0xf5357e1da3acf909ceaed3492183cbad85a3c9e1f0076495f66d3eed05219bd5": "0x000000000000000000000000000000000000000000000000000002de01482e02",
                "0xf5357e1da3acf909ceaed3492183cbad85a3c9e1f0076495f66d3eed05219bd6": "0x0000000000000000000000000000000000000000000000000000000000000000"
                },
                "0xb856dbd4fa1a79a46d426f537455e7d3e79ab7c4": {
                "0x0000000000000000000000000000000000000000000000000000000000000000": "0x00000000000000000000f7f00d283ce4cdbefd1a7c7c22d3e3b7189f2fd10001",
                "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc": "0x0000000000000000000000008abb13360b87be5eeb1b98647a016add927a136c"
                }
            },
            "factoryCalledCreate2Twice": false,
            "phases": [
                {
                "addressesCallingWithValue": [],
                "calledBannedEntryPointMethod": false,
                "calledNonEntryPointWithValue": false,
                "forbiddenOpcodesUsed": [],
                "forbiddenPrecompilesUsed": [],
                "ranOutOfGas": false,
                "storageAccesses": [],
                "undeployedContractAccesses": []
                },
                {
                "addressesCallingWithValue": ["0xb856dbd4fa1a79a46d426f537455e7d3e79ab7c4"],
                "calledBannedEntryPointMethod": false,
                "calledNonEntryPointWithValue": false,
                "forbiddenOpcodesUsed": [],
                "forbiddenPrecompilesUsed": [],
                "ranOutOfGas": false,
                "storageAccesses": [
                    {
                    "address": "0xb856dbd4fa1a79a46d426f537455e7d3e79ab7c4",
                    "slots": [
                        "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc",
                        "0x0000000000000000000000000000000000000000000000000000000000000000"
                    ]
                    },
                    {
                    "address": "0x5ff137d4b0fdcd49dca30c7cf57e578a026d2789",
                    "slots": ["0xf5357e1da3acf909ceaed3492183cbad85a3c9e1f0076495f66d3eed05219bd5"]
                    }
                ],
                "undeployedContractAccesses": []
                },
                {
                "addressesCallingWithValue": [],
                "calledBannedEntryPointMethod": false,
                "calledNonEntryPointWithValue": false,
                "forbiddenOpcodesUsed": [],
                "forbiddenPrecompilesUsed": [],
                "ranOutOfGas": false,
                "storageAccesses": [],
                "undeployedContractAccesses": []
                }
            ],
            "revertData": "0xe0cff05f00000000000000000000000000000000000000000000000000000000000000e00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014eff00000000000000000000000000000000000000000000000000000b7679c50c24000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ffffffffffff00000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000000000000000"
        }"#;

        let value: Value = serde_json::from_str(simulation_tracer_result_json).unwrap();

        provider
            .expect_debug_trace_call()
            .returning(move |_, _, _| Ok(GethTrace::Unknown(value.clone())));

        entry_point
            .expect_address()
            .returning(|| Address::from_str("0x5ff137d4b0fdcd49dca30c7cf57e578a026d2789").unwrap());

        // The underlying eth_call when getting the code hash in check_contracts
        provider.expect_call().returning(|_, _| {
            let json_rpc_error = JsonRpcError {
                code: -32000,
                message: "execution reverted".to_string(),
                data: Some(serde_json::Value::String(
                    "0x091cd005abf68e7b82c951a8619f065986132f67a0945153533cfcdd93b6895f33dbc0c7"
                        .to_string(),
                )),
            };
            Err(ProviderError::JsonRpcClientError(Box::new(
                MockError::JsonRpcError(json_rpc_error),
            )))
        });

        provider
            .expect_validate_user_op_signature()
            .returning(|_, _, _| Ok(AggregatorOut::NotNeeded));

        let user_operation = UserOperation {
            sender: Address::from_str("b856dbd4fa1a79a46d426f537455e7d3e79ab7c4").unwrap(),
            nonce: U256::from(264),
            init_code: Bytes::from_str("0x").unwrap(),
            call_data: Bytes::from_str("0xb61d27f6000000000000000000000000b856dbd4fa1a79a46d426f537455e7d3e79ab7c4000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000004d087d28800000000000000000000000000000000000000000000000000000000").unwrap(),
            call_gas_limit: U256::from(9100),
            verification_gas_limit: U256::from(64805),
            pre_verification_gas: U256::from(46128),
            max_fee_per_gas: U256::from(105000100),
            max_priority_fee_per_gas: U256::from(105000000),
            paymaster_and_data: Bytes::from_str("0x").unwrap(),
            signature: Bytes::from_str("0x98f89993ce573172635b44ef3b0741bd0c19dd06909d3539159f6d66bef8c0945550cc858b1cf5921dfce0986605097ba34c2cf3fc279154dd25e161ea7b3d0f1c").unwrap(),
        };

        let simulator = create_simulator(provider, entry_point);
        let res = simulator
            .simulate_validation(user_operation, None, None)
            .await;
        assert_eq!(res.is_ok(), true);
    }
}