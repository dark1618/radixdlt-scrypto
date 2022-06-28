use sbor::rust::collections::*;
use sbor::rust::string::String;
use sbor::rust::string::ToString;
use sbor::rust::vec::*;
use sbor::*;
use scrypto::buffer::scrypto_decode;
use scrypto::engine::types::*;
use scrypto::prelude::{
    ResourceManagerCreateBucketInput, ResourceManagerCreateInput, ResourceManagerCreateVaultInput,
    ResourceManagerGetNonFungibleInput, ResourceManagerGetResourceTypeInput,
    ResourceManagerGetTotalSupplyInput, ResourceManagerLockAuthInput, ResourceManagerMintInput,
    ResourceManagerNonFungibleExistsInput, ResourceManagerUpdateAuthInput,
    ResourceManagerUpdateMetadataInput, ResourceManagerUpdateNonFungibleDataInput,
};
use scrypto::resource::AccessRule::{self, *};
use scrypto::resource::Mutability::{self, *};
use scrypto::resource::ResourceManagerGetMetadataInput;
use scrypto::resource::ResourceMethodAuthKey::{self, *};
use scrypto::values::ScryptoValue;

use crate::engine::SystemApi;
use crate::model::resource_manager::ResourceMethodRule::{Protected, Public};
use crate::model::{Bucket, NonFungible, Vault};
use crate::model::ResourceManagerError::InvalidMethod;
use crate::model::{convert, MethodAuthorization, ResourceContainer};
use crate::wasm::*;

/// Converts soft authorization rule to a hard authorization rule.
/// Currently required as all auth is defined by soft authorization rules.
macro_rules! convert_auth {
    ($auth:expr) => {
        convert(&Type::Unit, &Value::Unit, &$auth)
    };
}

/// Represents an error when accessing a bucket.
#[derive(Debug, Clone, PartialEq)]
pub enum ResourceManagerError {
    InvalidDivisibility,
    InvalidAmount(Decimal, u8),
    InvalidResourceFlags(u64),
    InvalidMintPermission,
    ResourceTypeDoesNotMatch,
    MaxMintAmountExceeded,
    InvalidNonFungibleData,
    NonFungibleAlreadyExists(NonFungibleAddress),
    NonFungibleNotFound(NonFungibleAddress),
    InvalidRequestData(DecodeError),
    MethodNotFound(String),
    CouldNotCreateBucket,
    CouldNotCreateVault,
    InvalidMethod,
}

enum MethodAccessRuleMethod {
    Lock(),
    Update(AccessRule),
}

#[derive(Debug, Clone, TypeId, Encode, Decode)]
struct MethodAccessRule {
    auth: MethodAuthorization,
    update_auth: MethodAuthorization,
}

impl MethodAccessRule {
    pub fn new(entry: (AccessRule, Mutability)) -> Self {
        MethodAccessRule {
            auth: convert_auth!(entry.0),
            update_auth: match entry.1 {
                Mutability::LOCKED => MethodAuthorization::DenyAll,
                Mutability::MUTABLE(method_auth) => convert_auth!(method_auth),
            },
        }
    }

    pub fn get_method_auth(&self) -> &MethodAuthorization {
        &self.auth
    }

    pub fn get_update_auth(&self, method: MethodAccessRuleMethod) -> &MethodAuthorization {
        match method {
            MethodAccessRuleMethod::Lock() | MethodAccessRuleMethod::Update(_) => &self.update_auth,
        }
    }

    pub fn main(
        &mut self,
        method: MethodAccessRuleMethod,
    ) -> Result<ScryptoValue, ResourceManagerError> {
        match method {
            MethodAccessRuleMethod::Lock() => self.lock(),
            MethodAccessRuleMethod::Update(method_auth) => {
                self.update(method_auth);
            }
        }

        Ok(ScryptoValue::from_typed(&()))
    }

    fn update(&mut self, method_auth: AccessRule) {
        self.auth = convert_auth!(method_auth)
    }

    fn lock(&mut self) {
        self.update_auth = MethodAuthorization::DenyAll;
    }
}

#[derive(Debug, Clone, TypeId, Encode, Decode)]
enum ResourceMethodRule {
    Public,
    Protected(ResourceMethodAuthKey),
}

