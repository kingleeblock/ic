use clap::Parser;
use ic_crypto::threshold_sig_public_key_to_der;
use ic_error_types::UserError;
use ic_ic00_types::{CanisterIdRecord, CanisterInstallMode, InstallCodeArgs};
use ic_state_machine_tests::StateMachine;
use ic_types::ingress::WasmResult;
use ic_types::{CanisterId, PrincipalId};
use serde::{Deserialize, Serialize};
use std::io::{stdin, stdout, Read, Write};
use std::time::Duration;
use Request::*;

macro_rules! debug_print {
    ($opts:expr, $msg:expr $(,$args:expr)* $(,)*) => {
        if $opts.debug {
            eprintln!($msg $(,$args)*);
        }
    }
}

#[derive(Deserialize)]
enum Request {
    RootKey,
    Time,
    AdvanceTime(Duration),
    CanisterUpdateCall(CanisterCall),
    CanisterQueryCall(CanisterCall),
    CanisterExists(RawCanisterId),
    CyclesBalance(RawCanisterId),
    AddCycles(AddCyclesArg),
    SetStableMemory(SetStableMemoryArg),
    ReadStableMemory(RawCanisterId),
}

#[derive(Deserialize)]
struct AddCyclesArg {
    // raw bytes of the principal
    canister_id: Vec<u8>,
    amount: u128,
}

#[derive(Deserialize)]
struct SetStableMemoryArg {
    // raw bytes of the principal
    canister_id: Vec<u8>,
    data: Vec<u8>,
}

#[derive(Deserialize)]
struct RawCanisterId {
    // raw bytes of the principal
    canister_id: Vec<u8>,
}

impl From<RawCanisterId> for CanisterId {
    fn from(raw_id: RawCanisterId) -> Self {
        CanisterId::try_from(raw_id.canister_id).expect("invalid canister id")
    }
}

#[derive(Deserialize)]
struct CanisterCall {
    sender: Vec<u8>,
    canister_id: Vec<u8>,
    method: String,
    arg: Vec<u8>,
}

struct ParsedCanisterCall {
    sender: PrincipalId,
    canister_id: CanisterId,
    method: String,
    arg: Vec<u8>,
}

impl From<CanisterCall> for ParsedCanisterCall {
    fn from(call: CanisterCall) -> Self {
        ParsedCanisterCall {
            sender: PrincipalId::try_from(&call.sender).unwrap_or_else(|err| {
                panic!(
                    "failed to parse sender from bytes {}: {}",
                    hex::encode(&call.sender),
                    err
                )
            }),
            canister_id: CanisterId::try_from(&call.canister_id).unwrap_or_else(|err| {
                panic!(
                    "failed to parse canister id from bytes {}: {}",
                    hex::encode(&call.canister_id),
                    err
                )
            }),
            method: call.method,
            arg: call.arg,
        }
    }
}

/// Command-line options
#[derive(Parser)]
#[clap(version = "1.0")]
struct Opts {
    /// Prints additional debug information to stderr (to not interfere with data sent over stdin/stdout).
    #[clap(short, long)]
    debug: bool,
}

