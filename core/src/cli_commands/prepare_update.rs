#![allow(unused_imports)]

use std::{
    cmp::max,
    convert::{TryFrom, TryInto},
    io::Write,
};

use derive_more::From;
use ergo_lib::{
    chain::{
        ergo_box::box_builder::{ErgoBoxCandidateBuilder, ErgoBoxCandidateBuilderError},
        transaction::Transaction,
    },
    ergo_chain_types::blake2b256_hash,
    ergotree_ir::{
        chain::{
            address::{Address, AddressEncoder, AddressEncoderError},
            ergo_box::{
                box_value::{BoxValue, BoxValueError},
                ErgoBox,
            },
            token::{Token, TokenAmount},
        },
        ergo_tree::ErgoTree,
        serialization::SigmaParsingError,
    },
    wallet::{
        box_selector::{BoxSelector, BoxSelectorError, SimpleBoxSelector},
        tx_builder::{TxBuilder, TxBuilderError},
    },
};
use ergo_node_interface::node_interface::NodeError;
use log::{debug, info};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    box_kind::{
        make_refresh_box_candidate, BallotBoxWrapperInputs, PoolBox, PoolBoxWrapperInputs,
        RefreshBoxWrapperInputs, UpdateBoxWrapperInputs,
    },
    contracts::{
        ballot::BallotContractError,
        pool::{PoolContractError, PoolContractParameters},
        refresh::{
            RefreshContract, RefreshContractError, RefreshContractInputs, RefreshContractParameters,
        },
        update::{
            self, UpdateContract, UpdateContractError, UpdateContractInputs,
            UpdateContractParameters,
        },
    },
    node_interface::{new_node_interface, SignTransaction, SubmitTransaction},
    oracle_config::{OracleConfig, BASE_FEE, ORACLE_CONFIG},
    oracle_state::{OraclePool, StageDataSource},
    serde::{OracleConfigSerde, SerdeConversionError, UpdateBootstrapConfigSerde},
    spec_token::{
        BallotTokenId, OracleTokenId, RefreshTokenId, RewardTokenId, TokenIdKind, UpdateTokenId,
    },
    wallet::{WalletDataError, WalletDataSource},
};

use super::bootstrap::{NftMintDetails, TokenMintDetails};

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct UpdateTokensToMint {
    pub refresh_nft: Option<NftMintDetails>,
    pub update_nft: Option<NftMintDetails>,
    pub oracle_tokens: Option<TokenMintDetails>,
    pub ballot_tokens: Option<TokenMintDetails>,
    pub reward_tokens: Option<TokenMintDetails>,
}

#[derive(Clone)]
pub struct UpdateBootstrapConfig {
    pub pool_contract_parameters: Option<PoolContractParameters>, // New pool script, etc. Note that we don't actually mint any new pool NFT in the update step, instead this is simply passed to the new oracle config for convenience
    pub refresh_contract_parameters: Option<RefreshContractParameters>,
    pub update_contract_parameters: Option<UpdateContractParameters>,
    pub tokens_to_mint: UpdateTokensToMint,
}

