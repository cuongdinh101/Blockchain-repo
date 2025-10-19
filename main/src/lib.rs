#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short,
    Address, BytesN, Env, String,
};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Error {
    Unauthorized = 1,
    NotFound = 2,
    BadState = 3,
    EscrowNotFunded = 4,
    AlreadySettled = 5,
}

#[contracttype]
#[derive(Clone)]
pub enum Status {
    Draft,
    Active,
    InTransit,
    Delivered,
    Settled,
}

#[contracttype]
#[derive(Clone)]
pub struct FreightContract {
    pub id: u128,
    pub shipper: Address,
    pub carrier: Address,
    pub origin: String,
    pub destination: String,
    pub token: Address,
    pub price: i128,
    pub deadline_unix: u64,
    pub doc_hash: BytesN<32>,
    pub status: Status,
    pub created_at: u64,
    pub escrow_funded: bool,
    pub total_secs: u64,
    pub total_km: u32,
    pub computed_cost: i128,
    pub last_paid: i128,
}

#[contracttype]
enum DataKey {
    NextId,
    Contract(u128),
}

fn put<T: soroban_sdk::IntoVal<Env, soroban_sdk::Val>>(e: &Env, k: &DataKey, v: &T) {
    e.storage().instance().set(k, v);
    e.storage().instance().extend_ttl(50, 200);
}
fn get<T: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>>(e: &Env, k: &DataKey) -> Option<T> {
    e.storage().instance().get(k)
}

#[contract]
pub struct RoadFreight;

#[contractimpl]
impl RoadFreight {
    fn next_id(e: &Env) -> u128 {
        let mut id: u128 = e.storage().instance().get(&DataKey::NextId).unwrap_or(0_u128);
        id += 1;
        put(e, &DataKey::NextId, &id);
        id
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_contract(
        e: Env,
        shipper: Address,
        carrier: Address,
        origin: String,
        destination: String,
        token: Address,
        price: i128,
        deadline_unix: u64,
        doc_hash: BytesN<32>,
    ) -> Result<u128, Error> {
        shipper.require_auth();

        let id = Self::next_id(&e);
        let fc = FreightContract {
            id,
            shipper: shipper.clone(),
            carrier: carrier.clone(),
            origin,
            destination,
            token,
            price,
            deadline_unix,
            doc_hash,
            status: Status::Draft,
            created_at: e.ledger().timestamp(),
            escrow_funded: false,
            total_secs: 0,
            total_km: 0,
            computed_cost: 0,
            last_paid: 0,
        };
        put(&e, &DataKey::Contract(id), &fc);

        e.events().publish((symbol_short!("EV"), symbol_short!("CREATED")), id);
        Ok(id)
    }

    pub fn accept(e: Env, id: u128, carrier: Address) -> Result<(), Error> {
        carrier.require_auth();
        let mut fc: FreightContract = get(&e, &DataKey::Contract(id)).ok_or(Error::NotFound)?;
        if fc.carrier != carrier { return Err(Error::Unauthorized); }
        if !matches!(fc.status, Status::Draft) { return Err(Error::BadState); }

        fc.status = Status::Active;
        put(&e, &DataKey::Contract(id), &fc);
        e.events().publish((symbol_short!("EV"), symbol_short!("ACCEPTED")), id);
        Ok(())
    }

    pub fn mark_funded(e: Env, id: u128, shipper: Address) -> Result<(), Error> {
        shipper.require_auth();
        let mut fc: FreightContract = get(&e, &DataKey::Contract(id)).ok_or(Error::NotFound)?;
        if fc.shipper != shipper { return Err(Error::Unauthorized); }
        if !matches!(fc.status, Status::Active) { return Err(Error::BadState); }

        fc.escrow_funded = true;
        put(&e, &DataKey::Contract(id), &fc);
        e.events().publish((symbol_short!("EV"), symbol_short!("FUNDED")), id);
        Ok(())
    }

    pub fn start_trip(e: Env, id: u128, caller: Address) -> Result<(), Error> {
        caller.require_auth();
        let mut fc: FreightContract = get(&e, &DataKey::Contract(id)).ok_or(Error::NotFound)?;
        if !(caller == fc.shipper || caller == fc.carrier) { return Err(Error::Unauthorized); }
        if !matches!(fc.status, Status::Active) { return Err(Error::BadState); }
        if !fc.escrow_funded { return Err(Error::EscrowNotFunded); }

        fc.status = Status::InTransit;
        put(&e, &DataKey::Contract(id), &fc);
        e.events().publish((symbol_short!("EV"), symbol_short!("STARTED")), id);
        Ok(())
    }

    pub fn log_telemetry(
        e: Env,
        id: u128,
        add_secs: u32,
        add_km: u32,
        add_cost: i128,
        oracle: Address,
    ) -> Result<(), Error> {
        oracle.require_auth();
        let mut fc: FreightContract = get(&e, &DataKey::Contract(id)).ok_or(Error::NotFound)?;
        if !matches!(fc.status, Status::InTransit) { return Err(Error::BadState); }

        fc.total_secs = fc.total_secs.saturating_add(add_secs as u64);
        fc.total_km   = fc.total_km.saturating_add(add_km);
        fc.computed_cost = fc.computed_cost.saturating_add(add_cost);
        put(&e, &DataKey::Contract(id), &fc);

        e.events().publish((symbol_short!("EV"), symbol_short!("TEL")), (id, add_secs, add_km));
        Ok(())
    }

    pub fn submit_pod(e: Env, id: u128, pod_hash: BytesN<32>, caller: Address) -> Result<(), Error> {
        caller.require_auth();
        let mut fc: FreightContract = get(&e, &DataKey::Contract(id)).ok_or(Error::NotFound)?;
        if !(caller == fc.shipper || caller == fc.carrier) { return Err(Error::Unauthorized); }
        if !matches!(fc.status, Status::InTransit) { return Err(Error::BadState); }

        fc.doc_hash = pod_hash.clone();
        fc.status = Status::Delivered;
        put(&e, &DataKey::Contract(id), &fc);
        e.events().publish((symbol_short!("EV"), symbol_short!("DELIVERED")), id);
        Ok(())
    }

    pub fn evaluate_and_settle(e: Env, id: u128, invoker: Address) -> Result<i128, Error> {
        invoker.require_auth();
        let mut fc: FreightContract = get(&e, &DataKey::Contract(id)).ok_or(Error::NotFound)?;
        if !matches!(fc.status, Status::Delivered) { return Err(Error::BadState); }
        if !fc.escrow_funded { return Err(Error::EscrowNotFunded); }

        let now = e.ledger().timestamp();
        let pay = if now <= fc.deadline_unix { fc.price } else { fc.price / 2 };

        fc.status = Status::Settled;
        fc.last_paid = pay;
        put(&e, &DataKey::Contract(id), &fc);

        e.events().publish((symbol_short!("EV"), symbol_short!("SETTLED")), (id, pay));
        Ok(pay)
    }

    pub fn get_contract(e: Env, id: u128) -> Result<FreightContract, Error> {
        get(&e, &DataKey::Contract(id)).ok_or(Error::NotFound)
    }
}