fn main() {
    let opts: Opts = Opts::parse();
    let env = StateMachine::new();
    loop {
        debug_print!(&opts, "enter request loop");
        let size =
            u64::from_le_bytes(TryFrom::try_from(read_bytes(8)).expect("failed to read data size"))
                as usize;
        debug_print!(&opts, "data size: {}", size);
        let payload = read_bytes(size);
        debug_print!(&opts, "payload received: {:?}", hex::encode(&payload));
        let data: Request = ciborium::from_reader(&payload[..]).unwrap();
        match data {
            RootKey => send_response(
                threshold_sig_public_key_to_der(env.root_key()).unwrap(),
                &opts,
            ),
            Time => send_response(env.time(), &opts),
            AdvanceTime(amount) => {
                env.advance_time(amount);
                send_response((), &opts);
            }
            CanisterUpdateCall(call) => {
                let call = ParsedCanisterCall::from(call);
                if call.canister_id == CanisterId::ic_00() {
                    management_call(&env, &call, &opts);
                } else {
                    let result = env.execute_ingress_as(
                        call.sender,
                        call.canister_id,
                        call.method,
                        call.arg,
                    );
                    send_response(result, &opts);
                }
            }
            CanisterQueryCall(call) => {
                let call = ParsedCanisterCall::from(call);
                let result = env.query_as(call.sender, call.canister_id, call.method, call.arg);
                send_response(result, &opts);
            }
            CanisterExists(canister_id) => {
                send_response(env.canister_exists(CanisterId::from(canister_id)), &opts)
            }
            SetStableMemory(arg) => {
                let canister_id =
                    CanisterId::try_from(arg.canister_id).expect("invalid canister id");
                env.set_stable_memory(canister_id, &arg.data);
                send_response((), &opts);
            }
            ReadStableMemory(canister_id) => {
                send_response(env.stable_memory(CanisterId::from(canister_id)), &opts);
            }
            CyclesBalance(canister_id) => {
                send_response(env.cycle_balance(CanisterId::from(canister_id)), &opts)
            }
            AddCycles(arg) => send_response(
                env.add_cycles(
                    CanisterId::try_from(arg.canister_id).expect("invalid canister id"),
                    arg.amount,
                ),
                &opts,
            ),
        }
    }
}

fn management_call(env: &StateMachine, call: &ParsedCanisterCall, opts: &Opts) {
    match call.method.as_str() {
        "create_canister" => {
            let settings = candid::decode_one(&call.arg)
                .expect("failed to decode candid argument for 'create_canister'");
            let id = env.create_canister(settings);
            let result = candid::encode_one(CanisterIdRecord::from(id)).unwrap();
            send_response(Ok::<WasmResult, UserError>(WasmResult::Reply(result)), opts);
        }
        "install_code" => {
            let settings: InstallCodeArgs = candid::decode_one(&call.arg)
                .expect("failed to decode candid argument for 'create_canister'");
            let canister_id =
                CanisterId::try_from(settings.canister_id).expect("invalid canister id");
            match settings.mode {
                CanisterInstallMode::Install => {
                    env.install_existing_canister(canister_id, settings.wasm_module, settings.arg)
                }
                CanisterInstallMode::Reinstall => {
                    env.reinstall_canister(canister_id, settings.wasm_module, settings.arg)
                }
                CanisterInstallMode::Upgrade => {
                    env.upgrade_canister(canister_id, settings.wasm_module, settings.arg)
                }
            }
            .expect("failed to install canister code");
            send_response(
                Ok::<WasmResult, UserError>(WasmResult::Reply(candid::encode_one(()).unwrap())),
                opts,
            );
        }
        other => {
            panic!("unsupported management canister call: {}", other)
        }
    }
}

fn read_bytes(num_bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; num_bytes];
    stdin()
        .read_exact(&mut buf)
        .expect("failed to read from stdin");
    buf
}

fn send_response<R: Serialize>(response: R, opts: &Opts) {
    let cbor = into_cbor(&response);
    let length_bytes = (cbor.len() as u64).to_le_bytes();
    stdout()
        .write_all(&length_bytes)
        .expect("failed to send response length");
    debug_print!(opts, "length sent: {:?}", hex::encode(length_bytes));

    stdout()
        .write_all(cbor.as_slice())
        .expect("failed to send response");
    stdout().flush().expect("failed to flush stdout");

    debug_print!(opts, "payload sent: {:?}", hex::encode(cbor));
}

fn into_cbor<R: Serialize>(value: &R) -> Vec<u8> {
    let mut bytes = vec![];
    ciborium::ser::into_writer(&value, &mut bytes).expect("bug: failed to encode a block");
    bytes
}