pub fn prepare_update(config_file_name: String) -> Result<(), PrepareUpdateError> {
    let s = std::fs::read_to_string(config_file_name)?;
    let config_serde: UpdateBootstrapConfigSerde = serde_yaml::from_str(&s)?;

    let node_interface = new_node_interface();
    let change_address = AddressEncoder::unchecked_parse_address_from_str(
        &node_interface
            .wallet_status()?
            .change_address
            .ok_or(PrepareUpdateError::NoChangeAddressSetInNode)?,
    )?;
    let config = UpdateBootstrapConfig::try_from(config_serde)?;
    let update_bootstrap_input = PrepareUpdateInput {
        wallet: &node_interface,
        tx_signer: &node_interface,
        submit_tx: &node_interface,
        tx_fee: *BASE_FEE,
        erg_value_per_box: *BASE_FEE,
        change_address,
        height: node_interface
            .current_block_height()
            .unwrap()
            .try_into()
            .unwrap(),
    };

    let prepare = PrepareUpdate::new(update_bootstrap_input, &ORACLE_CONFIG)?;
    let new_config = prepare.execute(config)?;
    // let new_config = perform_update_chained_transaction(update_bootstrap_input)?;
    let blake2b_pool_ergo_tree: String = blake2b256_hash(
        new_config
            .pool_box_wrapper_inputs
            .contract_inputs
            .contract_parameters()
            .ergo_tree_bytes()
            .as_slice(),
    )
    .into();

    info!("Update chain-transaction complete");
    info!("Writing new config file to oracle_config_updated.yaml");
    let config = OracleConfigSerde::from(new_config);
    let s = serde_yaml::to_string(&config)?;
    let mut file = std::fs::File::create("oracle_config_updated.yaml")?;
    file.write_all(s.as_bytes())?;
    info!("Updated oracle configuration file oracle_config_updated.yaml");
    info!(
        "Base16-encoded blake2b hash of the serialized new pool box contract(ErgoTree): {}",
        blake2b_pool_ergo_tree
    );
    print_hints_for_voting()?;
    Ok(())
}

fn print_hints_for_voting() -> Result<(), PrepareUpdateError> {
    let epoch_length = ORACLE_CONFIG
        .refresh_box_wrapper_inputs
        .contract_inputs
        .contract_parameters()
        .epoch_length() as u32;
    let current_height: u32 = new_node_interface().current_block_height()? as u32;
    let op = OraclePool::new().unwrap();
    let oracle_boxes = op.datapoint_stage.stage.get_boxes().unwrap();
    let min_oracle_box_height = current_height - epoch_length;
    let active_oracle_count = oracle_boxes
        .into_iter()
        .filter(|b| b.creation_height >= min_oracle_box_height)
        .count() as u32;
    let pool_box = op.get_pool_box_source().get_pool_box().unwrap();
    let pool_box_height = pool_box.get_box().creation_height;
    let next_epoch_height = max(pool_box_height + epoch_length, current_height);
    let reward_tokens_left = *pool_box.reward_token().amount.as_u64();
    let update_box = op.get_update_box_source().get_update_box().unwrap();
    let update_box_height = update_box.get_box().creation_height;
    info!("Update box height: {}", update_box_height);
    info!(
        "Reward token id in the pool box: {}",
        String::from(pool_box.reward_token().token_id.token_id())
    );
    info!(
        "Current height is {}, pool box height (epoch start) {}, epoch length is {}",
        current_height, pool_box_height, epoch_length
    );
    info!(
        "Estimated active oracle count is {}, reward tokens in the pool box {}",
        active_oracle_count, reward_tokens_left
    );
    for i in 0..10 {
        info!(
            "On new epoch height {} estimating reward tokens in the pool box: {}",
            next_epoch_height + i * (epoch_length + 1),
            reward_tokens_left - ((i + 1) * (active_oracle_count * 2)) as u64
        );
    }
    Ok(())
}

struct PrepareUpdateInput<'a> {
    pub wallet: &'a dyn WalletDataSource,
    pub tx_signer: &'a dyn SignTransaction,
    pub submit_tx: &'a dyn SubmitTransaction,
    pub tx_fee: BoxValue,
    pub erg_value_per_box: BoxValue,
    pub change_address: Address,
    pub height: u32,
}

struct PrepareUpdate<'a> {
    input: PrepareUpdateInput<'a>,
    config: &'a OracleConfig,
    wallet_pk_ergo_tree: ErgoTree,
    num_transactions_left: u32,
    inputs_for_next_tx: Vec<ErgoBox>,
    built_txs: Vec<Transaction>,
}

impl<'a> PrepareUpdate<'a> {
    fn new(
        input: PrepareUpdateInput<'a>,
        config: &'a OracleConfig,
    ) -> Result<Self, PrepareUpdateError> {
        let wallet_pk_ergo_tree = config.oracle_address.address().script()?;
        Ok(Self {
            input,
            wallet_pk_ergo_tree,
            config,
            num_transactions_left: 0,
            inputs_for_next_tx: vec![],
            built_txs: vec![],
        })
    }

