#![allow(unused_imports)]
#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

extern crate blockstack_lib;

use blockstack_lib::address::AddressHashMode;
use blockstack_lib::burnchains::Address;
use blockstack_lib::chainstate::stacks::{
    StacksAddress, StacksPrivateKey, StacksPublicKey, StacksTransaction, StacksTransactionSigner,
    TokenTransferMemo, TransactionAuth, TransactionContractCall, TransactionPayload,
    TransactionSmartContract, TransactionSpendingCondition, TransactionVersion,
    C32_ADDRESS_VERSION_MAINNET_SINGLESIG, C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
};
use blockstack_lib::net::{Error as NetError, StacksMessageCodec};
use blockstack_lib::util::{hash::hex_bytes, hash::to_hex, log, strings::StacksString};
use blockstack_lib::vm;
use blockstack_lib::vm::{
    errors::{Error as ClarityError, RuntimeErrorType},
    types::PrincipalData,
    ClarityName, ContractName, Value,
};
use std::convert::TryFrom;
use std::io::prelude::*;
use std::io::Read;
use std::{env, fs, io};

const TESTNET_CHAIN_ID: u32 = 0x80000000;
const MAINNET_CHAIN_ID: u32 = 0x00000001;

const USAGE: &str = "blockstack-cli (options) [method] [args...]

This CLI allows you to generate simple signed transactions for blockstack-core
to process.

This CLI has these methods:

  publish          used to generate and sign a contract publish transaction
  contract-call    used to generate and sign a contract-call transaction
  generate-sk      used to generate a secret key for transaction signing
  token-transfer   used to generate and sign a transfer transaction

For usage information on those methods, call `blockstack-cli [method] -h`

`blockstack-cli` accepts flag options as well:

   --testnet       instruct the transaction generator to use a testnet version byte instead of MAINNET (default)

";

const PUBLISH_USAGE: &str = "blockstack-cli (options) publish [publisher-secret-key-hex] [fee-rate] [nonce] [contract-name] [file-name.clar]

The publish command generates and signs a contract publish transaction. If successful,
this command outputs the hex string encoding of the transaction to stdout, and exits with
code 0";

const CALL_USAGE: &str = "blockstack-cli (options) contract-call [origin-secret-key-hex] [fee-rate] [nonce] [contract-publisher-address] [contract-name] [function-name] [args...]

The contract-call command generates and signs a contract-call transaction. If successful,
this command outputs the hex string encoding of the transaction to stdout, and exits with
code 0

Arguments are supplied in one of two ways: through script evaluation or via hex encoding
of the value serialization format. The method for supplying arguments is chosen by
prefacing each argument with a flag:

  -e  indicates the argument should be _evaluated_
  -x  indicates the argument that a serialized Clarity value is being passed (hex-serialized)

e.g.,

   blockstack-cli contract-call $secret_key 10 0 SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4 foo-contract \\
      transfer-fookens -e \\'SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4 \\
                       -e \"(+ 1 2)\" \\
                       -x 0000000000000000000000000000000001 \\
                       -x 050011deadbeef11ababffff11deadbeef11ababffff
";

const TOKEN_TRANSFER_USAGE: &str = "blockstack-cli (options) token-transfer [origin-secret-key-hex] [fee-rate] [nonce] [recipient-address] [amount] [memo] [args...]

The transfer command generates and signs a STX transfer transaction. If successful,
this command outputs the hex string encoding of the transaction to stdout, and exits with
code 0";

const GENERATE_USAGE: &str = "blockstack-cli (options) generate-sk

This method generates a secret key, outputting the hex encoding of the
secret key, the corresponding public key, and the corresponding P2PKH Stacks address.";

#[derive(Debug)]
enum CliError {
    ClarityRuntimeError(RuntimeErrorType),
    ClarityGeneralError(ClarityError),
    Message(String),
    Usage,
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CliError::ClarityRuntimeError(e) => Some(e),
            CliError::ClarityGeneralError(e) => Some(e),
            _ => None,
        }
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::ClarityRuntimeError(e) => write!(f, "Clarity error: {:?}", e),
            CliError::ClarityGeneralError(e) => write!(f, "Clarity error: {}", e),
            CliError::Message(e) => write!(f, "{}", e),
            CliError::Usage => write!(f, "{}", USAGE),
        }
    }
}

