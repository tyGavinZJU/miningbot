pub mod analysis_db;
pub mod contract_interface_builder;
pub mod errors;
pub mod read_only_checker;
pub mod trait_checker;
pub mod type_checker;
pub mod types;

pub use self::types::{AnalysisPass, ContractAnalysis};
use vm::costs::LimitedCostTracker;
use vm::database::STORE_CONTRACT_SRC_INTERFACE;
use vm::representations::SymbolicExpression;
use vm::types::{QualifiedContractIdentifier, TypeSignature};

pub use self::analysis_db::AnalysisDatabase;
pub use self::errors::{CheckError, CheckErrors, CheckResult};

use self::contract_interface_builder::build_contract_interface;
use self::read_only_checker::ReadOnlyChecker;
use self::trait_checker::TraitChecker;
use self::type_checker::TypeChecker;

pub fn mem_type_check(snippet: &str) -> CheckResult<(Option<TypeSignature>, ContractAnalysis)> {
    use vm::ast::parse;
    use vm::database::MemoryBackingStore;
    let contract_identifier = QualifiedContractIdentifier::transient();
    let mut contract = parse(&contract_identifier, snippet).unwrap();
    let mut marf = MemoryBackingStore::new();
    let mut analysis_db = marf.as_analysis_db();
    type_check(
        &QualifiedContractIdentifier::transient(),
        &mut contract,
        &mut analysis_db,
        false,
    )
    .map(|x| {
        // return the first type result of the type checker
        let first_type = x
            .type_map
            .as_ref()
            .unwrap()
            .get_type(&x.expressions.last().unwrap())
            .cloned();
        (first_type, x)
    })
}

// Legacy function
// The analysis is not just checking type.
pub fn type_check(
    contract_identifier: &QualifiedContractIdentifier,
    expressions: &mut [SymbolicExpression],
    analysis_db: &mut AnalysisDatabase,
    insert_contract: bool,
) -> CheckResult<ContractAnalysis> {
    run_analysis(
        &contract_identifier,
        expressions,
        analysis_db,
        insert_contract,
        LimitedCostTracker::new_max_limit(),
    )
    .map_err(|(e, _cost_tracker)| e)
}

pub fn run_analysis(
    contract_identifier: &QualifiedContractIdentifier,
    expressions: &mut [SymbolicExpression],
    analysis_db: &mut AnalysisDatabase,
    save_contract: bool,
    cost_tracker: LimitedCostTracker,
) -> Result<ContractAnalysis, (CheckError, LimitedCostTracker)> {
    let mut contract_analysis = ContractAnalysis::new(
        contract_identifier.clone(),
        expressions.to_vec(),
        cost_tracker,
    );
    let result = analysis_db.execute(|db| {
        ReadOnlyChecker::run_pass(&mut contract_analysis, db)?;
        TypeChecker::run_pass(&mut contract_analysis, db)?;
        TraitChecker::run_pass(&mut contract_analysis, db)?;
        if STORE_CONTRACT_SRC_INTERFACE {
            let interface = build_contract_interface(&contract_analysis);
            contract_analysis.contract_interface = Some(interface);
        }
        if save_contract {
            db.insert_contract(&contract_identifier, &contract_analysis)?;
        }
        Ok(())
    });
    match result {
        Ok(_) => Ok(contract_analysis),
        Err(e) => Err((e, contract_analysis.take_contract_cost_tracker())),
    }
}

#[cfg(test)]
mod tests;