    fn calc_target_balance(&self, num_transactions: u32) -> Result<BoxValue, BoxValueError> {
        let b = self
            .input
            .erg_value_per_box
            .checked_mul_u32(num_transactions)?;
        let fees = self.input.tx_fee.checked_mul_u32(num_transactions)?;
        b.checked_add(&fees)
    }

    fn mint_token(
        &mut self,
        token_name: String,
        token_desc: String,
        token_amount: TokenAmount,
        different_token_box_guard: Option<ErgoTree>,
    ) -> Result<Token, PrepareUpdateError> {
        let target_balance = self.calc_target_balance(self.num_transactions_left)?;
        let box_selector = SimpleBoxSelector::new();
        let box_selection =
            box_selector.select(self.inputs_for_next_tx.clone(), target_balance, &[])?;
        let token = Token {
            token_id: box_selection.boxes.first().box_id().into(),
            amount: token_amount,
        };
        let token_box_guard =
            different_token_box_guard.unwrap_or_else(|| self.wallet_pk_ergo_tree.clone());
        let mut builder = ErgoBoxCandidateBuilder::new(
            self.input.erg_value_per_box,
            token_box_guard,
            self.input.height,
        );
        builder.mint_token(token.clone(), token_name, token_desc, 1);
        let mut output_candidates = vec![builder.build()?];

        let remaining_funds = ErgoBoxCandidateBuilder::new(
            self.calc_target_balance(self.num_transactions_left - 1)?,
            self.wallet_pk_ergo_tree.clone(),
            self.input.height,
        )
        .build()?;
        output_candidates.push(remaining_funds.clone());

        let inputs = box_selection.boxes.clone();
        let tx_builder = TxBuilder::new(
            box_selection,
            output_candidates,
            self.input.height,
            self.input.tx_fee,
            self.input.change_address.clone(),
        );
        let mint_token_tx = tx_builder.build()?;
        debug!("Mint token unsigned transaction: {:?}", mint_token_tx);
        let signed_tx =
            self.input
                .tx_signer
                .sign_transaction_with_inputs(&mint_token_tx, inputs, None)?;
        self.num_transactions_left -= 1;
        self.built_txs.push(signed_tx.clone());
        self.inputs_for_next_tx = self.filter_tx_outputs(signed_tx.outputs.clone());
        info!("minting tx id: {:?}", signed_tx.id());
        Ok(token)
    }

    fn build_refresh_box(
        &mut self,
        contract: &RefreshContract,
        refresh_nft_token: Token,
    ) -> Result<Transaction, PrepareUpdateError> {
        let refresh_box_candidate = make_refresh_box_candidate(
            contract,
            refresh_nft_token.clone(),
            self.input.erg_value_per_box,
            self.input.height,
        )?;
        let target_balance = self.calc_target_balance(self.num_transactions_left)?;
        let box_selection = SimpleBoxSelector::new().select(
            self.inputs_for_next_tx.clone(),
            target_balance,
            &[refresh_nft_token.clone()],
        )?;
        let mut output_candidates = vec![refresh_box_candidate];
        let remaining_funds = ErgoBoxCandidateBuilder::new(
            self.calc_target_balance(self.num_transactions_left - 1)?,
            self.wallet_pk_ergo_tree.clone(),
            self.input.height,
        )
        .build()?;
        output_candidates.push(remaining_funds.clone());
        let tx_builder = TxBuilder::new(
            box_selection.clone(),
            output_candidates,
            self.input.height,
            self.input.tx_fee,
            self.input.change_address.clone(),
        );
        let refresh_box_tx = tx_builder.build()?;
        let signed_refresh_box_tx = self.input.tx_signer.sign_transaction_with_inputs(
            &refresh_box_tx,
            box_selection.boxes.clone(),
            None,
        )?;
        self.num_transactions_left -= 1;
        self.built_txs.push(signed_refresh_box_tx.clone());
        self.inputs_for_next_tx = self.filter_tx_outputs(signed_refresh_box_tx.outputs.clone());
        Ok(signed_refresh_box_tx)
    }