impl From<&str> for CliError {
    fn from(value: &str) -> Self {
        CliError::Message(value.into())
    }
}

impl From<RuntimeErrorType> for CliError {
    fn from(value: RuntimeErrorType) -> Self {
        CliError::ClarityRuntimeError(value)
    }
}

impl From<ClarityError> for CliError {
    fn from(value: ClarityError) -> Self {
        CliError::ClarityGeneralError(value)
    }
}

impl From<NetError> for CliError {
    fn from(value: NetError) -> Self {
        CliError::Message(format!("Stacks NetError: {}", value))
    }
}

impl From<std::num::ParseIntError> for CliError {
    fn from(value: std::num::ParseIntError) -> Self {
        CliError::Message(format!("Failed to parse integer: {}", value))
    }
}

impl From<io::Error> for CliError {
    fn from(value: io::Error) -> Self {
        CliError::Message(format!("IO error reading CLI input: {}", value))
    }
}

impl From<blockstack_lib::util::HexError> for CliError {
    fn from(value: blockstack_lib::util::HexError) -> Self {
        CliError::Message(format!("Bad hex string supplied: {}", value))
    }
}

impl From<blockstack_lib::vm::types::serialization::SerializationError> for CliError {
    fn from(value: blockstack_lib::vm::types::serialization::SerializationError) -> Self {
        CliError::Message(format!("Failed to deserialize: {}", value))
    }
}

fn make_contract_publish(
    contract_name: String,
    contract_content: String,
) -> Result<TransactionSmartContract, CliError> {
    let name = ContractName::try_from(contract_name)?;
    let code_body = StacksString::from_string(&contract_content)
        .ok_or("Non-legal characters in contract-content")?;
    Ok(TransactionSmartContract { name, code_body })
}

fn make_contract_call(
    contract_address: String,
    contract_name: String,
    function_name: String,
    function_args: Vec<Value>,
) -> Result<TransactionContractCall, CliError> {
    let address =
        StacksAddress::from_string(&contract_address).ok_or("Failed to parse contract address")?;
    let contract_name = ContractName::try_from(contract_name)?;
    let function_name = ClarityName::try_from(function_name)?;

    Ok(TransactionContractCall {
        address,
        contract_name,
        function_name,
        function_args,
    })
}

fn make_standard_single_sig_tx(
    version: TransactionVersion,
    chain_id: u32,
    payload: TransactionPayload,
    publicKey: &StacksPublicKey,
    nonce: u64,
    fee_rate: u64,
) -> StacksTransaction {
    let mut spending_condition =
        TransactionSpendingCondition::new_singlesig_p2pkh(publicKey.clone())
            .expect("Failed to create p2pkh spending condition from public key.");
    spending_condition.set_nonce(nonce);
    spending_condition.set_fee_rate(fee_rate);
    let auth = TransactionAuth::Standard(spending_condition);
    let mut tx = StacksTransaction::new(version, auth, payload);
    tx.chain_id = chain_id;
    tx
}

fn sign_transaction_single_sig_standard(
    transaction: &str,
    secret_key: &StacksPrivateKey,
) -> Result<StacksTransaction, CliError> {
    let transaction =
        StacksTransaction::consensus_deserialize(&mut io::Cursor::new(&hex_bytes(transaction)?))?;

    let mut tx_signer = StacksTransactionSigner::new(&transaction);
    tx_signer.sign_origin(secret_key)?;

    Ok(tx_signer
        .get_tx()
        .ok_or("TX did not finish signing -- was this a standard single signature transaction?")?)
}

