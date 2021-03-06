// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{account::Account, executor::FakeExecutor, gas_costs};
use compiled_stdlib::transaction_scripts::StdlibScript;
use libra_crypto::{ed25519::Ed25519PrivateKey, PrivateKey, Uniform};
use libra_types::{
    account_config::{self, BurnEvent, COIN1_NAME},
    transaction::{authenticator::AuthenticationKey, TransactionArgument},
    vm_status::StatusCode,
};
use move_core_types::{
    identifier::Identifier,
    language_storage::{StructTag, TypeTag},
};
use std::convert::TryFrom;
use transaction_builder::{
    encode_burn_txn_fees_script, encode_create_testing_account_script, encode_mint_script,
};

#[test]
fn burn_txn_fees() {
    let mut executor = FakeExecutor::from_genesis_file();
    let sender = Account::new();
    let tc = Account::new_blessed_tc();
    let association = Account::new_association();

    executor.execute_and_apply(association.signed_script_txn(
        encode_create_testing_account_script(
            account_config::coin1_tag(),
            *sender.address(),
            sender.auth_key_prefix(),
            false,
        ),
        1,
    ));

    executor.execute_and_apply(tc.signed_script_txn(
        encode_mint_script(account_config::coin1_tag(), sender.address(), 10_000_000),
        0,
    ));

    let gas_used = {
        let privkey = Ed25519PrivateKey::generate_for_testing();
        let pubkey = privkey.public_key();
        let new_key_hash = AuthenticationKey::ed25519(&pubkey).to_vec();
        let args = vec![TransactionArgument::U8Vector(new_key_hash)];
        let status = executor.execute_and_apply(
            sender.create_signed_txn_with_args(
                StdlibScript::RotateAuthenticationKey
                    .compiled_bytes()
                    .into_vec(),
                vec![],
                args,
                0,
                gas_costs::TXN_RESERVED,
                1,
                COIN1_NAME.to_owned(),
            ),
        );
        assert_eq!(
            status.status().vm_status().major_status,
            StatusCode::EXECUTED
        );
        status.gas_used()
    };

    let coin1_ty = TypeTag::Struct(StructTag {
        address: account_config::CORE_CODE_ADDRESS,
        module: Identifier::new("Coin1").unwrap(),
        name: Identifier::new("Coin1").unwrap(),
        type_params: vec![],
    });

    let output =
        executor.execute_and_apply(tc.signed_script_txn(encode_burn_txn_fees_script(coin1_ty), 1));

    let burn_events: Vec<_> = output
        .events()
        .iter()
        .filter_map(|event| BurnEvent::try_from(event).ok())
        .collect();

    assert_eq!(burn_events.len(), 1);
    assert!(burn_events
        .iter()
        .any(|event| event.currency_code().as_str() == "Coin1"));
    burn_events
        .iter()
        .for_each(|event| assert_eq!(event.amount(), gas_used));
}