    /// Since we're building a chain of transactions, we need to filter the output boxes of each
    /// constituent transaction to be only those that are guarded by our wallet's key.
    fn filter_tx_outputs(&self, outputs: Vec<ErgoBox>) -> Vec<ErgoBox> {
        outputs
            .into_iter()
            .filter(|b| b.ergo_tree == self.wallet_pk_ergo_tree)
            .collect()
    }

    fn execute(
        mut self,
        config: UpdateBootstrapConfig,
    ) -> Result<OracleConfig, PrepareUpdateError> {
        self.num_transactions_left = 7; // 5 for the tokens, 1 for the refresh box, 1 for the change

        let mut need_pool_contract_update = false;
        let mut need_ballot_contract_update = false;

        let unspent_boxes = self.input.wallet.get_unspent_wallet_boxes()?;
        debug!("unspent boxes: {:?}", unspent_boxes);
        let target_balance = self.calc_target_balance(self.num_transactions_left)?;
        debug!("target_balance: {:?}", target_balance);
        let box_selector = SimpleBoxSelector::new();
        let box_selection = box_selector.select(unspent_boxes.clone(), target_balance, &[])?;
        debug!("box selection: {:?}", box_selection);

        let mut new_oracle_config = self.config.clone();
        // Inputs for each transaction in chained tx, updated after each mint step
        self.inputs_for_next_tx = box_selection.boxes.as_vec().clone();

        if let Some(ref token_mint_details) = config.tokens_to_mint.oracle_tokens {
            info!("Minting oracle tokens");
            let token = self.mint_token(
                token_mint_details.name.clone(),
                token_mint_details.description.clone(),
                token_mint_details.quantity.try_into().unwrap(),
                None,
            )?;
            new_oracle_config.token_ids.oracle_token_id =
                OracleTokenId::from_token_id_unchecked(token.token_id);
        }
        if let Some(ref token_mint_details) = config.tokens_to_mint.ballot_tokens {
            info!("Minting ballot tokens");
            let token = self.mint_token(
                token_mint_details.name.clone(),
                token_mint_details.description.clone(),
                token_mint_details.quantity.try_into().unwrap(),
                None,
            )?;
            new_oracle_config.token_ids.ballot_token_id =
                BallotTokenId::from_token_id_unchecked(token.token_id);
        }
        if let Some(ref token_mint_details) = config.tokens_to_mint.reward_tokens {
            info!("Minting reward tokens");
            let token = self.mint_token(
                token_mint_details.name.clone(),
                token_mint_details.description.clone(),
                token_mint_details.quantity.try_into().unwrap(),
                None,
            )?;
            new_oracle_config.token_ids.reward_token_id =
                RewardTokenId::from_token_id_unchecked(token.token_id);
        }
        if config.refresh_contract_parameters.is_some()
            || config.tokens_to_mint.oracle_tokens.is_some()
        {
            let contract_parameters = config.refresh_contract_parameters.unwrap_or_else(|| {
                new_oracle_config
                    .refresh_box_wrapper_inputs
                    .contract_inputs
                    .contract_parameters()
                    .clone()
            });
            info!("Creating new refresh NFT and refresh box");
            let refresh_nft_details = config
                .tokens_to_mint
                .refresh_nft
                .ok_or(PrepareUpdateError::NoMintDetails)?;
            let token = self.mint_token(
                refresh_nft_details.name.clone(),
                refresh_nft_details.description.clone(),
                1.try_into().unwrap(),
                None,
            )?;
            new_oracle_config.token_ids.refresh_nft_token_id =
                RefreshTokenId::from_token_id_unchecked(token.token_id.clone());

            // Create refresh box --------------------------------------------------------------------------
            info!("Create and sign refresh box tx");
            let refresh_contract_inputs = RefreshContractInputs::build_with(
                contract_parameters.clone(),
                new_oracle_config.token_ids.oracle_token_id.clone(),
                self.config.token_ids.pool_nft_token_id.clone(),
            )?;
            let refresh_contract = RefreshContract::checked_load(&refresh_contract_inputs)?;
            new_oracle_config.refresh_box_wrapper_inputs = RefreshBoxWrapperInputs {
                contract_inputs: refresh_contract_inputs,
                refresh_nft_token_id: new_oracle_config.token_ids.refresh_nft_token_id.clone(),
            };
            let signed_refresh_box_tx = self.build_refresh_box(&refresh_contract, token)?;
            info!("Refresh box tx id: {:?}", signed_refresh_box_tx.id());
            // pool contract needs to be updated with new refresh NFT
            need_pool_contract_update = true;
        }

        if config.update_contract_parameters.is_some()
            || config.tokens_to_mint.ballot_tokens.is_some()
        {
            let update_contract_parameters =
                config.update_contract_parameters.unwrap_or_else(|| {
                    new_oracle_config
                        .update_box_wrapper_inputs
                        .contract_inputs
                        .contract_parameters()
                        .clone()
                });
            info!("Creating new update NFT and update box");
            let update_contract_inputs = UpdateContractInputs::build_with(
                update_contract_parameters.clone(),
                new_oracle_config.token_ids.pool_nft_token_id.clone(),
                new_oracle_config.token_ids.ballot_token_id.clone(),
            )?;
            let update_contract = UpdateContract::checked_load(&update_contract_inputs)?;
            let update_nft_details = config
                .tokens_to_mint
                .update_nft
                .ok_or(PrepareUpdateError::NoMintDetails)?;
            let token = self.mint_token(
                update_nft_details.name.clone(),
                update_nft_details.description.clone(),
                1.try_into().unwrap(),
                Some(update_contract.ergo_tree()),
            )?;
            new_oracle_config.token_ids.update_nft_token_id =
                UpdateTokenId::from_token_id_unchecked(token.token_id.clone());
            new_oracle_config.update_box_wrapper_inputs = UpdateBoxWrapperInputs {
                contract_inputs: update_contract_inputs,
                update_nft_token_id: new_oracle_config.token_ids.update_nft_token_id.clone(),
            };
            // update ballot and pool contract with new update NFT
            need_ballot_contract_update = true;
            need_pool_contract_update = true;
        }

        if need_ballot_contract_update {
            new_oracle_config.ballot_box_wrapper_inputs = BallotBoxWrapperInputs::build_with(
                new_oracle_config
                    .ballot_box_wrapper_inputs
                    .contract_inputs
                    .contract_parameters()
                    .clone(),
                new_oracle_config.token_ids.ballot_token_id.clone(),
                new_oracle_config.token_ids.update_nft_token_id.clone(),
            )?;
        }

        if config.pool_contract_parameters.is_some() || need_pool_contract_update {
            let new_pool_contract_parameters =
                config.pool_contract_parameters.unwrap_or_else(|| {
                    new_oracle_config
                        .pool_box_wrapper_inputs
                        .contract_inputs
                        .contract_parameters()
                        .clone()
                });
            let new_pool_box_wrapper_inputs = PoolBoxWrapperInputs::build_with(
                new_pool_contract_parameters,
                new_oracle_config.token_ids.refresh_nft_token_id.clone(),
                new_oracle_config.token_ids.update_nft_token_id.clone(),
                new_oracle_config.token_ids.pool_nft_token_id.clone(),
                new_oracle_config.token_ids.reward_token_id.clone(),
            )?;
            new_oracle_config.pool_box_wrapper_inputs = new_pool_box_wrapper_inputs;
        }

        for tx in self.built_txs {
            let tx_id = self.input.submit_tx.submit_transaction(&tx)?;
            info!("Tx submitted {}", tx_id);
        }
        Ok(new_oracle_config)
    }
}