fn handle_contract_publish(
    args: &[String],
    version: TransactionVersion,
    chain_id: u32,
) -> Result<String, CliError> {
    if args.len() >= 1 && args[0] == "-h" {
        return Err(CliError::Message(format!("USAGE:\n {}", PUBLISH_USAGE)));
    }
    if args.len() != 5 {
        return Err(CliError::Message(format!(
            "Incorrect argument count supplied \n\nUSAGE:\n {}",
            PUBLISH_USAGE
        )));
    }
    let sk_publisher = &args[0];
    let fee_rate = args[1].parse()?;
    let nonce = args[2].parse()?;
    let contract_name = &args[3];
    let contract_file = &args[4];

    let contract_contents = if contract_file == "-" {
        let mut buffer = String::new();
        io::stdin().read_to_string(&mut buffer)?;
        buffer
    } else {
        fs::read_to_string(contract_file)?
    };

    let sk_publisher = StacksPrivateKey::from_hex(sk_publisher)?;

    let payload = make_contract_publish(contract_name.clone(), contract_contents)?;
    let unsigned_tx = make_standard_single_sig_tx(
        version,
        chain_id,
        payload.into(),
        &StacksPublicKey::from_private(&sk_publisher),
        nonce,
        fee_rate,
    );
    let mut unsigned_tx_bytes = vec![];
    unsigned_tx
        .consensus_serialize(&mut unsigned_tx_bytes)
        .expect("FATAL: invalid transaction");
    let signed_tx =
        sign_transaction_single_sig_standard(&to_hex(&unsigned_tx_bytes), &sk_publisher)?;

    let mut signed_tx_bytes = vec![];
    signed_tx
        .consensus_serialize(&mut signed_tx_bytes)
        .expect("FATAL: invalid signed transaction");
    Ok(to_hex(&signed_tx_bytes))
}

fn handle_contract_call(
    args: &[String],
    version: TransactionVersion,
    chain_id: u32,
) -> Result<String, CliError> {
    if args.len() >= 1 && args[0] == "-h" {
        return Err(CliError::Message(format!("USAGE:\n {}", CALL_USAGE)));
    }
    if args.len() < 6 {
        return Err(CliError::Message(format!(
            "Incorrect argument count supplied \n\nUSAGE:\n {}",
            CALL_USAGE
        )));
    }
    let sk_origin = &args[0];
    let fee_rate = args[1].parse()?;
    let nonce = args[2].parse()?;
    let contract_address = &args[3];
    let contract_name = &args[4];
    let function_name = &args[5];

    let val_args = &args[6..];

    if val_args.len() % 2 != 0 {
        return Err(
            "contract-call arguments must be supplied as a list of `-e ...` or `-x 0000...` pairs"
                .into(),
        );
    }

    let mut arg_iterator = 0;
    let mut values = Vec::new();
    while arg_iterator < val_args.len() {
        let eval_method = &val_args[arg_iterator];
        let input = &val_args[arg_iterator + 1];
        let value = match eval_method.as_str() {
            "-x" => {
                Value::try_deserialize_hex_untyped(input)?
            },
            "-e" => {
                vm::execute(input)?
                    .ok_or("Supplied argument did not evaluate to a Value")?
            },
            _ => {
                return Err("contract-call arguments must be supplied as a list of `-e ...` or `-x 0000...` pairs".into())
            }
        };

        values.push(value);
        arg_iterator += 2;
    }

    let sk_origin = StacksPrivateKey::from_hex(sk_origin)?;

    let payload = make_contract_call(
        contract_address.clone(),
        contract_name.clone(),
        function_name.clone(),
        values,
    )?;
    let unsigned_tx = make_standard_single_sig_tx(
        version,
        chain_id,
        payload.into(),
        &StacksPublicKey::from_private(&sk_origin),
        nonce,
        fee_rate,
    );

    let mut unsigned_tx_bytes = vec![];
    unsigned_tx
        .consensus_serialize(&mut unsigned_tx_bytes)
        .expect("FATAL: invalid transaction");
    let signed_tx = sign_transaction_single_sig_standard(&to_hex(&unsigned_tx_bytes), &sk_origin)?;

    let mut signed_tx_bytes = vec![];
    signed_tx
        .consensus_serialize(&mut signed_tx_bytes)
        .expect("FATAL: invalid signed transaction");
    Ok(to_hex(&signed_tx_bytes))
}

