use vm::costs::cost_functions;
use vm::errors::{
    check_argument_count, check_arguments_at_least, CheckErrors, InterpreterResult as Result,
};
use vm::representations::SymbolicExpressionType::List;
use vm::representations::{SymbolicExpression, SymbolicExpressionType};
use vm::types::{TupleData, TypeSignature, Value};
use vm::{eval, Environment, LocalContext};

pub fn tuple_cons(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value> {
    //    (tuple (arg-name value)
    //           (arg-name value))
    use super::parse_eval_bindings;

    check_arguments_at_least(1, args)?;

    let bindings = parse_eval_bindings(args, env, context)?;
    runtime_cost!(cost_functions::TUPLE_CONS, env, bindings.len())?;

    TupleData::from_data(bindings).map(Value::from)
}

pub fn tuple_get(
    args: &[SymbolicExpression],
    env: &mut Environment,
    context: &LocalContext,
) -> Result<Value> {
    // (get arg-name (tuple ...))
    //    if the tuple argument is an option type, then return option(field-name).
    check_argument_count(2, args)?;

    let arg_name = args[0].match_atom().ok_or(CheckErrors::ExpectedName)?;

    let value = eval(&args[1], env, context)?;

    match value {
        Value::Optional(opt_data) => {
            match opt_data.data {
                Some(data) => {
                    if let Value::Tuple(tuple_data) = *data {
                        runtime_cost!(cost_functions::TUPLE_GET, env, tuple_data.len())?;
                        Ok(Value::some(tuple_data.get_owned(arg_name)?)
                            .expect("Tuple contents should *always* fit in a some wrapper"))
                    } else {
                        Err(CheckErrors::ExpectedTuple(TypeSignature::type_of(&data)).into())
                    }
                }
                None => Ok(Value::none()), // just pass through none-types.
            }
        }
        Value::Tuple(tuple_data) => {
            runtime_cost!(cost_functions::TUPLE_GET, env, tuple_data.len())?;
            tuple_data.get_owned(arg_name)
        }
        _ => Err(CheckErrors::ExpectedTuple(TypeSignature::type_of(&value)).into()),
    }
}

pub enum TupleDefinitionType {
    Implicit(Box<[SymbolicExpression]>),
    Explicit,
}

/// Identify whether a symbolic expression is an implicit tuple structure ((key2 k1) (key2 k2)),
/// or other - (tuple (key2 k1) (key2 k2)) / bound variable / function call.
/// The caller is responsible for any eventual type checks or actual execution.
/// Used in:
/// - the type checker: doesn't eval the resulting structure, it only type checks it,
/// - the interpreter: want to eval the result, and then do type enforcement on the value, not the type signature.
pub fn get_definition_type_of_tuple_argument(args: &SymbolicExpression) -> TupleDefinitionType {
    if let List(ref outer_expr) = args.expr {
        if let List(_) = (&outer_expr[0]).expr {
            return TupleDefinitionType::Implicit(outer_expr.clone());
        }
    }
    TupleDefinitionType::Explicit
}