#[derive(Debug, Error, From)]
pub enum PrepareUpdateError {
    #[error("tx builder error: {0}")]
    TxBuilder(TxBuilderError),
    #[error("box builder error: {0}")]
    ErgoBoxCandidateBuilder(ErgoBoxCandidateBuilderError),
    #[error("node error: {0}")]
    Node(NodeError),
    #[error("box selector error: {0}")]
    BoxSelector(BoxSelectorError),
    #[error("box value error: {0}")]
    BoxValue(BoxValueError),
    #[error("IO error: {0}")]
    Io(std::io::Error),
    #[error("serde-yaml error: {0}")]
    SerdeYaml(serde_yaml::Error),
    #[error("yaml-rust error: {0}")]
    YamlRust(String),
    #[error("AddressEncoder error: {0}")]
    AddressEncoder(AddressEncoderError),
    #[error("SigmaParsing error: {0}")]
    SigmaParse(SigmaParsingError),
    #[error("Node doesn't have a change address set")]
    NoChangeAddressSetInNode,
    #[error("Refresh contract failed: {0}")]
    RefreshContract(RefreshContractError),
    #[error("Update contract error: {0}")]
    UpdateContract(UpdateContractError),
    #[error("Pool contract failed: {0}")]
    PoolContract(PoolContractError),
    #[error("Bootstrap config file already exists")]
    ConfigFilenameAlreadyExists,
    #[error("No parameters were added for update")]
    NoOpUpgrade,
    #[error("No mint details were provided for update/refresh contract in tokens_to_mint")]
    NoMintDetails,
    #[error("Serde conversion error {0}")]
    SerdeConversion(SerdeConversionError),
    #[error("WalletData error: {0}")]
    WalletData(WalletDataError),
    #[error("Ballot contract error: {0}")]
    BallotContract(BallotContractError),
}