fn handle_token_transfer(
    args: &[String],
    version: TransactionVersion,
    chain_id: u32,
) -> Result<String, CliError> {
    if args.len() >= 1 && args[0] == "-h" {
        return Err(CliError::Message(format!(
            "USAGE:\n {}",
            TOKEN_TRANSFER_USAGE
        )));
    }
    if args.len() < 5 {
        return Err(CliError::Message(format!(
            "Incorrect argument count supplied \n\nUSAGE:\n {}",
            TOKEN_TRANSFER_USAGE
        )));
    }
    let sk_origin = StacksPrivateKey::from_hex(&args[0])?;
    let fee_rate = args[1].parse()?;
    let nonce = args[2].parse()?;
    let recipient_address =
        PrincipalData::parse(&args[3]).map_err(|_e| "Failed to parse recipient")?;
    let amount = &args[4].parse()?;
    let memo = {
        let mut memo = [0; 34];
        let mut bytes = if args.len() == 6 {
            args[5].as_bytes().to_vec()
        } else {
            vec![]
        };
        bytes.resize(34, 0);
        memo.copy_from_slice(&bytes);
        TokenTransferMemo(memo)
    };

    let payload = TransactionPayload::TokenTransfer(recipient_address, *amount, memo);
    let unsigned_tx = make_standard_single_sig_tx(
        version,
        chain_id,
        payload,
        &StacksPublicKey::from_private(&sk_origin),
        nonce,
        fee_rate,
    );
    let mut unsigned_tx_bytes = vec![];
    unsigned_tx
        .consensus_serialize(&mut unsigned_tx_bytes)
        .expect("FATAL: invalid transaction");
    let signed_tx = sign_transaction_single_sig_standard(&to_hex(&unsigned_tx_bytes), &sk_origin)?;

    let mut signed_tx_bytes = vec![];
    signed_tx
        .consensus_serialize(&mut signed_tx_bytes)
        .expect("FATAL: invalid signed transaction");
    Ok(to_hex(&signed_tx_bytes))
}

fn generate_secret_key(args: &[String], version: TransactionVersion) -> Result<String, CliError> {
    if args.len() >= 1 && args[0] == "-h" {
        return Err(CliError::Message(format!("USAGE:\n {}", GENERATE_USAGE)));
    }

    let sk = StacksPrivateKey::new();
    let pk = StacksPublicKey::from_private(&sk);
    let version = match version {
        TransactionVersion::Mainnet => C32_ADDRESS_VERSION_MAINNET_SINGLESIG,
        TransactionVersion::Testnet => C32_ADDRESS_VERSION_TESTNET_SINGLESIG,
    };

    let address = StacksAddress::from_public_keys(
        version,
        &AddressHashMode::SerializeP2PKH,
        1,
        &vec![pk.clone()],
    )
    .expect("Failed to generate address from public key");
    Ok(format!(
        "{{ 
  \"secretKey\": \"{}\",
  \"publicKey\": \"{}\",
  \"stacksAddress\": \"{}\"
}}",
        sk.to_hex(),
        pk.to_hex(),
        address.to_string()
    ))
}