/// The definition of a resource.
#[derive(Debug, Clone, TypeId, Encode, Decode)]
pub struct ResourceManager {
    resource_type: ResourceType,
    metadata: HashMap<String, String>,
    method_table: HashMap<String, ResourceMethodRule>,
    vault_method_table: HashMap<String, ResourceMethodRule>,
    authorization: HashMap<ResourceMethodAuthKey, MethodAccessRule>,
    total_supply: Decimal,
}

impl ResourceManager {
    pub fn new(
        resource_type: ResourceType,
        metadata: HashMap<String, String>,
        mut auth: HashMap<ResourceMethodAuthKey, (AccessRule, Mutability)>,
    ) -> Result<Self, ResourceManagerError> {
        let mut vault_method_table: HashMap<String, ResourceMethodRule> = HashMap::new();
        vault_method_table.insert("take".to_string(), Protected(Withdraw));
        vault_method_table.insert("put".to_string(), Protected(Deposit));
        for pub_method in [
            "amount",
            "resource_address",
            "non_fungible_ids",
            "create_proof",
            "create_proof_by_amount",
            "create_proof_by_ids",
        ] {
            vault_method_table.insert(pub_method.to_string(), Public);
        }
        // Non Fungible methods
        vault_method_table.insert("take_non_fungibles".to_string(), Protected(Withdraw));

        let mut method_table: HashMap<String, ResourceMethodRule> = HashMap::new();
        method_table.insert("mint".to_string(), Protected(Mint));
        method_table.insert("burn".to_string(), Protected(Burn));
        method_table.insert("update_metadata".to_string(), Protected(UpdateMetadata));
        for pub_method in [
            "create_bucket",
            "metadata",
            "resource_type",
            "total_supply",
            "create_vault",
        ] {
            method_table.insert(pub_method.to_string(), Public);
        }

        // Non Fungible methods
        method_table.insert(
            "update_non_fungible_data".to_string(),
            Protected(UpdateNonFungibleData),
        );
        for pub_method in ["non_fungible_exists", "non_fungible_data"] {
            method_table.insert(pub_method.to_string(), Public);
        }

        let mut authorization: HashMap<ResourceMethodAuthKey, MethodAccessRule> = HashMap::new();
        for (auth_entry_key, default) in [
            (Mint, (DenyAll, LOCKED)),
            (Burn, (DenyAll, LOCKED)),
            (Withdraw, (AllowAll, LOCKED)),
            (Deposit, (AllowAll, LOCKED)),
            (UpdateMetadata, (DenyAll, LOCKED)),
            (UpdateNonFungibleData, (DenyAll, LOCKED)),
        ] {
            let entry = auth.remove(&auth_entry_key).unwrap_or(default);
            authorization.insert(auth_entry_key, MethodAccessRule::new(entry));
        }

        let resource_manager = Self {
            resource_type,
            metadata,
            method_table,
            vault_method_table,
            authorization,
            total_supply: 0.into(),
        };

        Ok(resource_manager)
    }

    pub fn get_vault_auth(&self, method_name: &str) -> &MethodAuthorization {
        match self.vault_method_table.get(method_name) {
            None => &MethodAuthorization::Unsupported,
            Some(Public) => &MethodAuthorization::AllowAll,
            Some(Protected(auth_key)) => {
                self.authorization.get(auth_key).unwrap().get_method_auth()
            }
        }
    }

    pub fn get_consuming_bucket_auth(&self, method_name: &str) -> &MethodAuthorization {
        match self.method_table.get(method_name) {
            None => &MethodAuthorization::Unsupported,
            Some(Public) => &MethodAuthorization::AllowAll,
            Some(Protected(method)) => self.authorization.get(method).unwrap().get_method_auth(),
        }
    }

    pub fn get_auth(&self, method_name: &str, arg: &ScryptoValue) -> &MethodAuthorization {
        match method_name {
            "update_auth" => {
                let input: ResourceManagerUpdateAuthInput = scrypto_decode(&arg.raw).unwrap();
                match self.authorization.get(&input.method) {
                    None => &MethodAuthorization::Unsupported,
                    Some(entry) => {
                        entry.get_update_auth(MethodAccessRuleMethod::Update(input.access_rule))
                    }
                }
            }
            "lock_auth" => {
                let input: ResourceManagerLockAuthInput = scrypto_decode(&arg.raw).unwrap();
                match self.authorization.get(&input.method) {
                    None => &MethodAuthorization::Unsupported,
                    Some(entry) => entry.get_update_auth(MethodAccessRuleMethod::Lock()),
                }
            }
            _ => match self.method_table.get(method_name) {
                None => &MethodAuthorization::Unsupported,
                Some(Public) => &MethodAuthorization::AllowAll,
                Some(Protected(method)) => {
                    self.authorization.get(method).unwrap().get_method_auth()
                }
            },
        }
    }

