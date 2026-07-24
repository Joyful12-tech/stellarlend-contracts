//! Tests asserting that `flash_loan` and `repay_flash_loan` emit the correct
//! schema-versioned events (`FlashLoanEventV1` / `FlashLoanRepaidEventV1`).

#![cfg(test)]

use crate::{
    DataKey, FlashLoanEventV1, FlashLoanRepaidEventV1, LendingContract, LendingContractClient,
};
use soroban_sdk::{
    contract, contractimpl,
    events::Event,
    testutils::{Address as _, Events},
    Address, Bytes, Env,
};

// ---------------------------------------------------------------------------
// Minimal flash-loan receiver that repays via `repay_flash_loan` so that
// `FlashLoanRepaidEventV1` is also emitted during the callback.
// ---------------------------------------------------------------------------

#[contract]
pub struct RepayingReceiver;

#[contractimpl]
impl RepayingReceiver {
    /// Store the lending contract address so `on_flash_loan` can call back.
    pub fn set_lending(env: Env, lending: Address) {
        env.storage()
            .instance()
            .set(&soroban_sdk::Symbol::new(&env, "lending"), &lending);
    }

    pub fn on_flash_loan(
        env: Env,
        _initiator: Address,
        asset: Address,
        amount: i128,
        fee: i128,
        _params: Bytes,
    ) {
        // Credit this receiver's balance with amount + fee so that
        // repay_flash_loan can debit it.
        let bal_key = DataKey::Balance(asset.clone(), env.current_contract_address());
        let cur_bal: i128 = env.storage().persistent().get(&bal_key).unwrap_or(0);
        let total = amount.checked_add(fee).expect("overflow");
        env.storage()
            .persistent()
            .set(&bal_key, &(cur_bal.checked_add(total).expect("overflow")));

        // Call repay_flash_loan on the lending contract.
        let lending: Address = env
            .storage()
            .instance()
            .get(&soroban_sdk::Symbol::new(&env, "lending"))
            .expect("lending not set");

        let client = LendingContractClient::new(&env, &lending);
        client.repay_flash_loan(&env.current_contract_address(), &asset, &total);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn setup() -> (
    Env,
    LendingContractClient<'static>,
    Address, // lending contract id
    Address, // receiver contract id
    Address, // asset
    Address, // initiator
) {
    let env = Env::default();
    env.mock_all_auths();

    let lending_id = env.register(LendingContract, ());
    let lending_client = LendingContractClient::new(&env, &lending_id);
    let receiver_id = env.register(RepayingReceiver, ());
    let receiver_client = RepayingReceiverClient::new(&env, &receiver_id);

    let admin = Address::generate(&env);
    let asset = Address::generate(&env);
    let initiator = Address::generate(&env);

    lending_client.initialize(&admin);
    // Zero flash fee for simpler fee calculations in the basic test.
    lending_client.set_flash_fee(&0);

    // Seed treasury with enough liquidity.
    env.as_contract(&lending_id, || {
        env.storage()
            .persistent()
            .set(&DataKey::Treasury(asset.clone()), &10_000i128);
    });

    // Tell the receiver which contract to repay.
    receiver_client.set_lending(&lending_id);

    (env, lending_client, lending_id, receiver_id, asset, initiator)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A successful flash loan must emit a `FlashLoanEventV1` with the correct fields.
#[test]
fn flash_loan_emits_event_with_correct_fields() {
    let (env, client, lending_id, receiver_id, asset, initiator) = setup();

    client.flash_loan(
        &initiator,
        &receiver_id,
        &asset,
        &1_000i128,
        &Bytes::new(&env),
    );

    let expected = FlashLoanEventV1 {
        schema_version: 1,
        initiator: initiator.clone(),
        receiver: receiver_id.clone(),
        asset: asset.clone(),
        amount: 1_000,
        fee: 0,
    }
    .to_xdr(&env, &lending_id);

    let all = env.events().all();
    assert!(
        all.events().contains(&expected),
        "FlashLoanEventV1 not found in events: {:?}",
        all
    );
}

/// `repay_flash_loan` must emit `FlashLoanRepaidEventV1`.
/// We call it directly (outside a flash-loan callback) by seeding the payer's
/// balance and the treasury.
#[test]
fn repay_flash_loan_emits_event_with_correct_fields() {
    let env = Env::default();
    env.mock_all_auths();

    let lending_id = env.register(LendingContract, ());
    let client = LendingContractClient::new(&env, &lending_id);

    let admin = Address::generate(&env);
    let payer = Address::generate(&env);
    let asset = Address::generate(&env);

    client.initialize(&admin);

    // Seed payer balance and treasury.
    env.as_contract(&lending_id, || {
        env.storage()
            .persistent()
            .set(&DataKey::Balance(asset.clone(), payer.clone()), &500i128);
        env.storage()
            .persistent()
            .set(&DataKey::Treasury(asset.clone()), &0i128);
    });

    client.repay_flash_loan(&payer, &asset, &500i128);

    let expected = FlashLoanRepaidEventV1 {
        schema_version: 1,
        payer: payer.clone(),
        asset: asset.clone(),
        amount: 500,
    }
    .to_xdr(&env, &lending_id);

    let all = env.events().all();
    assert!(
        all.events().contains(&expected),
        "FlashLoanRepaidEventV1 not found in events: {:?}",
        all
    );
}

/// End-to-end: a flash loan with a non-zero fee emits `FlashLoanEventV1` with
/// the correct `fee` field, and the receiver's `repay_flash_loan` call emits
/// `FlashLoanRepaidEventV1` with `amount == principal + fee`.
#[test]
fn flash_loan_with_fee_emits_correct_fee_in_event() {
    let env = Env::default();
    env.mock_all_auths();

    let lending_id = env.register(LendingContract, ());
    let client = LendingContractClient::new(&env, &lending_id);
    let receiver_id = env.register(RepayingReceiver, ());
    let receiver_client = RepayingReceiverClient::new(&env, &receiver_id);

    let admin = Address::generate(&env);
    let asset = Address::generate(&env);
    let initiator = Address::generate(&env);

    client.initialize(&admin);
    // 100 bps = 1% fee
    client.set_flash_fee(&100);

    env.as_contract(&lending_id, || {
        env.storage()
            .persistent()
            .set(&DataKey::Treasury(asset.clone()), &10_000i128);
    });

    receiver_client.set_lending(&lending_id);

    // Borrow 1_000; fee = 1_000 * 100 / 10_000 = 10.
    client.flash_loan(
        &initiator,
        &receiver_id,
        &asset,
        &1_000i128,
        &Bytes::new(&env),
    );

    let all = env.events().all();

    let expected_flash = FlashLoanEventV1 {
        schema_version: 1,
        initiator: initiator.clone(),
        receiver: receiver_id.clone(),
        asset: asset.clone(),
        amount: 1_000,
        fee: 10,
    }
    .to_xdr(&env, &lending_id);

    assert!(
        all.events().contains(&expected_flash),
        "FlashLoanEventV1 with fee not found in events: {:?}",
        all
    );

    let expected_repaid = FlashLoanRepaidEventV1 {
        schema_version: 1,
        payer: receiver_id.clone(),
        asset: asset.clone(),
        amount: 1_010, // 1_000 principal + 10 fee
    }
    .to_xdr(&env, &lending_id);

    assert!(
        all.events().contains(&expected_repaid),
        "FlashLoanRepaidEventV1 not found in events: {:?}",
        all
    );
}