fn main() {
    log::set_loglevel(log::LOG_DEBUG).unwrap();
    let mut argv: Vec<String> = env::args().collect();

    argv.remove(0);

    match main_handler(argv) {
        Ok(s) => {
            println!("{}", s);
        }
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}

fn main_handler(mut argv: Vec<String>) -> Result<String, CliError> {
    let tx_version = if let Some(ix) = argv.iter().position(|x| x == "--testnet") {
        argv.remove(ix);
        TransactionVersion::Testnet
    } else {
        TransactionVersion::Mainnet
    };

    let chain_id = if tx_version == TransactionVersion::Testnet {
        TESTNET_CHAIN_ID
    } else {
        MAINNET_CHAIN_ID
    };

    if let Some((method, args)) = argv.split_first() {
        match method.as_str() {
            "contract-call" => handle_contract_call(args, tx_version, chain_id),
            "publish" => handle_contract_publish(args, tx_version, chain_id),
            "token-transfer" => handle_token_transfer(args, tx_version, chain_id),
            "generate-sk" => generate_secret_key(args, tx_version),
            _ => Err(CliError::Usage),
        }
    } else {
        Err(CliError::Usage)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    #[test]
    fn generate_should_work() {
        assert!(main_handler(vec!["generate-sk".into(), "--testnet".into()]).is_ok());
        assert!(main_handler(vec!["generate-sk".into()]).is_ok());
        assert!(generate_secret_key(&vec!["-h".into()], TransactionVersion::Mainnet).is_err());
    }

    fn to_string_vec(x: &[&str]) -> Vec<String> {
        x.iter().map(|&x| x.into()).collect()
    }

    #[test]
    fn simple_publish() {
        let publish_args = [
            "publish",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "foo-contract",
            "./sample-contracts/tokens.clar",
        ];

        assert!(main_handler(to_string_vec(&publish_args)).is_ok());

        let publish_args = [
            "publish",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "foo-contract",
            "./sample-contracts/non-existent-tokens.clar",
        ];

        assert!(format!(
            "{}",
            main_handler(to_string_vec(&publish_args)).unwrap_err()
        )
        .contains("IO error"));
    }

    #[test]
    fn simple_token_transfer() {
        let tt_args = [
            "token-transfer",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "ST1A14RBKJ289E3DP89QAZE2RRHDPWP5RHMYFRCHV",
            "10",
        ];

        assert!(main_handler(to_string_vec(&tt_args)).is_ok());

        let tt_args = [
            "token-transfer",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "ST1A14RBKJ289E3DP89QAZE2RRHDPWP5RHMYFRCHV",
            "10",
            "Memo",
        ];

        assert!(main_handler(to_string_vec(&tt_args)).is_ok());

        let tt_args = [
            "token-transfer",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "ST1A14RBKJ289E3DP89QAZE2RRHDPWP5RHMYFRCHV",
            "-1",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&tt_args)).unwrap_err())
                .contains("Failed to parse integer")
        );

        let tt_args = [
            "token-transfer",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SX1A14RBKJ289E3DP89QAZE2RRHDPWP5RHMYFRCHV",
            "10",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&tt_args)).unwrap_err())
                .contains("Failed to parse recipient")
        );
    }

    #[test]
    fn simple_cc() {
        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-e",
            "(+ 1 0)",
            "-e",
            "2",
        ];

        let exec_1 = main_handler(to_string_vec(&cc_args)).unwrap();

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-e",
            "(+ 0 1)",
            "-e",
            "(+ 1 1)",
        ];

        let exec_2 = main_handler(to_string_vec(&cc_args)).unwrap();

        assert_eq!(exec_1, exec_2);

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-x",
            "0000000000000000000000000000000001",
            "-x",
            "0000000000000000000000000000000002",
        ];

        let exec_3 = main_handler(to_string_vec(&cc_args)).unwrap();

        assert_eq!(exec_2, exec_3);

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-e",
            "(+ 0 1)",
            "-e",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&cc_args)).unwrap_err())
                .contains("arguments must be supplied as")
        );

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-e",
            "(/ 1 0)",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&cc_args)).unwrap_err())
                .contains("Clarity error")
        );

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "quryey",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-e",
            "1",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&cc_args)).unwrap_err())
                .contains("parse integer")
        );

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000fz",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-e",
            "1",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&cc_args)).unwrap_err())
                .contains("Failed to decode hex")
        );

        let sk = StacksPrivateKey::new();
        let s = format!(
            "{}",
            sign_transaction_single_sig_standard("01zz", &sk).unwrap_err()
        );
        println!("{}", s);
        assert!(s.contains("Bad hex string"));

        let cc_args = [
            "contract-call",
            "043ff5004e3d695060fa48ac94c96049b8c14ef441c50a184a6a3875d2a000f3",
            "1",
            "0",
            "SPJT598WY1RJN792HRKRHRQYFB7RJ5ZCG6J6GEZ4",
            "foo-contract",
            "transfer-fookens",
            "-x",
            "1010",
        ];

        assert!(
            format!("{}", main_handler(to_string_vec(&cc_args)).unwrap_err())
                .contains("deserialize")
        );
    }
}