    pub fn resource_type(&self) -> ResourceType {
        self.resource_type
    }

    pub fn metadata(&self) -> &HashMap<String, String> {
        &self.metadata
    }

    pub fn total_supply(&self) -> Decimal {
        self.total_supply
    }

    fn mint<'borrowed, S: SystemApi<'borrowed, W, I>, W: WasmEngine<I>, I: WasmInstance>(
        &mut self,
        mint_params: MintParams,
        self_address: ResourceAddress,
        system_api: &mut S,
    ) -> Result<ResourceContainer, ResourceManagerError> {
        match mint_params {
            MintParams::Fungible { amount } => self.mint_fungible(amount, self_address),
            MintParams::NonFungible { entries } => {
                self.mint_non_fungibles(entries, self_address, system_api)
            }
        }
    }

    pub fn mint_fungible(
        &mut self,
        amount: Decimal,
        self_address: ResourceAddress,
    ) -> Result<ResourceContainer, ResourceManagerError> {
        if let ResourceType::Fungible { divisibility } = self.resource_type {
            // check amount
            self.check_amount(amount)?;

            // It takes `1,701,411,835` mint operations to reach `Decimal::MAX`,
            // which will be impossible with metering.
            if amount > 100_000_000_000i128.into() {
                return Err(ResourceManagerError::MaxMintAmountExceeded);
            }

            self.total_supply += amount;

            Ok(ResourceContainer::new_fungible(
                self_address,
                divisibility,
                amount,
            ))
        } else {
            Err(ResourceManagerError::ResourceTypeDoesNotMatch)
        }
    }

    fn process_non_fungible_data(data: &[u8]) -> Result<ScryptoValue, ResourceManagerError> {
        let validated = ScryptoValue::from_slice(data)
            .map_err(|_| ResourceManagerError::InvalidNonFungibleData)?;
        if !validated.bucket_ids.is_empty() {
            return Err(ResourceManagerError::InvalidNonFungibleData);
        }
        if !validated.proof_ids.is_empty() {
            return Err(ResourceManagerError::InvalidNonFungibleData);
        }
        if !validated.kv_store_ids.is_empty() {
            return Err(ResourceManagerError::InvalidNonFungibleData);
        }
        if !validated.vault_ids.is_empty() {
            return Err(ResourceManagerError::InvalidNonFungibleData);
        }
        Ok(validated)
    }

    fn mint_non_fungibles<
        'borrowed,
        S: SystemApi<'borrowed, W, I>,
        W: WasmEngine<I>,
        I: WasmInstance,
    >(
        &mut self,
        entries: HashMap<NonFungibleId, (Vec<u8>, Vec<u8>)>,
        self_address: ResourceAddress,
        system_api: &mut S,
    ) -> Result<ResourceContainer, ResourceManagerError> {
        // check resource type
        if !matches!(self.resource_type, ResourceType::NonFungible) {
            return Err(ResourceManagerError::ResourceTypeDoesNotMatch);
        }

        // check amount
        let amount: Decimal = entries.len().into();
        self.check_amount(amount)?;

        // It takes `1,701,411,835` mint operations to reach `Decimal::MAX`,
        // which will be impossible with metering.
        if amount > 100_000_000_000i128.into() {
            return Err(ResourceManagerError::MaxMintAmountExceeded);
        }

        self.total_supply += amount;

        // Allocate non-fungibles
        let mut ids = BTreeSet::new();
        for (id, data) in entries {
            let non_fungible_address = NonFungibleAddress::new(self_address, id.clone());
            if system_api.get_non_fungible(&non_fungible_address).is_some() {
                return Err(ResourceManagerError::NonFungibleAlreadyExists(
                    non_fungible_address,
                ));
            }

            let immutable_data = Self::process_non_fungible_data(&data.0)?;
            let mutable_data = Self::process_non_fungible_data(&data.1)?;
            let non_fungible = NonFungible::new(immutable_data.raw, mutable_data.raw);

            system_api.set_non_fungible(non_fungible_address, Some(non_fungible));
            ids.insert(id);
        }

        Ok(ResourceContainer::new_non_fungible(self_address, ids))
    }