#[cfg(test)]
mod test {
    use ergo_lib::{
        chain::{ergo_state_context::ErgoStateContext, transaction::TxId},
        ergotree_interpreter::sigma_protocol::private_input::DlogProverInput,
        ergotree_ir::chain::{
            address::{AddressEncoder, NetworkAddress, NetworkPrefix},
            ergo_box::{ErgoBox, NonMandatoryRegisters},
        },
        wallet::Wallet,
    };
    use sigma_test_util::force_any_val;

    use super::*;
    use crate::cli_commands::bootstrap::tests::SubmitTxMock;
    use crate::pool_commands::test_utils::{LocalTxSigner, WalletDataMock};

    #[test]
    fn test_prepare_update_transaction() {
        let old_config: OracleConfig = serde_yaml::from_str(
            r#"
---
node_ip: 10.94.77.47
node_port: 9052
node_api_key: hello
base_fee: 1100000
log_level: ~
core_api_port: 9010
oracle_address: 3Wy3BaCjGDWE3bjjZkNo3aWaMz3cYrePMFhchcKovY9uG9vhpAuW
data_point_source: NanoErgXau
data_point_source_custom_script: ~
oracle_contract_parameters:
  ergo_tree_bytes: 100a040004000580dac409040004000e20193ad1f35c7dc8ac7e27dee7c2bc15e11fa9df24b2984c31e7a3a423e25c17e80402040204020402d804d601b2a5e4e3000400d602db63087201d603db6308a7d604e4c6a70407ea02d1ededed93b27202730000b2720373010093c27201c2a7e6c67201040792c172017302eb02cd7204d1ededededed938cb2db6308b2a4730300730400017305938cb27202730600018cb2720373070001918cb27202730800028cb272037309000293e4c672010407720492c17201c1a7efe6c672010561
  pool_nft_index: 5
  min_storage_rent_index: 2
  min_storage_rent: 10000000
pool_contract_parameters:
  ergo_tree_bytes: 1004040204000e20c44c61d2eaade8107e4fe9e01b1e6b6fe5c2c35e9cd9de0ffd930106b7f3c5910e20001b2069acf6bf206a3b9449c6e3966d4339be43fadad05484bddb040c37faa4d801d6018cb2db6308b2a473000073010001d1ec93720173029372017303
  refresh_nft_index: 2
  update_nft_index: 3
refresh_contract_parameters:
  ergo_tree_bytes: 1016043c040004000e20c43a3cb9a1854334a1a5daa55e38f96a2a0dc2aaefc89611e2c06a7e6c3dce6001000502010105000400040004020402040204040400040a05c8010e20193ad1f35c7dc8ac7e27dee7c2bc15e11fa9df24b2984c31e7a3a423e25c17e80400040404020408d80ed60199a37300d602b2a4730100d603b5a4d901036395e6c672030605eded928cc77203017201938cb2db6308720373020001730393e4c672030504e4c6720205047304d604b17203d605b0720386027305860273067307d901053c413d0563d803d607e4c68c7205020605d6088c720501d6098c720802860272078602ed8c720901908c72080172079a8c7209027207d6068c720502d6078c720501d608db63087202d609b27208730800d60ab2a5730900d60bdb6308720ad60cb2720b730a00d60db27208730b00d60eb2a5730c00ea02ea02ea02ea02ea02ea02ea02ea02ea02ea02ea02ea02ea02ea02ea02ea02ea02cde4c6b27203e4e30004000407d18f8cc77202017201d1927204730dd18c720601d190997207e4c6b27203730e0006059d9c72077e730f057310d1938c7209017311d193b2720b7312007209d1938c720c018c720d01d1928c720c02998c720d027e9c7204731305d193b1720bb17208d193e4c6720a04059d8c7206027e720405d193e4c6720a05049ae4c6720205047314d193c2720ac27202d192c1720ac17202d1928cc7720a0199a37315d193db6308720edb6308a7d193c2720ec2a7d192c1720ec1a7
  pool_nft_index: 17
  oracle_token_id_index: 3
  min_data_points_index: 13
  min_data_points: 2
  buffer_length_index: 21
  buffer_length: 4
  max_deviation_percent_index: 15
  max_deviation_percent: 5
  epoch_length_index: 0
  epoch_length: 30
update_contract_parameters:
  ergo_tree_bytes: 100e040004000400040204020e20193ad1f35c7dc8ac7e27dee7c2bc15e11fa9df24b2984c31e7a3a423e25c17e80400040004000e204ef9c5fa01d634eea5177eb9d5d73889a4b4a458c4024b1b646fc332c2346c270100050004000404d806d601b2a4730000d602b2db63087201730100d603b2a5730200d604db63087203d605b2a5730300d606b27204730400d1ededed938c7202017305edededed937202b2720473060093c17201c1720393c672010405c67203040593c672010504c672030504efe6c672030661edededed93db63087205db6308a793c27205c2a792c17205c1a7918cc77205018cc7a701efe6c67205046192b0b5a4d9010763d801d609db630872079591b172097307edededed938cb2720973080001730993e4c6720705048cc7a70193e4c67207060ecbc2720393e4c67207070e8c72060193e4c6720708058c720602730a730bd9010741639a8c7207018cb2db63088c720702730c00027e730d05
  pool_nft_index: 5
  ballot_token_index: 9
  min_votes_index: 13
  min_votes: 2
ballot_contract_parameters:
  ergo_tree_bytes: 10070580dac409040204020400040204000e20001b2069acf6bf206a3b9449c6e3966d4339be43fadad05484bddb040c37faa4d803d601b2a5e4e3000400d602c672010407d603e4c6a70407ea02d1ededede6720293c27201c2a793db63087201db6308a792c172017300eb02cd7203d1ededededed91b1a4730191b1db6308b2a47302007303938cb2db6308b2a473040073050001730693e47202720392c17201c1a7efe6c672010561
  min_storage_rent_index: 0
  min_storage_rent: 10000000
  update_nft_index: 6
token_ids:
  pool_nft_token_id: 193ad1f35c7dc8ac7e27dee7c2bc15e11fa9df24b2984c31e7a3a423e25c17e8
  refresh_nft_token_id: c44c61d2eaade8107e4fe9e01b1e6b6fe5c2c35e9cd9de0ffd930106b7f3c591
  update_nft_token_id: 001b2069acf6bf206a3b9449c6e3966d4339be43fadad05484bddb040c37faa4
  oracle_token_id: c43a3cb9a1854334a1a5daa55e38f96a2a0dc2aaefc89611e2c06a7e6c3dce60
  reward_token_id: e24b439a078960a48667aefbcf58c3a9b1451ac55c95940747fb3a4335a4173a
  ballot_token_id: 4ef9c5fa01d634eea5177eb9d5d73889a4b4a458c4024b1b646fc332c2346c27
rescan_height: 141887
"#).unwrap();

        let ctx = force_any_val::<ErgoStateContext>();
        let height = ctx.pre_header.height;
        let secret = force_any_val::<DlogProverInput>();
        let network_address = NetworkAddress::new(
            NetworkPrefix::Testnet,
            &Address::P2Pk(secret.public_image()),
        );
        let old_config = OracleConfig {
            oracle_address: network_address.clone(),
            ..old_config
        };
        let wallet = Wallet::from_secrets(vec![secret.clone().into()]);
        let ergo_tree = network_address.address().script().unwrap();

        let value = BASE_FEE.checked_mul_u32(10000).unwrap();
        let unspent_boxes = vec![ErgoBox::new(
            value,
            ergo_tree.clone(),
            None,
            NonMandatoryRegisters::empty(),
            height - 9,
            force_any_val::<TxId>(),
            0,
        )
        .unwrap()];
        let change_address =
            AddressEncoder::new(ergo_lib::ergotree_ir::chain::address::NetworkPrefix::Mainnet)
                .parse_address_from_str("9iHyKxXs2ZNLMp9N9gbUT9V8gTbsV7HED1C1VhttMfBUMPDyF7r")
                .unwrap();

        let state = UpdateBootstrapConfig {
            tokens_to_mint: UpdateTokensToMint {
                refresh_nft: Some(NftMintDetails {
                    name: "refresh NFT".into(),
                    description: "refresh NFT".into(),
                }),
                update_nft: Some(NftMintDetails {
                    name: "update NFT".into(),
                    description: "update NFT".into(),
                }),
                oracle_tokens: Some(TokenMintDetails {
                    name: "oracle token".into(),
                    description: "oracle token".into(),
                    quantity: 15,
                }),
                ballot_tokens: Some(TokenMintDetails {
                    name: "ballot token".into(),
                    description: "ballot token".into(),
                    quantity: 15,
                }),
                reward_tokens: Some(TokenMintDetails {
                    name: "reward token".into(),
                    description: "reward token".into(),
                    quantity: 100_000_000,
                }),
            },
            refresh_contract_parameters: Some(RefreshContractParameters::default()),
            pool_contract_parameters: Some(PoolContractParameters::default()),
            update_contract_parameters: Some(UpdateContractParameters::default()),
        };

        let height = ctx.pre_header.height;
        let submit_tx = SubmitTxMock::default();
        let prepare_update_input = PrepareUpdateInput {
            wallet: &WalletDataMock {
                unspent_boxes: unspent_boxes.clone(),
            },
            tx_signer: &mut LocalTxSigner {
                ctx: &ctx,
                wallet: &wallet,
            },
            submit_tx: &submit_tx,
            tx_fee: *BASE_FEE,
            erg_value_per_box: *BASE_FEE,
            change_address,
            height,
        };

        let prepare = PrepareUpdate::new(prepare_update_input, &old_config).unwrap();
        let new_config = prepare.execute(state).unwrap();
        assert!(new_config.token_ids != old_config.token_ids);
    }
}