    pub fn burn(&mut self, amount: Decimal) {
        self.total_supply -= amount;
    }

    fn update_metadata(
        &mut self,
        new_metadata: HashMap<String, String>,
    ) -> Result<(), ResourceManagerError> {
        self.metadata = new_metadata;

        Ok(())
    }

    fn check_amount(&self, amount: Decimal) -> Result<(), ResourceManagerError> {
        let divisibility = self.resource_type.divisibility();

        if amount.is_negative() || amount.0 % 10i128.pow((18 - divisibility).into()) != 0i128 {
            Err(ResourceManagerError::InvalidAmount(amount, divisibility))
        } else {
            Ok(())
        }
    }

    pub fn static_main<
        'borrowed,
        S: SystemApi<'borrowed, W, I>,
        W: WasmEngine<I>,
        I: WasmInstance,
    >(
        method_name: &str,
        arg: ScryptoValue,
        system_api: &mut S,
    ) -> Result<ScryptoValue, ResourceManagerError> {
        match method_name {
            "create" => {
                let input: ResourceManagerCreateInput = scrypto_decode(&arg.raw)
                    .map_err(|e| ResourceManagerError::InvalidRequestData(e))?;

                let resource_manager =
                    ResourceManager::new(input.resource_type, input.metadata, input.access_rules)?;

                let resource_id = system_api.native_create(resource_manager);
                let resource_address = match resource_id {
                    ValueId::Resource(resource_address) => resource_address,
                    _ => panic!("Expected to be a resource address")
                };

                let bucket_id = if let Some(mint_params) = input.mint_params {
                    let mut value = system_api.borrow_native_value(&resource_id);
                    let resource_manager = value.resource_manager();
                    let container =
                        resource_manager.mint(mint_params, resource_address, system_api)?;
                    system_api.return_native_value(resource_id, value);
                    let bucket_id = system_api.native_create(Bucket::new(container)).into();
                    Some(scrypto::resource::Bucket(bucket_id))
                } else {
                    None
                };

                system_api.native_globalize(&resource_id);

                Ok(ScryptoValue::from_typed(&(resource_address, bucket_id)))
            }
            _ => Err(InvalidMethod),
        }
    }

    pub fn main<'borrowed, S: SystemApi<'borrowed, W, I>, W: WasmEngine<I>, I: WasmInstance>(
        resource_address: ResourceAddress,
        method_name: &str,
        arg: ScryptoValue,
        system_api: &mut S,
    ) -> Result<ScryptoValue, ResourceManagerError> {
        let value_id = ValueId::Resource(resource_address);
        let mut ref_mut = system_api.borrow_native_value(&value_id);
        let resource_manager = ref_mut.resource_manager();

        let rtn = match method_name {
            "update_auth" => {
                let input: ResourceManagerUpdateAuthInput = scrypto_decode(&arg.raw)
                    .map_err(|e| ResourceManagerError::InvalidRequestData(e))?;
                let method_entry = resource_manager
                    .authorization
                    .get_mut(&input.method)
                    .unwrap();
                method_entry.main(MethodAccessRuleMethod::Update(input.access_rule))
            }
            "lock_auth" => {
                let input: ResourceManagerLockAuthInput = scrypto_decode(&arg.raw)
                    .map_err(|e| ResourceManagerError::InvalidRequestData(e))?;
                let method_entry = resource_manager
                    .authorization
                    .get_mut(&input.method)
                    .unwrap();
                method_entry.main(MethodAccessRuleMethod::Lock())
            }
            "create_vault" => {
                let _: ResourceManagerCreateVaultInput = scrypto_decode(&arg.raw)
                    .map_err(|e| ResourceManagerError::InvalidRequestData(e))?;
                let container = ResourceContainer::new_empty(
                    resource_address,
                    resource_manager.resource_type(),
                );
                let vault_id = system_api.native_create(Vault::new(container)).into();
                Ok(ScryptoValue::from_typed(&scrypto::resource::Vault(
                    vault_id,
                )))
            }
            "create_bucket" => {
                let _: ResourceManagerCreateBucketInput = scrypto_decode(&arg.raw)
                    .map_err(|e| ResourceManagerError::InvalidRequestData(e))?;
                let container = ResourceContainer::new_empty(
                    resource_address,
                    resource_manager.resource_type(),
                );
                let bucket_id = system_api.native_create(Bucket::new(container)).into();
                Ok(ScryptoValue::from_typed(&scrypto::resource::Bucket(
                    bucket_id,
                )))
            }
            "mint" => {
                let input: ResourceManagerMintInput = scrypto_decode(&arg.raw)
                    .map_err(|e| ResourceManagerError::InvalidRequestData(e))?;
                let container =
                    resource_manager.mint(input.mint_params, resource_address, system_api)?;
                let bucket_id = system_api.native_create(Bucket::new(container)).into();
                Ok(ScryptoValue::from_typed(&scrypto::resource::Bucket(
                    bucket_id,
                )))
            }
            "metadata" => {
                let _: ResourceManagerGetMetadataInput = scrypto_decode(&arg.raw)
                    .map_err(|e| ResourceManagerError::InvalidRequestData(e))?;
                Ok(ScryptoValue::from_typed(&resource_manager.metadata))
            }
            "resource_type" => {
                let _: ResourceManagerGetResourceTypeInput = scrypto_decode(&arg.raw)
                    .map_err(|e| ResourceManagerError::InvalidRequestData(e))?;
                Ok(ScryptoValue::from_typed(&resource_manager.resource_type))
            }
            "total_supply" => {
                let _: ResourceManagerGetTotalSupplyInput = scrypto_decode(&arg.raw)
                    .map_err(|e| ResourceManagerError::InvalidRequestData(e))?;
                Ok(ScryptoValue::from_typed(&resource_manager.total_supply))
            }
            "update_metadata" => {
                let input: ResourceManagerUpdateMetadataInput = scrypto_decode(&arg.raw)
                    .map_err(|e| ResourceManagerError::InvalidRequestData(e))?;
                resource_manager.update_metadata(input.metadata)?;
                Ok(ScryptoValue::from_typed(&()))
            }
            "update_non_fungible_data" => {
                let input: ResourceManagerUpdateNonFungibleDataInput = scrypto_decode(&arg.raw)
                    .map_err(|e| ResourceManagerError::InvalidRequestData(e))?;
                let non_fungible_address =
                    NonFungibleAddress::new(resource_address.clone(), input.id);
                let data = Self::process_non_fungible_data(&input.data)?;
                let mut non_fungible = system_api.get_non_fungible(&non_fungible_address).ok_or(
                    ResourceManagerError::NonFungibleNotFound(non_fungible_address.clone()),
                )?;
                non_fungible.set_mutable_data(data.raw);
                system_api.set_non_fungible(non_fungible_address, Some(non_fungible));

                Ok(ScryptoValue::from_typed(&()))
            }
            "non_fungible_exists" => {
                let input: ResourceManagerNonFungibleExistsInput = scrypto_decode(&arg.raw)
                    .map_err(|e| ResourceManagerError::InvalidRequestData(e))?;
                let non_fungible_address =
                    NonFungibleAddress::new(resource_address.clone(), input.id);
                let non_fungible = system_api.get_non_fungible(&non_fungible_address);
                Ok(ScryptoValue::from_typed(&non_fungible.is_some()))
            }
            "non_fungible_data" => {
                let input: ResourceManagerGetNonFungibleInput = scrypto_decode(&arg.raw)
                    .map_err(|e| ResourceManagerError::InvalidRequestData(e))?;
                let non_fungible_address =
                    NonFungibleAddress::new(resource_address.clone(), input.id);
                let non_fungible = system_api.get_non_fungible(&non_fungible_address).ok_or(
                    ResourceManagerError::NonFungibleNotFound(non_fungible_address),
                )?;
                Ok(ScryptoValue::from_typed(&[
                    non_fungible.immutable_data(),
                    non_fungible.mutable_data(),
                ]))
            }
            _ => Err(InvalidMethod),
        }?;

        system_api.return_native_value(value_id, ref_mut);

        Ok(rtn)
    }
}
